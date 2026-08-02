//! The batch consolidation round (CE-9b), ported from
//! `engine/consolidation/consolidator.py:1002-1526` (the job) and
//! `:2400-2555` (one LLM batch).
//!
//! One round reads at most `batch_size` not-yet-consolidated facts, splits
//! them into `llm_batch_size` groups, and asks the model — once per group,
//! **sequentially** — to fold them into the bank's existing observations. The
//! reply is a `{creates, updates, deletes}` plan, validated hard and applied
//! in one write transaction per group. Each created observation then goes
//! through CE-9a's semantic dedup.
//!
//! ## The prompt is token-bounded by construction
//!
//! CE-9a bounded a *one-pair* prompt. This module assembles a **batch**
//! prompt — a system message, a mission, N fact lines and a pool of existing
//! observations — which is exactly the shape that pinned a GPU for over an
//! hour on 2026-08-02. The legacy dominant term was a per-fact source-facts
//! budget (4096 tokens) multiplied by an LLM batch size of 8; the assembled
//! prompt outgrew `num_ctx`, Ollama truncated the input with `keep=4` (eating
//! the system prompt), the model rambled past the client timeout, and the
//! identical payload was retried forever. The fix there was three *config*
//! values — one edit away from being gone.
//!
//! Here the bound is code, at both ends and by construction:
//!
//! | | |
//! |---|---|
//! | Prompt | [`CONSOLIDATION_PROMPT_MAX_TOKENS`], a `const`, counted with `retain::token_count` over **system + user** before every call |
//! | Reply | [`CONSOLIDATION_REPLY_MAX_TOKENS`], a `const`, applied per call as a `num_predict` ceiling, plus `maxLength` on every free-text field |
//! | Window | [`CONSOLIDATION_NUM_CTX`] is requested explicitly, so prompt + reply fits whatever the server's own default happens to be |
//! | Source facts | **never embedded** in a pooled observation — the incident's actual multiplier, see [`prompts::observation_entry`] |
//! | Enforcement | [`assemble`], the only path from this module to Ollama |
//!
//! **Nothing is ever truncated.** Over-budget input is shed *whole*, in a
//! documented order (see [`assemble`]), because a fact with its tail cut off
//! becomes an observation that silently asserts less than the fact did.
//!
//! [`prompts::observation_entry`]: super::prompts::observation_entry

use std::collections::{HashMap, HashSet, VecDeque};

use serde::Deserialize;
use serde_json::{Value, json};

use memgarden_core::config::ConsolidationConfig;
use memgarden_core::error::{Error, Result};
use memgarden_core::types::FactType;
use memgarden_store::search::CandidateRow;
use memgarden_store::{banks, consolidate as store, search};

use super::prompts;
use crate::recall::{RecallParams, TagsMatch};
use crate::retain::token_count;
use crate::state::AppState;

/// Hard ceiling on the assembled batch prompt (**system + user**), in cl100k
/// tokens. **Not configurable — see the module docs for why.**
///
/// The system message alone is **2,314 measured tokens** of ported rules and
/// worked examples, so this cannot be CE-9a's 2048. What it *is* is measured
/// against the window this module explicitly requests
/// ([`CONSOLIDATION_NUM_CTX`] = 8192): 6144 + the 1536-token reply is 7680,
/// leaving 512 tokens of headroom for the chat template's role markers, which
/// are not counted here (a few dozen tokens in practice).
///
/// Real batches land nowhere near it — measured: system + 8 engineering fact
/// lines = **2,702**, and with a six-observation pool at the configured
/// `max_tokens` = **3,075**. The margin is deliberate: a bound that trips on
/// ordinary input is a bound nobody keeps. It is also *bounded above*, which
/// is what makes it a real `const` — the num_ctx assertion in
/// `the_budget_leaves_real_room_for_a_full_batch` fails if this is inflated
/// past 6656, and
/// `an_over_budget_batch_bisects_and_a_lone_over_budget_fact_is_refused`
/// fails if it is deleted.
pub const CONSOLIDATION_PROMPT_MAX_TOKENS: u64 = 6144;

/// Hard ceiling on the **reply**, in tokens — the incident's second stage.
///
/// A prompt that fits still lets the model start generating, ramble, and
/// exhaust the window mid-generation, at which point Ollama context-shifts:
/// the same truncation mechanism reached from the far side.
/// `ollama.num_predict` defaults to 8192, so the shared default bounds
/// nothing; without this the only limit is the client's total deadline.
///
/// 1536 is generous for a plan over 8 facts (~8 entries of text + reason).
/// A reply that overruns is cut off mid-JSON → unparseable → the batch is
/// retried and then skipped, which is the safe direction: a long-winded model
/// costs a batch, never a fact.
pub const CONSOLIDATION_REPLY_MAX_TOKENS: u32 = 1536;

/// The context window this module asks Ollama for, per call.
///
/// CE-9a could rely on the server default (4096) because its pair prompt fit
/// in half of it. A batch prompt does not, so the window becomes part of the
/// bound rather than an assumption about the deployment: prompt + reply is
/// 7680 against 8192 whatever `num_ctx` the server would otherwise have
/// picked. A server that cannot honour it clamps *down*, and the model then
/// truncates a reply into unparseable JSON — the batch is skipped, no fact is
/// lost. Local to this call; the shared client's other callers are untouched.
pub const CONSOLIDATION_NUM_CTX: u32 = 8192;

