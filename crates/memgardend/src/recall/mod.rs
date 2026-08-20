//! Hybrid recall (CE-6): BM25 + vector arms, RRF fusion, combined scoring,
//! token budget, and the server-built `injected_text`.
//!
//! Legacy references: `engine/memory_engine.py` (the recall orchestration and
//! `_filter_by_token_budget`), `engine/search/{fusion,reranking,retrieval,tags}.py`,
//! and the fork hook's `scripts/recall.py` + `scripts/lib/content.py` for the
//! injection format.

pub mod budget;
pub mod fusion;
pub mod graph;
pub mod scoring;

use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use memgarden_core::metrics::METRICS;
use memgarden_core::types::FactType;
use memgarden_store::search::{self, CandidateRow};

use crate::error::{ApiError, join_err};
use crate::rerank;
use crate::retain::token_count;
use crate::state::AppState;
use crate::temporal::query::{Constraint, extract_constraint};
use fusion::ArmHit;

/// Queries shorter than this are not worth a round trip — the fork hook
/// short-circuits identically (`scripts/recall.py:128`, `len(prompt) < 5`).
///
/// Known limit, recorded in the design note: it is a *character* count, so a
/// dense CJK query like "메모리 회수" clears it easily but a genuinely
/// meaningful 4-char one ("RRF?") is skipped. Kept as legacy wrote it
/// because the AC-1 A/B compares injection behaviour.
pub const MIN_QUERY_CHARS: usize = 5;

/// Over-fetch, `engine/search/retrieval.py:225` (`max(limit * 5, 100)`),
/// clamped: the KNN here is brute-force rather than HNSW, so an unbounded
/// over-fetch would scan-and-sort the whole bank for nothing.
pub const MAX_OVER_FETCH: usize = 1000;

fn over_fetch(limit: usize) -> usize {
    (limit * 5).clamp(100, MAX_OVER_FETCH)
}

/// Tag matching modes, ported from `engine/search/tags.py`. The two axes are
/// independent: ANY-vs-ALL overlap, and whether an *untagged* node passes.
/// `exact` (observation scope equality) is not ported — nothing in Phase B
/// requests scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagsMatch {
    #[default]
    Any,
    All,
    AnyStrict,
    AllStrict,
}

impl TagsMatch {
    fn include_untagged(self) -> bool {
        matches!(self, TagsMatch::Any | TagsMatch::All)
    }

    fn is_any(self) -> bool {
        matches!(self, TagsMatch::Any | TagsMatch::AnyStrict)
    }

    /// `tags.py:filter_results_by_tags`. An empty request tag list means "no
    /// filtering" in every mode.
    pub fn matches(self, node_tags: &[String], request_tags: &[String]) -> bool {
        if request_tags.is_empty() {
            return true;
        }
        if node_tags.is_empty() {
            return self.include_untagged();
        }
        if self.is_any() {
            request_tags.iter().any(|t| node_tags.contains(t))
        } else {
            request_tags.iter().all(|t| node_tags.contains(t))
        }
    }
}

/// Everything the pipeline needs, already defaulted by the route.
#[derive(Debug, Clone)]
pub struct RecallParams {
    pub query: String,
    pub limit: usize,
    /// `low | mid | high`. Steers `rerank_limit` only — see `max_tokens`.
    pub budget: String,
    /// Token ceiling on the returned text (`[recall] max_tokens`, or the
    /// request's `maxTokens`).
    pub max_tokens: usize,
    pub fact_types: Vec<FactType>,
    pub tags: Vec<String>,
    pub tags_match: TagsMatch,
    pub cap_per_source: usize,
    /// `[recall] semantic_alpha`; `0.0` is legacy scoring exactly.
    pub semantic_alpha: f64,
    pub preamble: String,
    /// Injected rather than read from the clock so `injected_text` can be
    /// asserted byte-exact (Critic Revision NIT-20).
    pub now_ms: i64,
}

