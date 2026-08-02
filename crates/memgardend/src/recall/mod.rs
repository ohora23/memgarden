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
    pub preamble: String,
    /// Injected rather than read from the clock so `injected_text` can be
    /// asserted byte-exact (Critic Revision NIT-20).
    pub now_ms: i64,
}

/// Per-result score breakdown. `proof` is still stubbed at
/// `scoring::NEUTRAL` and filled in by CE-9; `temporal` is live as of CE-8
/// whenever the query carries a constraint (`scoring::NEUTRAL` when it does
/// not, which is the same number the stub returned).
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
    let merged = fusion::reciprocal_rank_fusion(&arms, fusion::RRF_K);
    let candidates = merged.len();

    let n = merged.len();
    let mut scored: Vec<(f64, RecallItem)> = merged
        .into_iter()
        .enumerate()
        .filter_map(|(rank0, m)| {
            let row = by_id.get(&m.id)?;
            // merged is already sorted by rrf_score desc, so its index IS
            // the passthrough reranker's `new_rank` (reranking.py:143).
            let base = scoring::passthrough_base(n, rank0);
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
            let final_score = scoring::combined(base, recency, temporal, scoring::NEUTRAL);
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
                        proof: scoring::NEUTRAL,
                    },
                },
            ))
        })
        .collect();

    // Stable sort: candidates whose boosts leave them tied keep RRF order.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

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
}