/// Ceiling on CE-9a dedup adjudications in one round (CE-9a handoff #8).
///
/// Each is one LLM call at **~1-2 s measured** behind `ollama.max_concurrent
/// = 1`, so an uncapped round that creates 30 near-duplicates serialises for
/// most of a minute with nothing else able to reach the GPU — including the
/// interactive `/dry-run-extract` path, which fails at 15 s rather than
/// queueing. 8 caps the dedup spend at roughly 8-16 s against the 300 s
/// interval, i.e. a few percent duty cycle.
///
/// // ponytail: a whole-number cap, not a time budget. Deduplication skipped
/// // this round is not lost — the *next* round's creates dedup against these
/// // same observations, so a burst drains over several rounds instead of one
/// // long GPU hold. Swap for a wall-clock budget only if rounds ever stop
/// // draining.
pub const MAX_ADJUDICATIONS_PER_ROUND: usize = 8;

/// What one round did. `run_id` is `None` when there was nothing to do — a
/// no-op round writes no ledger row at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RoundSummary {
    pub run_id: Option<i64>,
    pub facts_seen: usize,
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    pub merged: usize,
    /// LLM batch calls that produced an applied plan.
    pub batches: usize,
    /// Batches abandoned after `max_attempts` (unparseable or invalid replies).
    pub skipped_batches: usize,
    /// Facts dropped because one fact alone could not fit the prompt budget.
    pub dropped_facts: usize,
    /// CE-9a adjudications actually spent (capped, see
    /// [`MAX_ADJUDICATIONS_PER_ROUND`]).
    pub adjudications: usize,
    pub watermark: i64,
}

/// Runs one consolidation round for `bank_id`.
///
/// Cheap and side-effect-free when there is nothing new: one indexed range
/// scan, no run row, no LLM call.
pub async fn run_round(state: &AppState, bank_id: &str) -> Result<RoundSummary> {
    let cfg = state.cfg.consolidation.clone();
    let db = state.db.clone();

    let (start_wm, facts, mission) = {
        let (db, bank) = (db.clone(), bank_id.to_string());
        let batch_size = cfg.batch_size;
        blocking(move || {
            let wm = store::watermark(&db, &bank)?;
            let facts = store::unconsolidated(&db, &bank, wm, batch_size)?;
            let mission = banks::get(&db, &bank)?
                .and_then(|b| b.mission)
                .unwrap_or_default();
            Ok((wm, facts, mission))
        })
        .await?
    };
    if facts.is_empty() {
        return Ok(RoundSummary {
            watermark: start_wm,
            ..Default::default()
        });
    }

    let run_id = {
        let (db, bank) = (db.clone(), bank_id.to_string());
        blocking(move || store::start_run(&db, &bank)).await?
    };

    let mut summary = RoundSummary {
        run_id: Some(run_id),
        facts_seen: facts.len(),
        watermark: start_wm,
        ..Default::default()
    };
    let mut budget = MAX_ADJUDICATIONS_PER_ROUND;

    // Contiguous index ranges into `facts`, oldest first. A range that does
    // not fit the prompt budget is bisected and its halves go back on the
    // front, so the queue stays in id order and the watermark stays
    // contiguous (`consolidator.py:1175` bisects the same way, on LLM
    // failure rather than on size).
    let mut queue: VecDeque<(usize, usize)> = (0..facts.len())
        .step_by(cfg.llm_batch_size.max(1))
        .map(|start| (start, (start + cfg.llm_batch_size.max(1)).min(facts.len())))
        .collect();

    while let Some((start, end)) = queue.pop_front() {
        let outcome = process_batch(
            state,
            &cfg,
            bank_id,
            &mission,
            &facts[start..end],
            &mut budget,
        )
        .await;
        match outcome {
            Ok(BatchOutcome::Split) => {
                let mid = start + (end - start) / 2;
                queue.push_front((mid, end));
                queue.push_front((start, mid));
                continue;
            }
            Ok(BatchOutcome::Applied {
                created,
                updated,
                deleted,
                merged,
                adjudications,
            }) => {
                summary.batches += 1;
                summary.created += created;
                summary.updated += updated;
                summary.deleted += deleted;
                summary.merged += merged;
                summary.adjudications += adjudications;
            }
            Ok(BatchOutcome::Skipped) => summary.skipped_batches += 1,
            Ok(BatchOutcome::Dropped) => summary.dropped_facts += end - start,
            Err(e) => {
                // Close the ledger with whatever the round committed before
                // the failure. The watermark is written even on a failure:
                // the earlier batches really were applied, and replaying them
                // would create duplicate observations.
                let (db, msg) = (db.clone(), e.to_string());
                let (counts, wm) = (summary.counts(), summary.watermark);
                let _ = blocking(move || {
                    store::finish_run(&db, run_id, "failed", counts, Some(wm), Some(&msg))
                })
                .await;
                return Err(e);
            }
        }
        // Every fact in the range has reached a terminal decision (applied,
        // skipped, or dropped), so the mark may pass it.
        summary.watermark = facts[end - 1].id;
    }

    let (db, counts, wm) = (db.clone(), summary.counts(), summary.watermark);
    blocking(move || store::finish_run(&db, run_id, "done", counts, Some(wm), None)).await?;
    Ok(summary)
}