/// Per-result score breakdown. `temporal` is live as of CE-8 whenever the
/// query carries a constraint, `proof` as of CE-9a whenever the node is an
/// observation with more than one source fact; both report
/// `scoring::NEUTRAL` otherwise, which is the same number their stubs
/// returned.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Scores {
    #[serde(rename = "final")]
    pub final_score: f64,
    pub semantic: Option<f64>,
    pub keyword: Option<f64>,
    pub rrf: f64,
    pub recency: f64,
    pub temporal: f64,
    pub proof: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallItem {
    pub id: i64,
    pub uuid: String,
    pub text: String,
    #[serde(rename = "type")]
    pub fact_type: FactType,
    pub context: Option<String>,
    pub tags: Vec<String>,
    pub occurred_start: Option<i64>,
    pub occurred_end: Option<i64>,
    pub mentioned_at: Option<i64>,
    pub scores: Scores,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallCounts {
    /// Distinct nodes that survived filtering and entered fusion.
    pub candidates: usize,
    pub returned: usize,
    /// cl100k tokens of the returned `text` fields — what the budget counts.
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallOutcome {
    pub results: Vec<RecallItem>,
    pub injected_text: String,
    pub counts: RecallCounts,
}

impl RecallOutcome {
    fn empty() -> Self {
        RecallOutcome {
            results: vec![],
            injected_text: String::new(),
            counts: RecallCounts {
                candidates: 0,
                returned: 0,
                tokens: 0,
            },
        }
    }
}

/// Removes a candidate that another candidate already restates.
///
/// `node_sources` records, at consolidation time, that an observation was
/// built from a set of facts (CE-9a). When both ends of such a pair rank into
/// the same result set the injection carries one memory twice — measured at
/// 7.5% of injected items in the AC-1 shadow comparison.
///
/// The **higher-ranked** end survives, whichever type it is. Preferring the
/// observation unconditionally would be a claim that consolidation never
/// loses detail, which is not established; the ranking already expresses what
/// this particular query wanted.
///
/// `scored` must already be sorted best-first — the index *is* the rank.
///
// ponytail: one pass, no transitive closure. A source that is itself a
// sourced observation could in principle chain (A restates B restates C) and
// this would only collapse the pairs it sees; consolidation does not produce
// those today, and the cost of being wrong is one extra item, not a wrong
// one.
fn dedupe_restatements(
    scored: &mut Vec<(f64, RecallItem)>,
    by_id: &std::collections::HashMap<i64, CandidateRow>,
) {
    if scored.len() < 2 {
        return;
    }
    let rank: std::collections::HashMap<i64, usize> = scored
        .iter()
        .enumerate()
        .map(|(i, (_, item))| (item.id, i))
        .collect();

    let mut drop: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for (i, (_, item)) in scored.iter().enumerate() {
        let Some(row) = by_id.get(&item.id) else {
            continue;
        };
        // Decide the observation once, against *all* of its ranked sources,
        // rather than pair by pair.
        //
        // Pair-by-pair had a hole, found by review and reproduced in
        // `a_source_is_not_dropped_for_an_observation_that_is_itself_dropped`:
        // an observation ranking between two of its sources lost to the one
        // above it — correctly — and then took the one below it down as the
        // restated end of a pair whose other end no longer existed. The lower
        // source left the results with nothing standing in for it.
        //
        // The relation is one observation to many facts, so the choice is
        // one-sided: either the observation outranks every source it has and
        // stands in for all of them, or it does not and they all stay.
        let ranked_sources: Vec<usize> = row
            .sources
            .iter()
            .filter_map(|source_id| rank.get(source_id).copied())
            .collect();
        if ranked_sources.is_empty() {
            continue; // nothing of this observation's provenance ranked
        }
        if ranked_sources.iter().all(|&j| i <= j) {
            drop.extend(row.sources.iter().filter(|s| rank.contains_key(s)));
        } else {
            drop.insert(item.id);
        }
    }
    if !drop.is_empty() {
        scored.retain(|(_, item)| !drop.contains(&item.id));
    }
}

/// Two nodes holding the same text are one memory, and the injection may
/// carry it once.
///
/// Distinct from [`dedupe_restatements`], which collapses a *provenance* pair
/// — an observation and the fact it was consolidated from, related through
/// `node_sources` and rarely worded alike. This collapses plain copies: five
/// `memory_nodes` rows in the live bank hold the byte-identical text
/// `PR #3086 … is open with no reviews, comments, or check status updates`,
/// all five written in the **same millisecond** by one retain, because the
/// transcript really did contain the line five times and the extractor
/// faithfully emitted it five times. `content_hash` is per transcript
/// (`retain/mod.rs:146`), not per fact, so nothing upstream collapses them.
///
/// Measured on the query where it shows worst — the AC-1 comparison's fourth
/// losing query — the twenty injected items were **sixteen** distinct texts,
/// one of them repeated four times. Four slots of a twenty-slot budget spent
/// restating one line.
///
/// Whitespace is normalised before comparing and nothing else is: two texts
/// that differ by a word are two claims, and deciding they are one needs a
/// similarity threshold. CE-7's resolver is the standing lesson about what
/// character similarity does when trusted to judge identity, so this compares
/// only what is actually identical and leaves the near-misses alone.
///
/// `scored` must already be sorted best-first — `retain` keeps the first
/// occurrence, which is therefore the highest-ranked one.
fn dedupe_identical_text(scored: &mut Vec<(f64, RecallItem)>) {
    if scored.len() < 2 {
        return;
    }
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    scored.retain(|(_, item)| {
        seen.insert(item.text.split_whitespace().collect::<Vec<_>>().join(" "))
    });
}

/// A candidate survives if its row is loaded and passes both the type and
/// the tag filter. A free function rather than a closure because `by_id`
/// grows once the graph arm hydrates its new nodes, and a closure would hold
/// it borrowed across that.
fn keep(by_id: &std::collections::HashMap<i64, CandidateRow>, p: &RecallParams, id: i64) -> bool {
    by_id.get(&id).is_some_and(|row| {
        p.fact_types.contains(&row.fact_type) && p.tags_match.matches(&row.tags, &p.tags)
    })
}

/// Runs the whole pipeline. The caller has already verified the bank exists.
pub async fn recall(
    state: &AppState,
    bank_id: String,
    p: RecallParams,
) -> Result<RecallOutcome, ApiError> {
    // Trim first, then measure — `scripts/recall.py:126-128` strips before
    // the length gate, so a whitespace-only prompt never reaches the
    // embedder. The trimmed form is what both arms search.
    let query = p.query.trim().to_string();
    if query.chars().count() < MIN_QUERY_CHARS {
        return Ok(RecallOutcome::empty());
    }

    let fetch = over_fetch(p.limit);

    // --- Arm 0 (semantic): embed the query, then KNN. -------------------
    // The embedder is absent while it loads, when embeddings are disabled,
    // or if loading failed. Recall degrades to BM25-only rather than 503:
    // a keyword-only answer beats no answer, and the FTS arm is the one
    // that carries Korean (Phase A decision #7).
    // A poisoned lock means some other task panicked while holding it; the
    // guarded value is just an `Option<Arc<_>>` and cannot be torn, so
    // recovering beats turning one panic into every subsequent request's.
    let embedder = state
        .embedder
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let query_embedding = match embedder {
        Some(e) => {
            let query = query.clone();
            let vectors = tokio::task::spawn_blocking(move || e.embed_batch(&[query]))
                .await
                .map_err(join_err)?
                .map_err(|e| ApiError::internal(format!("query embedding failed: {e}")))?;
            vectors.into_iter().next()
        }
        None => None,
    };

    // --- Arm 3 (temporal): what, if anything, the query says about time. --
    // Pure string work, so it happens out here; only the SQL it implies goes
    // into the blocking hop below. `Unconstrainable` and "no expression" both
    // mean the same thing to the arm — no window, no query — but they are not
    // the same *decision*, which is why the third state exists upstream.
    let window = match extract_constraint(&query, p.now_ms) {
        Some(Constraint::Range { start_ms, end_ms }) => Some((start_ms, end_ms)),
        Some(Constraint::Unconstrainable) | None => None,
    };

    // --- All DB work in one blocking hop. -------------------------------
    // Three arms plus the hydrate share this hop deliberately: at CE-7 the
    // loaded p95 had 11.3ms of headroom against the 60ms gate, and each extra
    // spawn_blocking is a scheduler round trip on the hot path. The temporal
    // arm is one more statement here, not one more hop.
    let db = state.db.clone();
    let bank = bank_id.clone();
    let match_query = search::fts_query_string(&query);
    let types = p.fact_types.clone();
    let (semantic_raw, keyword_raw, temporal_raw, rows) = tokio::task::spawn_blocking(move || {
        let semantic = match &query_embedding {
            Some(emb) => search::knn(&db, &bank, emb, fetch)?,
            None => vec![],
        };
        // fact_type is filtered in SQL for the FTS arm (cheap, and it stops
        // an excluded type from consuming the LIMIT); vec0 partitions on
        // bank_id only, so the semantic arm is filtered below in Rust.
        let keyword = search::fts_candidates_filtered(&db, &bank, &match_query, &types, fetch)?;
        let temporal = match window {
            Some((lo, hi)) => search::temporal_candidates(&db, &bank, lo, hi, fetch)?,
            None => vec![],
        };

        let mut ids: Vec<i64> = semantic.iter().map(|(id, _)| *id).collect();
        ids.extend(keyword.iter().map(|(id, _)| *id));
        ids.extend(temporal.iter().copied());
        ids.sort_unstable();
        ids.dedup();
        let rows = search::hydrate(&db, &bank, &ids)?;
        Ok::<_, memgarden_core::Error>((semantic, keyword, temporal, rows))
    })
    .await
    .map_err(join_err)??;

    // --- Filter both arms through one tag/type implementation. ----------
    let mut by_id: std::collections::HashMap<i64, CandidateRow> =
        rows.into_iter().map(|r| (r.id, r)).collect();

    let semantic: Vec<ArmHit> = semantic_raw
        .into_iter()
        .filter(|(id, _)| keep(&by_id, &p, *id))
        // vec0's cosine `distance` is `1 - cosine_similarity`.
        .map(|(id, distance)| ArmHit {
            id,
            score: 1.0 - distance,
        })
        .collect();
    let keyword: Vec<ArmHit> = keyword_raw
        .into_iter()
        .filter(|(id, _)| keep(&by_id, &p, *id))
        .map(|(id, score)| ArmHit { id, score })
        .collect();
    // The temporal arm's hits are already recency-ordered by SQL; RRF reads
    // position only, and slot 3 has no named raw-score field
    // (`fusion::SOURCE_NAMES`), so the score is unused and stays 0.
    let temporal: Vec<ArmHit> = temporal_raw
        .into_iter()
        .filter(|id| keep(&by_id, &p, *id))
        .map(|id| ArmHit { id, score: 0.0 })
        .collect();

    // --- Pass 1: fuse the retrieval arms to pick graph seeds (R13). ------
    // The graph slot stays empty here; positions still matter, so it is a
    // placeholder rather than a missing list.
    let mut arms = vec![
        fusion::cap_per_source(&semantic, p.cap_per_source).to_vec(),
        fusion::cap_per_source(&keyword, p.cap_per_source).to_vec(),
        vec![], // graph, filled below
        fusion::cap_per_source(&temporal, p.cap_per_source).to_vec(),
    ];
    let seeds: Vec<i64> = fusion::reciprocal_rank_fusion(&arms, fusion::RRF_K)
        .into_iter()
        .take(graph::GRAPH_SEEDS)
        .map(|m| m.id)
        .collect();

    // --- Graph arm: 1 hop off the seeds, then hydrate whatever is new. ---
    if !seeds.is_empty() {
        let db = state.db.clone();
        let bank = bank_id.clone();
        let known: std::collections::HashSet<i64> = by_id.keys().copied().collect();
        let (hits, extra) = tokio::task::spawn_blocking(move || {
            let hits = graph::arm(&db, &bank, &seeds)?;
            let new_ids: Vec<i64> = hits
                .iter()
                .map(|h| h.id)
                .filter(|id| !known.contains(id))
                .collect();
            let extra = search::hydrate(&db, &bank, &new_ids)?;
            Ok::<_, memgarden_core::Error>((hits, extra))
        })
        .await
        .map_err(join_err)??;

        by_id.extend(extra.into_iter().map(|r| (r.id, r)));
        let graph_hits: Vec<ArmHit> = hits
            .into_iter()
            .filter(|h| keep(&by_id, &p, h.id))
            .collect();
        arms[2] = fusion::cap_per_source(&graph_hits, p.cap_per_source).to_vec();
    }

    // --- Pass 2: fuse all four arms, score, rank. ------------------------
    let mut merged = fusion::reciprocal_rank_fusion(&arms, fusion::RRF_K);
    // Counted before the reranker's truncation below: this is "how many
    // distinct nodes the arms produced", which is an arm-health number and
    // must not change meaning depending on a ranking config.
    let candidates = merged.len();

    // --- Cross-encoder rerank (CE-11), OFF by default. -------------------
    // Legacy truncates the candidate list to `rerank_limit` and cross-encodes
    // what survives (`memory_engine.py:5266`), dropping the tail outright;
    // `top_k` is that knob at a tenth the depth (10 vs 600), so it also caps
    // what recall can return whenever it is below `limit`.
    //
    // The `enabled` check comes first and short-circuits: with the reranker
    // off, `merged` is untouched and the base below stays
    // `scoring::passthrough_base`, so this whole block is provably a no-op
    // rather than a different-but-similar path. That is the parity claim.
    //
    // Keyed by node id, not by position: the scoring loop below re-filters
    // through `by_id`, and a positional zip would pair scores with the wrong
    // documents the first time a hydrate ever comes back short.
    let reranked: Option<std::collections::HashMap<i64, f64>> = if state.cfg.reranker.enabled {
        // Poisoned lock: recover for the same reason the embedder does — the
        // guarded value is an `Option<Arc<_>>` and cannot be torn.
        let reranker = state
            .reranker
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match reranker {
            Some(reranker) => {
                let candidates: Vec<rerank::Candidate<'_>> = merged
                    .iter()
                    .filter_map(|m| by_id.get(&m.id))
                    .map(|row| rerank::Candidate {
                        id: row.id,
                        text: &row.text,
                        context: row.context.as_deref(),
                        occurred_start: row.occurred_start,
                    })
                    .collect();
                let (ids, docs) = rerank::rerank_inputs(&candidates, state.cfg.reranker.top_k);
                drop(candidates); // borrows `by_id`, which the scoring loop reads
                let q = query.clone();
                let scored = tokio::task::spawn_blocking(move || reranker.scores(&q, &docs))
                    .await
                    .map_err(join_err)?;
                match scored {
                    Ok(scores) => {
                        // Truncated only on success, deliberately: it makes the
                        // failure arm below a *pure* passthrough, byte-identical
                        // to `enabled = false`, instead of a third behaviour
                        // (top_k results in RRF order) that nothing tests and
                        // no reader expects.
                        merged.truncate(state.cfg.reranker.top_k);
                        Some(ids.into_iter().zip(scores).collect())
                    }
                    // Degrade, do not 500. `Reranker::load`'s contract already
                    // promises the passthrough when the model is absent; a
                    // model that is present but erroring — a bad re-export, an
                    // ONNX runtime fault — is the same situation arriving
                    // later, and the same answer is the right one.
                    Err(e) => {
                        tracing::warn!(error = %e, "rerank failed; falling back to RRF passthrough");
                        None
                    }
                }
            }
            // Still loading, or it failed to load. Degrade to the passthrough
            // rather than 503: a ranking refinement being absent is not an
            // outage, and this is the ordering the rest of Phase B ships.
            None => None,
        }
    } else {
        None
    };

    let n = merged.len();
    // Min/max over the candidates the semantic arm actually scored. A set with
    // none of them leaves `lo > hi`, which `semantic_norm` reads as a
    // degenerate span and answers NEUTRAL — the boost is then exactly 1.0.
    let (sem_lo, sem_hi) = merged
        .iter()
        .filter_map(|m| m.semantic)
        .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let mut scored: Vec<(f64, RecallItem)> = merged
        .into_iter()
        .enumerate()
        .filter_map(|(rank0, m)| {
            let row = by_id.get(&m.id)?;
            // Base relevance. With the cross-encoder on it is the
            // sigmoid-normalized logit (`reranking.py:298-312`); with it off,
            // `merged` is already sorted by rrf_score desc, so its index IS
            // the passthrough reranker's `new_rank` (reranking.py:143).
            let base = match &reranked {
                Some(scores) => scores.get(&m.id).copied().unwrap_or(0.0),
                None => scoring::passthrough_base(n, rank0),
            };
            let recency = scoring::recency(
                scoring::effective_time(row.occurred_start, row.mentioned_at, row.occurred_end),
                p.now_ms,
            );
            // Neutral without a constraint — there is no window to be close
            // to, and NEUTRAL is exactly the 1.0 boost CE-6 shipped.
            let temporal = match window {
                Some((lo, hi)) => scoring::temporal_proximity(
                    scoring::temporal_best_time(
                        row.occurred_start,
                        row.occurred_end,
                        row.mentioned_at,
                    ),
                    lo,
                    hi,
                ),
                None => scoring::NEUTRAL,
            };
            // CE-9a: `proof_count` is 0 for everything but a sourced
            // observation, and `proof_norm` maps 0 (and 1) to NEUTRAL — so
            // this is the same number CE-6 shipped until B8's batch round
            // starts producing multi-source observations.
            let proof = scoring::proof_norm(row.proof_count);
            // Where this candidate's cosine sits in the spread this query
            // produced. Raw cosine is a narrow high band and would be another
            // near-constant multiplier; the position inside the band is what
            // carries information.
            let semantic = scoring::semantic_norm(m.semantic, sem_lo, sem_hi);
            let final_score = scoring::combined_with_semantic(
                base,
                recency,
                temporal,
                proof,
                semantic,
                p.semantic_alpha,
            );
            Some((
                final_score,
                RecallItem {
                    id: row.id,
                    uuid: row.uuid.clone(),
                    text: row.text.clone(),
                    fact_type: row.fact_type,
                    context: row.context.clone(),
                    tags: row.tags.clone(),
                    occurred_start: row.occurred_start,
                    occurred_end: row.occurred_end,
                    mentioned_at: row.mentioned_at,
                    scores: Scores {
                        final_score,
                        semantic: m.semantic,
                        keyword: m.keyword,
                        rrf: m.rrf_score,
                        recency,
                        temporal,
                        proof,
                    },
                },
            ))
        })
        .collect();

    // Stable sort: candidates whose boosts leave them tied keep RRF order.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // --- Drop a fact about to be injected twice. -------------------------
    //
    // Consolidation writes an observation that restates the facts it was
    // built from (CE-9a), and both can rank into the same result set — the
    // AC-1 shadow comparison found 99 of 1,323 injected items were such a
    // pair, 7.5% of the budget spent saying one thing twice. (Legacy has the
    // same flaw at 10.7%, so this is not a parity gap; it is a gap we can
    // close and legacy structurally cannot, because `node_sources` records
    // the pairing and legacy has no equivalent.)
    //
    // **Whichever ranked higher survives.** Not "always keep the
    // observation": an observation is more consolidated but sometimes drops
    // detail its source carried, and the ranking already encodes which one
    // this query wanted. Deduplicating by rank makes no claim about which
    // fact type is better.
    //
    // Before the budget truncation below, so the freed slots are filled by
    // the next candidate rather than lost.
    dedupe_restatements(&mut scored, &by_id);

    // And plain copies, which `dedupe_restatements` cannot see: those are
    // related through `node_sources`, these are unrelated rows that happen to
    // hold the same text. Also before the truncation, for the same reason.
    dedupe_identical_text(&mut scored);

    // Two distinct knobs (architect recommendation A): `budget` decides how
    // deep the candidate list is carried, `max_tokens` decides how much text
    // is injected. Legacy sends both and the fork's coding profile sends
    // `low` + 1024 — collapsing them would have capped the injection at 100
    // tokens and invalidated the AC-1 comparison.
    scored.truncate(budget::rerank_limit(budget::budget_tokens(&p.budget)));

    let mut results: Vec<RecallItem> = scored.into_iter().map(|(_, item)| item).collect();
    let texts: Vec<String> = results.iter().map(|r| r.text.clone()).collect();
    let (kept, tokens) = budget::fit_to_budget(&texts, p.max_tokens, token_count);
    results.truncate(kept.min(p.limit));
    // `limit` can cut below what the budget allowed, so recount rather than
    // reporting the budget's total.
    let tokens = if results.len() == kept {
        tokens
    } else {
        results.iter().map(|r| token_count(&r.text)).sum()
    };

    let injected_text = build_injection(&results, &p.preamble, p.now_ms);

    // The meter counts the whole injected block — framing, preamble and
    // timestamp included — because that is what the client actually pays
    // for. `counts.tokens` stays text-only, which is the number the budget
    // enforces and the number legacy reports (AC-1 parity).
    METRICS
        .recall_injected_tokens
        .fetch_add(token_count(&injected_text), Ordering::Relaxed);
    METRICS
        .recall_injected_memories
        .fetch_add(results.len() as u64, Ordering::Relaxed);

    Ok(RecallOutcome {
        counts: RecallCounts {
            candidates,
            returned: results.len(),
            tokens,
        },
        injected_text,
        results,
    })
}

/// `"%Y-%m-%d %H:%M UTC"` — `scripts/lib/content.py:231`. Falls back to the
/// raw millis on an out-of-range timestamp rather than panicking.
fn format_utc(unix_ms: i64) -> String {
    let Ok(ts) = jiff::Timestamp::from_millisecond(unix_ms) else {
        return unix_ms.to_string();
    };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::UTC);
    jiff::fmt::strtime::format("%Y-%m-%d %H:%M UTC", &zoned).unwrap_or_else(|_| unix_ms.to_string())
}

/// Neutralizes any `<memgarden_memories` / `</memgarden_memories` sequence
/// inside recalled text before it goes into the container.
///
/// Fact text is model-extracted from a transcript, i.e. attacker-influenced:
/// anything that can get a string into a retained conversation can get it
/// into a fact. A fact carrying `</memgarden_memories>` would close the block
/// early and everything after it would read to the client model as
/// out-of-band instruction rather than recalled memory. A zero-width space
/// after the `<` keeps the text legible to a human and to the model while
/// making the sequence no longer the tag.
///
/// Borrowed unless a `<` is actually present, which is the overwhelmingly
/// common case.
fn defang(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('<') {
        return std::borrow::Cow::Borrowed(text);
    }
    // Closing form first: after it runs, its output no longer contains the
    // opening form, so the second replace cannot double-process it.
    std::borrow::Cow::Owned(
        text.replace("</memgarden_memories", "<\u{200b}/memgarden_memories")
            .replace("<memgarden_memories", "<\u{200b}memgarden_memories"),
    )
}

/// The block the Phase C hook injects verbatim, reproducing
/// `scripts/recall.py:252-258` + `content.py:203-219`: item lines
/// `- {text} [{type}] ({mentioned_at})` joined by a blank line. Only the tag
/// name changes (`<memgarden_memories>`); B3's strip list covers both names.
///
/// Empty when there is nothing to inject — the hook prints nothing then, and
/// an empty envelope would cost tokens for no content.
fn build_injection(results: &[RecallItem], preamble: &str, now_ms: i64) -> String {
    if results.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = results
        .iter()
        .map(|r| {
            let date = r
                .mentioned_at
                .map(|ms| format!(" ({})", format_utc(ms)))
                .unwrap_or_default();
            format!("- {} [{}]{}", defang(&r.text), r.fact_type.as_str(), date)
        })
        .collect();
    format!(
        "<memgarden_memories>\n{preamble}\nCurrent time - {}\n\n{}\n</memgarden_memories>",
        format_utc(now_ms),
        lines.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tags_match_modes() {
        let node = tags(&["a", "b"]);
        let untagged: Vec<String> = vec![];
        let req = tags(&["a", "c"]);

        // any: overlap, untagged included.
        assert!(TagsMatch::Any.matches(&node, &req));
        assert!(TagsMatch::Any.matches(&untagged, &req));
        // any_strict: overlap, untagged excluded.
        assert!(TagsMatch::AnyStrict.matches(&node, &req));
        assert!(!TagsMatch::AnyStrict.matches(&untagged, &req));
        // all: every requested tag present.
        assert!(!TagsMatch::All.matches(&node, &req));
        assert!(TagsMatch::All.matches(&node, &tags(&["a", "b"])));
        assert!(TagsMatch::All.matches(&untagged, &req));
        // all_strict: every requested tag present, untagged excluded.
        assert!(!TagsMatch::AllStrict.matches(&untagged, &req));
        assert!(TagsMatch::AllStrict.matches(&node, &tags(&["b"])));
        // No request tags means no filtering, in every mode.
        for mode in [
            TagsMatch::Any,
            TagsMatch::All,
            TagsMatch::AnyStrict,
            TagsMatch::AllStrict,
        ] {
            assert!(mode.matches(&node, &[]));
            assert!(mode.matches(&untagged, &[]));
        }
    }

    #[test]
    fn over_fetch_floor_and_clamp() {
        assert_eq!(over_fetch(1), 100, "floor");
        assert_eq!(over_fetch(20), 100);
        assert_eq!(over_fetch(50), 250);
        assert_eq!(over_fetch(10_000), MAX_OVER_FETCH, "clamp");
    }

    fn item(text: &str, fact_type: FactType, mentioned_at: Option<i64>) -> RecallItem {
        RecallItem {
            id: 1,
            uuid: "u".to_string(),
            text: text.to_string(),
            fact_type,
            context: None,
            tags: vec![],
            occurred_start: None,
            occurred_end: None,
            mentioned_at,
            scores: Scores {
                final_score: 1.0,
                semantic: None,
                keyword: None,
                rrf: 0.0,
                recency: 0.5,
                temporal: 0.5,
                proof: 0.5,
            },
        }
    }

    /// 2026-08-02T04:55:41Z — deliberately not on a minute boundary, so a
    /// seconds field creeping into the format would fail the assert.
    const NOW: i64 = 1_785_646_541_000;
    /// 2026-07-01T09:30:00Z.
    const MENTIONED: i64 = 1_782_898_200_000;

    #[test]
    fn injection_is_byte_exact() {
        let results = vec![
            item(
                "the daemon binds 127.0.0.1:9100",
                FactType::World,
                Some(MENTIONED),
            ),
            item("메모리 회수는 하이브리드", FactType::Observation, None),
        ];
        let text = build_injection(&results, "Relevant memories:", NOW);
        assert_eq!(
            text,
            "<memgarden_memories>\n\
             Relevant memories:\n\
             Current time - 2026-08-02 04:55 UTC\n\
             \n\
             - the daemon binds 127.0.0.1:9100 [world] (2026-07-01 09:30 UTC)\n\
             \n\
             - 메모리 회수는 하이브리드 [observation]\n\
             </memgarden_memories>"
        );
    }

    #[test]
    fn injection_empty_when_nothing_recalled() {
        assert_eq!(build_injection(&[], "preamble", NOW), "");
    }

    #[test]
    fn injection_keeps_the_blank_preamble_line() {
        // An empty preamble still leaves its line — that is what the fork
        // emits (`recall.py:252-258` interpolates unconditionally), and the
        // Phase C hook strips the whole block by tag, not by line count.
        let text = build_injection(&[item("x", FactType::World, None)], "", NOW);
        assert!(
            text.starts_with("<memgarden_memories>\n\nCurrent time - "),
            "{text}"
        );
    }

    #[test]
    fn format_utc_survives_a_garbage_timestamp() {
        assert_eq!(format_utc(i64::MAX), i64::MAX.to_string());
    }

    // --- dedupe_restatements (AC-1) -------------------------------------

    fn ranked(id: i64) -> (f64, RecallItem) {
        (
            0.0,
            RecallItem {
                id,
                uuid: format!("u{id}"),
                text: format!("t{id}"),
                fact_type: FactType::World,
                context: None,
                tags: vec![],
                occurred_start: None,
                occurred_end: None,
                mentioned_at: None,
                scores: Scores {
                    final_score: 0.0,
                    semantic: None,
                    keyword: None,
                    rrf: 0.0,
                    recency: 0.0,
                    temporal: 0.0,
                    proof: 0.0,
                },
            },
        )
    }

    fn sourced(id: i64, sources: Vec<i64>) -> CandidateRow {
        CandidateRow {
            id,
            uuid: format!("u{id}"),
            fact_type: FactType::Observation,
            text: format!("t{id}"),
            context: None,
            occurred_start: None,
            occurred_end: None,
            mentioned_at: None,
            tags: vec![],
            proof_count: sources.len() as i64,
            sources,
        }
    }

    /// `ranked` gives every item a distinct `t{id}`; this one forces a text.
    fn worded(id: i64, text: &str) -> (f64, RecallItem) {
        let mut r = ranked(id);
        r.1.text = text.to_string();
        r
    }

    #[test]
    fn identical_text_is_injected_once_and_the_best_ranked_copy_survives() {
        // The live shape: one line captured five times by a single retain.
        let mut scored = vec![
            worded(7, "PR #3086 is open with no reviews"),
            worded(11, "something else entirely"),
            worded(9, "PR #3086 is open with no reviews"),
            worded(4, "PR #3086 is open with no reviews"),
        ];
        dedupe_identical_text(&mut scored);
        assert_eq!(
            id_list(&scored),
            vec![7, 11],
            "the first (best-ranked) copy stays, the later ones go"
        );
    }

    #[test]
    fn only_whitespace_is_normalised_before_comparing() {
        let mut scored = vec![
            worded(1, "a  fact   about\nthings"),
            worded(2, "a fact about things"),
            worded(3, "a fact about other things"),
        ];
        dedupe_identical_text(&mut scored);
        assert_eq!(
            id_list(&scored),
            vec![1, 3],
            "whitespace-equal collapses; a differing word is a different claim"
        );
    }

    /// Across fact types on purpose: a `world` fact and an `observation`
    /// holding the same sentence are one sentence to whoever reads the
    /// injection, and spending two budget slots on it helps nobody. This is a
    /// decision rather than a detail — `recall_types_filters_and_defaults_to_all_three`
    /// had to stop seeding one text three times because of it.
    #[test]
    fn identical_text_collapses_across_fact_types() {
        let mut a = worded(1, "deployment checklist");
        a.1.fact_type = FactType::World;
        let mut b = worded(2, "deployment checklist");
        b.1.fact_type = FactType::Observation;
        let mut c = worded(3, "deployment checklist");
        c.1.fact_type = FactType::Experience;
        let mut scored = vec![a, b, c];
        dedupe_identical_text(&mut scored);
        assert_eq!(id_list(&scored), vec![1]);
    }

    /// Reported by review, and it reproduces: a multi-source observation that
    /// ranks *between* its own sources takes one of them down with it.
    ///
    /// `scored = [F, O, G]`, `O.sources = [F, G]`. The loop sees F above O and
    /// drops O — correct — then sees G below O and drops G as the "restated"
    /// end of a pair whose other end has already been removed. G leaves the
    /// result with nothing standing in for it.
    #[test]
    fn a_source_is_not_dropped_for_an_observation_that_is_itself_dropped() {
        let mut by_id = std::collections::HashMap::new();
        by_id.insert(1, sourced(1, vec![]));
        by_id.insert(2, sourced(2, vec![1, 3]));
        by_id.insert(3, sourced(3, vec![]));
        let mut scored = vec![ranked(1), ranked(2), ranked(3)];
        dedupe_restatements(&mut scored, &by_id);
        assert_eq!(
            id_list(&scored),
            vec![1, 3],
            "the observation loses to its best source; both sources survive"
        );
    }

    #[test]
    fn dedupe_identical_text_is_a_no_op_on_distinct_items() {
        let mut scored = vec![ranked(1), ranked(2), ranked(3)];
        dedupe_identical_text(&mut scored);
        assert_eq!(id_list(&scored), vec![1, 2, 3]);
        let mut one = vec![ranked(1)];
        dedupe_identical_text(&mut one);
        assert_eq!(id_list(&one), vec![1]);
    }

    fn id_list(scored: &[(f64, RecallItem)]) -> Vec<i64> {
        scored.iter().map(|(_, i)| i.id).collect()
    }

    /// Rank decides, not fact type: the observation is second here, so it is
    /// the one that goes.
    #[test]
    fn the_lower_ranked_end_of_a_restatement_is_dropped() {
        let mut scored = vec![ranked(1), ranked(2)]; // 1 outranks 2
        let by_id = std::collections::HashMap::from([(2, sourced(2, vec![1]))]);
        dedupe_restatements(&mut scored, &by_id);
        assert_eq!(
            id_list(&scored),
            vec![1],
            "the source outranked its observation"
        );

        // Same pair, opposite ranking — now the source is the one dropped.
        let mut scored = vec![ranked(2), ranked(1)];
        let by_id = std::collections::HashMap::from([(2, sourced(2, vec![1]))]);
        dedupe_restatements(&mut scored, &by_id);
        assert_eq!(
            id_list(&scored),
            vec![2],
            "the observation outranked its source"
        );
    }

    /// An observation whose sources did not rank keeps its place: nothing is
    /// being said twice.
    #[test]
    fn a_source_that_did_not_rank_removes_nothing() {
        let mut scored = vec![ranked(2), ranked(3)];
        let by_id = std::collections::HashMap::from([(2, sourced(2, vec![99]))]);
        dedupe_restatements(&mut scored, &by_id);
        assert_eq!(id_list(&scored), vec![2, 3]);
    }

    /// A multi-source observation that outranks them collapses all of them —
    /// that is the whole point of `proof_count > 1`.
    #[test]
    fn a_multi_source_observation_absorbs_every_source_it_outranks() {
        let mut scored = vec![ranked(10), ranked(1), ranked(2), ranked(3)];
        let by_id = std::collections::HashMap::from([(10, sourced(10, vec![1, 2, 3]))]);
        dedupe_restatements(&mut scored, &by_id);
        assert_eq!(id_list(&scored), vec![10]);
    }

    /// The pair is data (`node_sources`), not similarity: two unrelated
    /// candidates with the same text are both kept, because nothing recorded
    /// that one was built from the other.
    #[test]
    fn identical_text_without_provenance_is_left_alone() {
        let mut scored = vec![ranked(1), ranked(2)];
        let by_id =
            std::collections::HashMap::from([(1, sourced(1, vec![])), (2, sourced(2, vec![]))]);
        dedupe_restatements(&mut scored, &by_id);
        assert_eq!(id_list(&scored), vec![1, 2]);
    }
}