impl RoundSummary {
    fn counts(&self) -> store::RunCounts {
        store::RunCounts {
            facts_seen: self.facts_seen as i64,
            created_n: self.created as i64,
            updated_n: self.updated as i64,
            deleted_n: self.deleted as i64,
            merged_n: self.merged as i64,
        }
    }
}

enum BatchOutcome {
    /// Applied, with what it produced.
    Applied {
        created: usize,
        updated: usize,
        deleted: usize,
        merged: usize,
        adjudications: usize,
    },
    /// Over the prompt budget with more than one fact — bisect and retry.
    Split,
    /// One fact, alone, over the prompt budget. It is dropped: the round must
    /// make forward progress, and a batch retried forever on identical input
    /// *is* the 2026-08-02 incident.
    Dropped,
    /// `max_attempts` replies were unparseable or invalid.
    Skipped,
}

async fn process_batch(
    state: &AppState,
    cfg: &ConsolidationConfig,
    bank_id: &str,
    mission: &str,
    facts: &[store::FactRow],
    adjudication_budget: &mut usize,
) -> Result<BatchOutcome> {
    let pool = pool_observations(state, cfg, bank_id, facts).await?;
    let Some((system, user, pool)) = assemble(mission, facts, pool) else {
        return Ok(if facts.len() > 1 {
            BatchOutcome::Split
        } else {
            tracing::warn!(
                fact_id = facts[0].id,
                "a single fact exceeds the consolidation prompt budget; dropping it"
            );
            BatchOutcome::Dropped
        });
    };

    let by_fact_uuid: HashMap<&str, i64> = facts.iter().map(|f| (f.uuid.as_str(), f.id)).collect();
    // Observations stay uuids all the way to the write: `apply_plan` resolves
    // them itself, so a rowid recycled between the pooling recall and the
    // write cannot be mistaken for its previous occupant.
    let pooled_uuids: HashSet<&str> = pool.iter().map(|o| o.uuid.as_str()).collect();

    // `consolidation_max_attempts` (`config.py:1147`) is the OUTER loop: the
    // client already retries transport and parse failures internally
    // (`ollama.max_retries`). This retries a *semantically* invalid plan —
    // two updates to one observation — which no transport retry would fix.
    let mut plan = None;
    for attempt in 1..=cfg.max_attempts.max(1) {
        let raw: std::result::Result<Value, _> = state
            .ollama
            .chat_json_background_bounded(
                &system,
                &user,
                &plan_schema(),
                CONSOLIDATION_REPLY_MAX_TOKENS,
                Some(CONSOLIDATION_NUM_CTX),
            )
            .await;
        match raw {
            Ok(value) => match validate(&value, &by_fact_uuid, &pooled_uuids) {
                Ok(p) => {
                    plan = Some(p);
                    break;
                }
                Err(reason) => tracing::warn!(
                    attempt,
                    max = cfg.max_attempts,
                    reason,
                    "consolidation batch rejected"
                ),
            },
            Err(e) => {
                tracing::warn!(attempt, max = cfg.max_attempts, error = %e, "consolidation batch call failed")
            }
        }
    }
    let Some(plan) = plan else {
        return Ok(BatchOutcome::Skipped);
    };

    // Embeddings are computed BEFORE the write transaction and one
    // observation at a time (CE-9a handoff #6): the ONNX model sits behind a
    // single process-wide mutex that interactive query embedding also needs,
    // so a batch-sized hold is a batch-sized stall on recall's semantic arm.
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(plan.creates.len());
    for c in &plan.creates {
        vectors.push(embed_one(state, &c.0).await?);
    }

    let created_ids = {
        let (db, bank) = (state.db.clone(), bank_id.to_string());
        let (creates, updates, deletes) = (
            plan.creates.clone(),
            plan.updates.clone(),
            plan.deletes.clone(),
        );
        let vectors = vectors.clone();
        blocking(move || {
            let creates: Vec<store::NewObservation> = creates
                .iter()
                .zip(&vectors)
                .map(|((text, sources), embedding)| store::NewObservation {
                    text,
                    embedding,
                    source_ids: sources,
                })
                .collect();
            let updates: Vec<store::ObservationUpdate> = updates
                .iter()
                .map(|(uuid, text, sources)| store::ObservationUpdate {
                    uuid,
                    text,
                    source_ids: sources,
                })
                .collect();
            let deletes: Vec<&str> = deletes.iter().map(String::as_str).collect();
            store::apply_plan(&db, &bank, &creates, &updates, &deletes)
        })
        .await?
    };

    // CE-9a's dedup, once per created observation, after the write and
    // outside it — the write lock is never held across an LLM call.
    let mut merged = 0usize;
    let mut adjudications = 0usize;
    for ((text, _), (id, embedding)) in plan
        .creates
        .iter()
        .zip(created_ids.created.iter().zip(&vectors))
    {
        if *adjudication_budget == 0 {
            tracing::debug!(
                "per-round dedup adjudication cap reached; remaining creates dedup next round"
            );
            break;
        }
        *adjudication_budget -= 1;
        adjudications += 1;
        match super::dedup_created(&state.db, &state.ollama, cfg, bank_id, *id, text, embedding)
            .await?
        {
            super::Outcome::Merged { into, .. } => {
                merged += 1;
                reembed_merged_twin(state, bank_id, into).await;
            }
            super::Outcome::Created { .. } => {}
        }
    }

    Ok(BatchOutcome::Applied {
        created: created_ids.created.len(),
        updated: created_ids.updated,
        deleted: created_ids.deleted,
        merged,
        adjudications,
    })
}

/// Re-embeds a twin whose text a merge just rewrote — CE-9a's correctness
/// debt #9, closing the null-embedding window it left open.
///
/// `merge_observation` nulls the embedding so the backlog regenerates it
/// (R4's one text-update rule), which leaves the twin **out of
/// `observation_vectors`** — a dedup probe requires `embedding IS NOT NULL`
/// — for one `embedding.backlog_poll_secs` tick, and *unbounded* if the
/// embedder never loaded. Inside a batch round the next created observation
/// is milliseconds away, so a third near-duplicate arriving in that window
/// creates instead of merging, and dedup never revisits it. CE-9a could not
/// fix this: `store_observation` has no embedder by construction (R3's
/// parameter design). This module holds one.
///
/// Best effort by design. The merge is already committed and durable; a
/// failure here costs exactly what CE-9a shipped — a backlog tick — so
/// turning it into a round failure would trade a small window for a large one.
async fn reembed_merged_twin(state: &AppState, bank_id: &str, twin_id: i64) {
    let db = state.db.clone();
    let text = match blocking(move || memgarden_store::nodes::get(&db, twin_id)).await {
        Ok(Some(node)) => node.text,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(twin_id, error = %e, "could not read a merged twin back to re-embed it");
            return;
        }
    };
    let embedding = match embed_one(state, &text).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(twin_id, error = %e, "merged twin left on the embed backlog");
            return;
        }
    };
    let (db, bank) = (state.db.clone(), bank_id.to_string());
    if let Err(e) =
        blocking(move || memgarden_store::nodes::set_embedding(&db, twin_id, &bank, &embedding))
            .await
    {
        tracing::warn!(twin_id, error = %e, "merged twin left on the embed backlog");
    }
}

/// Existing observations related to this batch's facts, pooled and
/// token-bounded (`_find_related_observations`, `consolidator.py:2250-2320`).
///
/// One recall per fact, `fact_type = ["observation"]`, at
/// `consolidation.recall_budget` — legacy runs the same per-fact recall and
/// unions the results ("A JSON array pooled from recalls across the new
/// facts", `prompts.py:68`). Rank order is preserved across the union: a
/// twin that a fact's own recall put first stays first, which is the whole
/// reason legacy forces `reranking="interleave"` there.
///
/// The pool is then cut to `consolidation.max_tokens` (512,
/// `config.py:1163`) counted over observation **text** — legacy's
/// `max_tokens` on the same call. That is the primary bound; the prompt
/// `const` is the backstop that does not move when config does.
async fn pool_observations(
    state: &AppState,
    cfg: &ConsolidationConfig,
    bank_id: &str,
    facts: &[store::FactRow],
) -> Result<Vec<CandidateRow>> {
    let mut ranked: Vec<i64> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    for fact in facts {
        let params = RecallParams {
            query: fact.text.clone(),
            limit: state.cfg.recall.limit,
            budget: cfg.recall_budget.clone(),
            max_tokens: cfg.max_tokens,
            fact_types: vec![FactType::Observation],
            // Not tag-scoped: legacy's `all_strict` tag filter serves
            // per-tenant observation scopes, which this deployment does not
            // have (the same divergence CE-9a recorded for the dedup probe).
            tags: vec![],
            tags_match: TagsMatch::Any,
            cap_per_source: state.cfg.recall.cap_per_source,
            preamble: String::new(),
            now_ms: memgarden_core::now_ms(),
        };
        let out = crate::recall::recall(state, bank_id.to_string(), params)
            .await
            .map_err(|e| Error::Storage(format!("consolidation recall failed: {e:?}")))?;
        for item in out.results {
            if seen.insert(item.id) {
                ranked.push(item.id);
            }
        }
    }
    if ranked.is_empty() {
        return Ok(vec![]);
    }

    let (db, bank, ids) = (state.db.clone(), bank_id.to_string(), ranked.clone());
    let rows = blocking(move || search::hydrate(&db, &bank, &ids)).await?;
    let by_id: HashMap<i64, CandidateRow> = rows.into_iter().map(|r| (r.id, r)).collect();

    let mut pooled = Vec::new();
    let mut spent = 0u64;
    for id in ranked {
        let Some(row) = by_id.get(&id) else { continue };
        let cost = token_count(&row.text);
        if spent + cost > cfg.max_tokens as u64 && !pooled.is_empty() {
            break;
        }
        spent += cost;
        pooled.push(row.clone());
    }
    Ok(pooled)
}

/// Renders the pair and enforces [`CONSOLIDATION_PROMPT_MAX_TOKENS`],
/// returning `None` when the batch cannot be made to fit.
///
/// **Shed order, deterministic and documented:**
///
/// 1. The pooled observations are shed from the **tail** — lowest-ranked
///    first, so the nearest twin (the one an UPDATE would target) is the last
///    thing to go. Losing a pooled observation costs a merge opportunity;
///    the fact is still consolidated, just possibly into a new sibling that
///    the next round's dedup can fold in.
/// 2. With an empty pool and still over budget, `None` means *bisect* — the
///    caller splits the batch and tries the halves, so no fact is ever shed
///    for size while another fact in its group is processed.
/// 3. One fact alone over the budget is dropped by the caller. That fact's
///    row stays in the bank and stays recallable; only its consolidation is
///    skipped.
///
/// **Nothing is truncated at any step.** The prompt asks for observations
/// that preserve what the facts said; hand the model a fact with its tail cut
/// off and it writes an observation that quietly asserts less — and unlike a
/// dropped batch, that error is durable and invisible.
fn assemble(
    mission: &str,
    facts: &[store::FactRow],
    mut pool: Vec<CandidateRow>,
) -> Option<(String, String, Vec<CandidateRow>)> {
    let system = prompts::build_consolidation_system_prompt();
    let facts_text = facts
        .iter()
        .map(|f| {
            prompts::fact_line(
                &f.uuid,
                &f.text,
                f.occurred_start,
                f.occurred_end,
                f.mentioned_at,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let system_tokens = token_count(&system);

    loop {
        let entries: Vec<Value> = pool
            .iter()
            .map(|o| {
                prompts::observation_entry(
                    &o.uuid,
                    &o.text,
                    o.proof_count,
                    o.occurred_start,
                    o.occurred_end,
                    o.mentioned_at,
                )
            })
            .collect();
        let user = prompts::build_consolidation_input(
            mission,
            &facts_text,
            &prompts::build_observations_json(&entries),
        );
        if system_tokens + token_count(&user) <= CONSOLIDATION_PROMPT_MAX_TOKENS {
            return Some((system, user, pool));
        }
        // Nothing left to shed: the facts alone are over budget, so the
        // caller bisects (or drops a lone fact). Never truncate.
        pool.pop()?;
    }
}

/// A validated plan: ids already resolved, every entry known-good.
#[derive(Debug, Default, Clone, PartialEq)]
struct Plan {
    /// `(text, source_ids)`
    creates: Vec<(String, Vec<i64>)>,
    /// `(observation uuid, text, source_ids)`
    updates: Vec<(String, String, Vec<i64>)>,
    /// Observation uuids.
    deletes: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawPlan {
    #[serde(default)]
    creates: Vec<RawCreate>,
    #[serde(default)]
    updates: Vec<RawUpdate>,
    #[serde(default)]
    deletes: Vec<RawDelete>,
}

#[derive(Debug, Deserialize)]
struct RawCreate {
    #[serde(default)]
    text: String,
    #[serde(default)]
    source_fact_ids: Vec<String>,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize)]
struct RawUpdate {
    #[serde(default)]
    observation_id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    source_fact_ids: Vec<String>,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize)]
struct RawDelete {
    #[serde(default)]
    observation_id: String,
    #[serde(default)]
    reason: String,
}

/// Turns a reply into a [`Plan`], or rejects the whole batch.
///
/// **Rejecting** (`Err`) is reserved for one condition: two `updates`
/// entries naming the same `observation_id`. The prompt forbids it in
/// capitals (`prompts.py:137`) precisely because applying both means the
/// second write silently overwrites the first — one of the two consolidations
/// the model intended is destroyed with no trace. Legacy collapses the pair
/// and warns (`_dedupe_updates`, `consolidator.py:2362-2397`); this rejects,
/// because a model confused enough to emit two plans for one observation is
/// not a model whose merge of those plans should be trusted, and the round
/// gets `max_attempts` fresh tries before the batch is skipped. Divergence,
/// recorded in the design note.
///
/// **Everything else drops the offending entry and keeps going** — the same
/// asymmetry CE-9a's parser has. A dropped entry costs one observation; a
/// rejected round costs a bank's whole backlog.
fn validate(
    raw: &Value,
    facts: &HashMap<&str, i64>,
    observations: &HashSet<&str>,
) -> std::result::Result<Plan, String> {
    let parsed: RawPlan =
        serde_json::from_value(raw.clone()).map_err(|e| format!("unparseable plan: {e}"))?;

    // The reject rule, checked before anything is applied.
    let mut seen: HashSet<&str> = HashSet::new();
    for u in &parsed.updates {
        if !seen.insert(u.observation_id.as_str()) {
            return Err(format!(
                "two updates target observation {}; the second would silently overwrite the first",
                u.observation_id
            ));
        }
    }

    // `source_fact_ids` must resolve to facts *in this batch*. A uuid we did
    // not show the model is a hallucination or a leak from another batch;
    // either way the entry's provenance is wrong, and provenance is what
    // `proof_count` and every "which facts back this?" answer are built on.
    let resolve = |ids: &[String]| -> Option<Vec<i64>> {
        ids.iter().map(|u| facts.get(u.as_str()).copied()).collect()
    };

    let mut plan = Plan::default();
    for c in parsed.creates {
        if c.reason.trim().is_empty() || c.text.trim().is_empty() {
            tracing::warn!(text = ?c.text, "dropping a create with no reason or no text");
            continue;
        }
        let Some(sources) = resolve(&c.source_fact_ids) else {
            tracing::warn!(ids = ?c.source_fact_ids, "dropping a create citing an unknown fact uuid");
            continue;
        };
        plan.creates.push((c.text.trim().to_string(), sources));
    }
    for u in parsed.updates {
        if u.reason.trim().is_empty() || u.text.trim().is_empty() {
            tracing::warn!(
                id = u.observation_id,
                "dropping an update with no reason or no text"
            );
            continue;
        }
        if !observations.contains(u.observation_id.as_str()) {
            tracing::warn!(
                id = u.observation_id,
                "dropping an update to an unpooled observation"
            );
            continue;
        }
        let Some(sources) = resolve(&u.source_fact_ids) else {
            tracing::warn!(ids = ?u.source_fact_ids, "dropping an update citing an unknown fact uuid");
            continue;
        };
        plan.updates
            .push((u.observation_id, u.text.trim().to_string(), sources));
    }
    let updated: HashSet<&str> = plan.updates.iter().map(|(u, ..)| u.as_str()).collect();
    for d in parsed.deletes {
        if d.reason.trim().is_empty() {
            tracing::warn!(id = d.observation_id, "dropping a delete with no reason");
            continue;
        }
        if !observations.contains(d.observation_id.as_str()) {
            tracing::warn!(
                id = d.observation_id,
                "dropping a delete of an unpooled observation"
            );
            continue;
        }
        // Rule 7 is "be very conservative with deletes"; deleting something
        // the same reply just rewrote is the least conservative reading of a
        // self-contradictory plan.
        if updated.contains(d.observation_id.as_str()) {
            tracing::warn!(
                id = d.observation_id,
                "dropping a delete of an observation this plan also updates"
            );
            continue;
        }
        plan.deletes.push(d.observation_id.clone());
    }
    Ok(plan)
}

/// `maxLength` on every free-text field, same reasoning as CE-9a's
/// `decision_schema`: the reply budget is shared, and `reason` is diagnostic
/// only — nothing reads it — so it must not be able to eat the budget `text`
/// needs. `required` carries the prompt's "every entry must include a
/// `reason`" (`prompts.py:94`) down to the grammar.
fn plan_schema() -> Value {
    let text = json!({"type": "string", "maxLength": 2000});
    let reason = json!({"type": "string", "maxLength": 500});
    let ids = json!({"type": "array", "items": {"type": "string"}});
    json!({
        "type": "object",
        "properties": {
            "creates": {"type": "array", "items": {
                "type": "object",
                "properties": {"text": text, "source_fact_ids": ids, "reason": reason},
                "required": ["text", "source_fact_ids", "reason"],
            }},
            "updates": {"type": "array", "items": {
                "type": "object",
                "properties": {
                    "text": text,
                    "observation_id": {"type": "string"},
                    "source_fact_ids": ids,
                    "reason": reason,
                },
                "required": ["text", "observation_id", "source_fact_ids", "reason"],
            }},
            "deletes": {"type": "array", "items": {
                "type": "object",
                "properties": {"observation_id": {"type": "string"}, "reason": reason},
                "required": ["observation_id", "reason"],
            }},
        },
        "required": ["creates", "updates", "deletes"],
    })
}

/// One observation's vector, on its own trip through the ONNX mutex.
async fn embed_one(state: &AppState, text: &str) -> Result<Vec<f32>> {
    let embedder = state
        .embedder
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| {
            Error::Storage("consolidation needs the embedding model; it is not loaded".to_string())
        })?;
    let text = text.to_string();
    let mut vectors = tokio::task::spawn_blocking(move || embedder.embed_batch(&[text]))
        .await
        .map_err(|e| Error::Storage(format!("task join error: {e}")))?
        .map_err(|e| Error::Storage(format!("observation embedding failed: {e}")))?;
    vectors
        .pop()
        .ok_or_else(|| Error::Storage("embedder returned no vector".to_string()))
}

/// Every rusqlite call off the async runtime, per the workspace rule.
async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Storage(format!("task join error: {e}")))?
}

/// Background task: one tick every `consolidation.interval_secs`
/// (`DEFAULT_CONSOLIDATION_RECONCILE_INTERVAL_SECONDS` = 300,
/// `config.py:1298`). `0` disables it, as it does in legacy.
pub async fn run_task(state: AppState) {
    let secs = state.cfg.consolidation.interval_secs;
    if secs == 0 {
        tracing::info!("consolidation background task disabled (interval_secs = 0)");
        return;
    }
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(secs));
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = ticker.tick() => tick_once(&state).await,
            _ = &mut shutdown => break,
        }
    }
}

/// One tick over every bank.
///
/// **The tick is skipped entirely while a retain job is queued or in flight**
/// (CE-9a handoff #4, the highest-value structural guard it named).
/// Consolidation has no latency SLO and a 300 s interval; retain is the hot
/// ingest path, and the two contend on exactly the same two resources — the
/// single ONNX mutex and the SQLite write lock. Deferring five minutes
/// *removes* the contention window instead of shrinking it, and it does so
/// without a single fine-grained guard anywhere else in this module.
///
/// `retain::queued_bytes()` is the existing admission counter: non-zero from
/// the moment a transcript is accepted until its job finishes.
async fn tick_once(state: &AppState) {
    if crate::retain::queued_bytes() > 0 {
        tracing::debug!("retain in flight; deferring the consolidation tick");
        return;
    }
    // No embedder, no round. A `creates` entry needs a vector synchronously
    // (CE-9a's R3), so without one the round burns real GPU on an LLM call
    // whose plan it then cannot apply. Checked here rather than in
    // `run_round`, which stays usable for an update-only plan.
    if state
        .embedder
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_none()
    {
        tracing::debug!("embedding model not ready; deferring the consolidation tick");
        return;
    }
    let db = state.db.clone();
    let banks = match blocking(move || banks::list(&db)).await {
        Ok(banks) => banks,
        Err(e) => {
            tracing::warn!(error = %e, "consolidation tick could not list banks");
            return;
        }
    };
    for bank in banks {
        // Re-checked per bank, not just per tick: a retain can arrive while
        // an earlier bank's round is running, and the remaining banks have no
        // reason to keep contending with it.
        if crate::retain::queued_bytes() > 0 {
            tracing::debug!("retain arrived mid-tick; deferring the remaining banks");
            return;
        }
        match run_round(state, &bank.bank_id).await {
            Ok(summary) if summary.run_id.is_some() => {
                tracing::info!(bank = %bank.bank_id, ?summary, "consolidation round finished");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(bank = %bank.bank_id, error = %e, "consolidation round failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts_map<'a>(pairs: &'a [(&'a str, i64)]) -> HashMap<&'a str, i64> {
        pairs.iter().copied().collect()
    }

    fn obs_set<'a>(uuids: &'a [&'a str]) -> HashSet<&'a str> {
        uuids.iter().copied().collect()
    }

    fn row(id: i64, uuid: &str, text: &str) -> store::FactRow {
        store::FactRow {
            id,
            uuid: uuid.to_string(),
            text: text.to_string(),
            occurred_start: None,
            occurred_end: None,
            mentioned_at: None,
        }
    }

    // --- The plan contract ------------------------------------------------

    #[test]
    fn a_well_formed_plan_resolves_every_uuid() {
        let facts = facts_map(&[("f1", 1), ("f2", 2)]);
        let obs = obs_set(&["o1"]);
        let raw = json!({
            "creates": [{"text": "new obs", "source_fact_ids": ["f2"], "reason": "distinct facet"}],
            "updates": [{"text": "merged", "observation_id": "o1", "source_fact_ids": ["f1"], "reason": "same decision"}],
            "deletes": [],
        });

        let plan = validate(&raw, &facts, &obs).unwrap();

        assert_eq!(plan.creates, vec![("new obs".to_string(), vec![2])]);
        assert_eq!(
            plan.updates,
            vec![("o1".to_string(), "merged".to_string(), vec![1])]
        );
        assert!(plan.deletes.is_empty());
    }

    /// The plan's named test: two updates to one observation must **reject
    /// the batch**, not silently overwrite.
    #[test]
    fn duplicate_observation_ids_in_updates_reject_the_whole_batch() {
        let facts = facts_map(&[("f1", 1), ("f2", 2)]);
        let obs = obs_set(&["o1"]);
        let raw = json!({
            "creates": [{"text": "would have been created", "source_fact_ids": ["f1"], "reason": "r"}],
            "updates": [
                {"text": "first", "observation_id": "o1", "source_fact_ids": ["f1"], "reason": "r"},
                {"text": "second", "observation_id": "o1", "source_fact_ids": ["f2"], "reason": "r"},
            ],
            "deletes": [],
        });

        let err = validate(&raw, &facts, &obs).unwrap_err();

        assert!(err.contains("two updates target observation o1"), "{err}");
        // Nothing survives — not even the perfectly good create. The whole
        // reply is distrusted, and the caller retries it.
    }

    /// The plan's other named test: an unknown `source_fact_ids` uuid drops
    /// that entry and the run continues.
    #[test]
    fn an_unknown_source_fact_uuid_drops_only_that_entry() {
        let facts = facts_map(&[("f1", 1)]);
        let obs = obs_set(&["o1"]);
        let raw = json!({
            "creates": [
                {"text": "hallucinated provenance", "source_fact_ids": ["f1", "cafebabe-0000-0000-0000-000000000000"], "reason": "r"},
                {"text": "good one", "source_fact_ids": ["f1"], "reason": "r"},
            ],
            "updates": [{"text": "bad", "observation_id": "o1", "source_fact_ids": ["not-a-fact"], "reason": "r"}],
            "deletes": [],
        });

        let plan = validate(&raw, &facts, &obs).unwrap();

        assert_eq!(plan.creates, vec![("good one".to_string(), vec![1])]);
        assert!(
            plan.updates.is_empty(),
            "the update cited a uuid we never showed it"
        );
    }

    #[test]
    fn unpooled_targets_missing_reasons_and_empty_texts_all_drop_their_entry() {
        let facts = facts_map(&[("f1", 1)]);
        let obs = obs_set(&["o1"]);
        let raw = json!({
            "creates": [
                {"text": "no reason", "source_fact_ids": ["f1"], "reason": "  "},
                {"text": "   ", "source_fact_ids": ["f1"], "reason": "blank text"},
            ],
            "updates": [{"text": "t", "observation_id": "o-unknown", "source_fact_ids": [], "reason": "r"}],
            "deletes": [
                {"observation_id": "o1", "reason": ""},
                {"observation_id": "o-unknown", "reason": "r"},
            ],
        });

        let plan = validate(&raw, &facts, &obs).unwrap();

        assert_eq!(plan, Plan::default(), "every entry was malformed");
    }

    /// Rule 7 is "be very conservative with deletes". A reply that updates an
    /// observation and deletes it in the same breath keeps the update.
    #[test]
    fn a_delete_of_an_observation_the_same_plan_updates_is_dropped() {
        let facts = facts_map(&[("f1", 1)]);
        let obs = obs_set(&["o1"]);
        let raw = json!({
            "creates": [],
            "updates": [{"text": "kept", "observation_id": "o1", "source_fact_ids": ["f1"], "reason": "r"}],
            "deletes": [{"observation_id": "o1", "reason": "superseded"}],
        });

        let plan = validate(&raw, &facts, &obs).unwrap();

        assert_eq!(plan.updates.len(), 1);
        assert!(plan.deletes.is_empty());
    }

    #[test]
    fn a_delete_of_a_pooled_observation_survives() {
        let plan = validate(
            &json!({"creates": [], "updates": [], "deletes": [{"observation_id": "o1", "reason": "restated identically"}]}),
            &facts_map(&[]),
            &obs_set(&["o1"]),
        )
        .unwrap();
        assert_eq!(plan.deletes, vec!["o1".to_string()]);
    }

    #[test]
    fn missing_arrays_and_junk_bodies_do_not_panic() {
        let facts = facts_map(&[]);
        let obs = obs_set(&[]);
        assert_eq!(validate(&json!({}), &facts, &obs).unwrap(), Plan::default());
        assert!(validate(&json!([1, 2, 3]), &facts, &obs).is_err());
        assert!(validate(&json!("creates"), &facts, &obs).is_err());
        assert!(validate(&json!(null), &facts, &obs).is_err());
    }

    // --- The prompt bound -------------------------------------------------

    /// The incident guard, first half: the budget must be a real
    /// discriminator over realistic batches, not a number past every input.
    #[test]
    fn the_budget_leaves_real_room_for_a_full_batch() {
        let system = token_count(&prompts::build_consolidation_system_prompt());
        assert!(
            system < CONSOLIDATION_PROMPT_MAX_TOKENS,
            "the system prompt alone ({system}) must fit the budget"
        );

        // A full 8-fact batch of ordinary engineering facts, with the
        // observation pool at its own 512-token ceiling, has to fit
        // comfortably — otherwise the guard trips on every round.
        let facts: Vec<store::FactRow> = (0..8)
            .map(|i| {
                row(
                    i,
                    &format!("fact-uuid-{i}"),
                    "The retain worker was changed to commit one chunk per BEGIN IMMEDIATE \
                     transaction after the 102MB transcript blew the wall clock.",
                )
            })
            .collect();
        let (_, user, kept) = assemble("", &facts, vec![]).expect("a normal batch must fit");
        assert!(kept.is_empty());
        let total = system + token_count(&user);
        assert!(
            total < CONSOLIDATION_PROMPT_MAX_TOKENS,
            "a normal batch is {total} tokens against a {CONSOLIDATION_PROMPT_MAX_TOKENS} budget"
        );
        assert!(
            CONSOLIDATION_PROMPT_MAX_TOKENS - total > 1000,
            "only {} tokens of headroom on a normal batch",
            CONSOLIDATION_PROMPT_MAX_TOKENS - total
        );

        // ...and prompt + reply must fit the window this module requests.
        assert!(
            CONSOLIDATION_PROMPT_MAX_TOKENS + u64::from(CONSOLIDATION_REPLY_MAX_TOKENS)
                < u64::from(CONSOLIDATION_NUM_CTX),
            "prompt + reply must not be able to exhaust num_ctx"
        );
    }

    /// The incident guard, second half: an oversized batch is **bisected**,
    /// and a single oversized fact is dropped — never truncated, never sent.
    /// Deleting or inflating the `const` fails this test.
    #[test]
    fn an_over_budget_batch_bisects_and_a_lone_over_budget_fact_is_refused() {
        let huge = "duplicated observation text ".repeat(20_000); // ~100k tokens
        let facts = vec![row(1, "f1", "short"), row(2, "f2", &huge)];

        assert!(
            assemble("", &facts, vec![]).is_none(),
            "an over-budget batch must refuse to render, so the caller bisects"
        );
        assert!(
            assemble("", &facts[1..], vec![]).is_none(),
            "one fact over the budget on its own is still refused"
        );
        // ...and its neighbour, alone, is fine — proving the refusal is about
        // size and not about the batch existing at all.
        assert!(assemble("", &facts[..1], vec![]).is_some());
    }

    /// The observation pool sheds from the tail, lowest-ranked first, and the
    /// prompt that survives is under budget.
    #[test]
    fn the_pool_is_shed_from_the_tail_until_the_prompt_fits() {
        let big = "existing observation text ".repeat(4_000); // ~20k tokens each
        let pool: Vec<CandidateRow> = (0..3)
            .map(|i| CandidateRow {
                id: i,
                uuid: format!("o{i}"),
                fact_type: FactType::Observation,
                text: if i == 0 {
                    "the nearest twin".to_string()
                } else {
                    big.clone()
                },
                context: None,
                occurred_start: None,
                occurred_end: None,
                mentioned_at: None,
                tags: vec![],
                proof_count: 1,
            })
            .collect();

        let (system, user, kept) =
            assemble("", &[row(1, "f1", "short fact")], pool).expect("the head must survive");

        assert_eq!(
            kept.iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![0],
            "the nearest twin is the last thing shed"
        );
        assert!(token_count(&system) + token_count(&user) <= CONSOLIDATION_PROMPT_MAX_TOKENS);
    }
}
