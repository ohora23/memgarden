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
//! | Source facts | the incident's actual multiplier: capped by [`SOURCE_FACTS_MAX_TOKENS_PER_OBSERVATION`] **and** by a whole-pool [`SOURCE_FACTS_MAX_TOKENS`], both `const` — see [`prompts::observation_entry`] |
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

/// Ceiling on `deletes` in one LLM plan. Above it the batch is rejected.
///
/// Rule 7 (`prompts.py:34`) says observations recording significant events are
/// history and deletes are exceptional — but that is *prompt* text, and the
/// observation pool is chosen by recall over the attacker's own fact text, so
/// one landed fact can both pull a target into the pool and argue for its
/// deletion. The watermark has already passed those facts by the time anyone
/// notices, so the deletion is unrecoverable. This is the structural backstop:
/// a plan asking to delete three observations at once has stopped following
/// rule 7 whatever its `reason` fields say, and a rejected batch costs one
/// batch.
pub const MAX_DELETES_PER_PLAN: usize = 2;

/// Per-observation ceiling on embedded source-fact text, in cl100k tokens —
/// legacy's `DEFAULT_CONSOLIDATION_SOURCE_FACTS_MAX_TOKENS_PER_OBSERVATION`
/// (`config.py:1171`), here a `const` rather than config.
pub const SOURCE_FACTS_MAX_TOKENS_PER_OBSERVATION: u64 = 256;

/// Whole-pool ceiling on embedded source-fact text, in cl100k tokens.
///
/// Legacy's equivalent is 4096 (`config.py:1169`), sized for a hosted model's
/// window; 1536 is the same knob against this module's 8192. **This is the
/// exact term that caused the 2026-08-02 incident** — a per-fact source-facts
/// budget multiplied by the LLM batch size — so it is a `const`, it is a
/// *pool* total rather than a per-fact budget that multiplies, and
/// `CONSOLIDATION_PROMPT_MAX_TOKENS` still sheds whole observations behind it.
/// Three layers where legacy had one, and none of them is an env var.
pub const SOURCE_FACTS_MAX_TOKENS: u64 = 1536;

/// One pooled observation and the source facts that back it, kept together so
/// that shedding an observation for the prompt budget sheds its sources too.
#[derive(Debug, Clone)]
pub struct PooledObservation {
    pub row: CandidateRow,
    pub sources: Vec<CandidateRow>,
}

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
    /// The round stopped early because a retain job arrived. Everything up to
    /// `watermark` is committed; the next tick resumes from there.
    pub deferred: bool,
    pub watermark: i64,
}

/// Holds a bank's consolidation slot for as long as it is alive.
struct BankGuard {
    set: std::sync::Arc<std::sync::Mutex<HashSet<String>>>,
    bank: String,
}

impl Drop for BankGuard {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.bank);
    }
}

/// Claims `bank_id`'s consolidation slot, or `None` if a round is already
/// running on it. Released on drop — including on the `?` paths inside
/// [`run_round`], on panic, and when the caller's timeout drops the future.
fn claim(state: &AppState, bank_id: &str) -> Option<BankGuard> {
    let set = state.consolidating.clone();
    let claimed = set
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(bank_id.to_string());
    claimed.then(|| BankGuard {
        set,
        bank: bank_id.to_string(),
    })
}

/// Runs one consolidation round for `bank_id`.
///
/// Cheap and side-effect-free when there is nothing new: one indexed range
/// scan, no run row, no LLM call.
///
/// **One round per bank at a time**, enforced here rather than at the route so
/// the background tick and a manual `POST` cannot race each other. The
/// watermark read, the fact selection and `start_run` are three separate
/// transactions; two overlapping rounds would read the same watermark, select
/// the same facts and both apply plans, and the advanced watermark then
/// guarantees the duplicates are never revisited. A second caller gets
/// `Conflict` (HTTP 409).
pub async fn run_round(state: &AppState, bank_id: &str) -> Result<RoundSummary> {
    let Some(_guard) = claim(state, bank_id) else {
        return Err(Error::Conflict(format!(
            "a consolidation round is already running for bank {bank_id}"
        )));
    };
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
        // The retain guard, rechecked per batch. Checking only at tick entry
        // leaves a retain arriving mid-round waiting behind up to a full
        // Ollama deadline of held permit; a round has no latency SLO and
        // resumes from the watermark on the next tick, so yielding here costs
        // nothing but one interval.
        if crate::retain::queued_bytes() > 0 {
            tracing::debug!(
                bank = bank_id,
                "retain arrived mid-round; deferring the rest"
            );
            summary.deferred = true;
            break;
        }
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
            // Both bisect: one because the prompt is too big, one because the
            // model's answer was refused. A single fact cannot be halved — an
            // oversized one is dropped inside `process_batch`, a rejected one
            // is counted and abandoned just below — so the worst case is ONE
            // fact, not a whole batch.
            Ok(BatchOutcome::Split) | Ok(BatchOutcome::Rejected) if end - start > 1 => {
                let mid = start + (end - start) / 2;
                queue.push_front((mid, end));
                queue.push_front((start, mid));
                continue;
            }
            Ok(BatchOutcome::Rejected) => {
                tracing::warn!(
                    fact_id = facts[start].id,
                    "a single fact was rejected on every attempt; abandoning it"
                );
                summary.skipped_batches += 1;
            }
            Ok(BatchOutcome::Split) => unreachable!("assemble refuses a lone fact with Dropped"),
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

    // **A `done` round that abandoned work must not look like a clean one.**
    // Without this, an Ollama outage across a whole round writes
    // `status='done', facts_seen=50, created=0, updated=0, deleted=0,
    // watermark=50` — byte-identical to "the model read 50 facts and correctly
    // found nothing durable" — with the watermark past all 50. The `error`
    // column already exists and is already surfaced by
    // `GET /v1/banks/{id}/consolidation`, so this needs no migration.
    let note =
        (summary.skipped_batches > 0 || summary.dropped_facts > 0 || summary.deferred).then(|| {
            format!(
                "{} batch(es) abandoned, {} fact(s) dropped for size{}",
                summary.skipped_batches,
                summary.dropped_facts,
                if summary.deferred {
                    ", round deferred mid-way for a retain job"
                } else {
                    ""
                }
            )
        });
    let (db, counts, wm) = (db.clone(), summary.counts(), summary.watermark);
    blocking(move || store::finish_run(&db, run_id, "done", counts, Some(wm), note.as_deref()))
        .await?;
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
    /// Every attempt reached Ollama and every reply was **refused** —
    /// unparseable, or a plan this module rejected. Content-dependent and so
    /// deterministic under retry (identical prompt, temperature 0.1), which
    /// means retrying the same bytes is not the answer: **bisect**, exactly as
    /// legacy does on a failed batch (`consolidator.py:1175` — "halve the
    /// sub-batch and retry, down to batch_size=1"), and give up only on a
    /// single fact.
    ///
    /// Without this a rejected 8-fact batch put all 8 facts under a committed
    /// watermark and `unconsolidated`'s `id > ?` never returned them again.
    /// Legacy loses **one** row to a `consolidation_failed_at` stamp because
    /// it tracks per row; a monotone rowid watermark turns every skip into an
    /// irreversible eight.
    Rejected,
    /// At least one attempt could not reach Ollama at all. Deliberately **not**
    /// bisected: a down or hung server fails the halves too, and 15
    /// sub-batches x `max_attempts` is 45 pointless calls (and 45 client
    /// deadlines) per batch. Counted in `skipped_batches` and surfaced on the
    /// ledger row.
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
    // `assemble` BPE-encodes the whole prompt once per shed iteration, which
    // is exactly the CPU-bound work CE-9a moved off the reactor and recorded
    // as security LOW 3. It runs in a blocking closure for the same reason.
    let assembled = {
        let (mission, facts) = (mission.to_string(), facts.to_vec());
        blocking(move || Ok(assemble(&mission, &facts, pool))).await?
    };
    let Some((system, user, pool)) = assembled else {
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

    // Fact uuids resolve to rowids **here**, before the LLM call, unlike the
    // observation side. That is safe only because no production path deletes a
    // non-observation node: `apply_plan`'s deletes are `fact_type =
    // 'observation'` only, and nothing else removes facts except a bank or
    // document cascade, which takes the observations with it. **If you add a
    // fact-deletion endpoint, move this resolution inside `apply_plan`'s
    // transaction** — a recycled fact rowid would attach the wrong provenance
    // to a real observation, which no later check would catch.
    let by_fact_uuid: HashMap<&str, i64> = facts.iter().map(|f| (f.uuid.as_str(), f.id)).collect();
    // Observations stay uuids all the way to the write: `apply_plan` resolves
    // them itself, so a rowid recycled between the pooling recall and the
    // write cannot be mistaken for its previous occupant.
    let pooled_uuids: HashSet<&str> = pool.iter().map(|o| o.row.uuid.as_str()).collect();

    // `consolidation_max_attempts` (`config.py:1147`) is the OUTER loop: the
    // client already retries transport and parse failures internally
    // (`ollama.max_retries`). This retries a *semantically* invalid plan —
    // two updates to one observation — which no transport retry would fix.
    let mut plan = None;
    // Distinguishes "the server answered and we refused the answer" from "we
    // could not reach the server" — opposite recovery strategies, see
    // [`BatchOutcome::Rejected`].
    let mut transport_failed = false;
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
                transport_failed = true;
                tracing::warn!(attempt, max = cfg.max_attempts, error = %e, "consolidation batch call failed")
            }
        }
    }
    let Some(plan) = plan else {
        return Ok(if transport_failed {
            BatchOutcome::Skipped
        } else {
            BatchOutcome::Rejected
        });
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

    // Every observation this plan touched is a dedup candidate, and each needs
    // a current vector to probe with.
    //
    // **Updates are in the list, which is CE-9a's handoff #11.**
    // `update_text_tx` nulls the embedding (R4), so a rewritten observation
    // drops out of `observation_vectors` — the probe requires
    // `embedding IS NOT NULL` — until the backlog catches up. Rule 1 makes
    // UPDATE the *common* path, so without the synchronous re-embed below the
    // majority of a round's writes are invisible to the dedup that runs
    // milliseconds later in the same round, and an observation updated by this
    // plan is invisible to a create in the same plan. Re-embedding also hands
    // `dedup_created` the vector it needs to probe the rewritten text — which
    // is precisely when an observation is most likely to have become a
    // near-duplicate of a sibling. `select_twin` excludes the anchor's own id;
    // that `exclude_id` parameter is what handoff #11 named.
    //
    // **Creates come first**, so they get the capped budget ahead of updates:
    // a create has never been probed at all, while an update was probed when
    // it was created.
    let mut candidates: Vec<(i64, String, Vec<f32>)> = created_ids
        .created
        .iter()
        .zip(plan.creates.iter().zip(&vectors))
        .map(|(id, ((text, _), embedding))| (*id, text.clone(), embedding.clone()))
        .collect();
    let update_texts: HashMap<&str, &str> = plan
        .updates
        .iter()
        .map(|(uuid, text, _)| (uuid.as_str(), text.as_str()))
        .collect();
    for (updated_id, uuid) in &created_ids.updated {
        let Some(text) = update_texts.get(uuid.as_str()).copied() else {
            continue;
        };
        // Best effort, like `reembed_merged_twin` and for the same reason: the
        // update is already committed and durable. A failure here costs
        // exactly what CE-9a shipped — the node waits for a backlog tick and
        // skips this round's dedup — whereas failing the round would abandon
        // the remaining batches over a recoverable miss. It also keeps an
        // update-only round working with no embedder at all.
        let embedding = match embed_one(state, text).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(id = updated_id, error = %e, "updated observation left on the embed backlog");
                continue;
            }
        };
        let (db, bank, id, v) = (
            state.db.clone(),
            bank_id.to_string(),
            *updated_id,
            embedding.clone(),
        );
        if let Err(e) =
            blocking(move || memgarden_store::nodes::set_embedding(&db, id, &bank, &v)).await
        {
            tracing::warn!(id = updated_id, error = %e, "updated observation left on the embed backlog");
            continue;
        }
        candidates.push((*updated_id, text.to_string(), embedding));
    }

    // CE-9a's dedup, after the write and outside it — the write lock is never
    // held across an LLM call.
    let mut merged = 0usize;
    let mut adjudications = 0usize;
    for (id, text, embedding) in &candidates {
        if *adjudication_budget == 0 {
            tracing::debug!(
                "per-round dedup adjudication cap reached; remaining writes dedup next round"
            );
            break;
        }
        let (outcome, adjudicated) =
            super::dedup_created(&state.db, &state.ollama, cfg, bank_id, *id, text, embedding)
                .await?;
        // Charged only when an LLM call actually happened. `dedup_created`
        // returns early — with no call — when dedup is disabled, and far more
        // often when nothing clears the 0.97 threshold, which is the normal
        // case on a bank of distinct observations. Charging for those would
        // let a bank exhaust its whole cap without once reaching the GPU,
        // silently skipping the probe on every later write in the round. A
        // skipped probe is permanent: dedup only ever runs on a write, never
        // as a sweep.
        if adjudicated {
            *adjudication_budget -= 1;
            adjudications += 1;
        }
        match outcome {
            super::Outcome::Merged { into, .. } => {
                merged += 1;
                reembed_merged_twin(state, bank_id, into).await;
            }
            super::Outcome::Created { .. } => {}
        }
    }

    Ok(BatchOutcome::Applied {
        created: created_ids.created.len(),
        updated: created_ids.updated.len(),
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
) -> Result<Vec<PooledObservation>> {
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
            // Legacy scoring on purpose: this path feeds consolidation and
            // reflection, not the injection, and re-ranking it is a separate
            // question from the one `semantic_alpha` was measured against.
            semantic_alpha: 0.0,
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

    let mut pooled: Vec<CandidateRow> = Vec::new();
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

    // Attach each observation's source facts, newest first, under
    // [`SOURCE_FACTS_MAX_TOKENS_PER_OBSERVATION`] and
    // [`SOURCE_FACTS_MAX_TOKENS`]. This is the anchor that stops an UPDATE
    // rewriting a summary it can no longer see the evidence for — and it is
    // also the 2026-08-02 incident's dominant term, which is why both budgets
    // are consts and why the whole-prompt bound still sheds behind them.
    //
    // // ponytail: one `sources_of` per pooled observation (at most a handful,
    // // bounded by `consolidation.max_tokens`) plus one batched hydrate.
    // // Collapse into a single join if the pool ever gets big.
    let (db, bank) = (state.db.clone(), bank_id.to_string());
    let pooled_ids: Vec<i64> = pooled.iter().map(|r| r.id).collect();
    let with_sources = blocking(move || {
        let mut per_obs: Vec<(i64, Vec<i64>)> = Vec::with_capacity(pooled_ids.len());
        let mut all: Vec<i64> = Vec::new();
        for id in pooled_ids {
            let mut src = memgarden_store::consolidate::sources_of(&db, id)?;
            src.reverse(); // newest first — `sources_of` is ascending by id
            all.extend(src.iter().copied());
            per_obs.push((id, src));
        }
        all.sort_unstable();
        all.dedup();
        let rows = search::hydrate(&db, &bank, &all)?;
        let by_id: HashMap<i64, CandidateRow> = rows.into_iter().map(|r| (r.id, r)).collect();
        Ok(per_obs
            .into_iter()
            .map(|(obs, src)| {
                (
                    obs,
                    src.into_iter()
                        .filter_map(|id| by_id.get(&id).cloned())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<i64, Vec<CandidateRow>>>())
    })
    .await?;

    let mut total = 0u64;
    Ok(pooled
        .into_iter()
        .map(|row| {
            let mut kept = Vec::new();
            let mut per = 0u64;
            for src in with_sources.get(&row.id).into_iter().flatten() {
                let cost = token_count(&src.text);
                if per + cost > SOURCE_FACTS_MAX_TOKENS_PER_OBSERVATION
                    || total + cost > SOURCE_FACTS_MAX_TOKENS
                {
                    break;
                }
                per += cost;
                total += cost;
                kept.push(src.clone());
            }
            PooledObservation { row, sources: kept }
        })
        .collect())
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
    mut pool: Vec<PooledObservation>,
) -> Option<(String, String, Vec<PooledObservation>)> {
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
                let sources: Vec<prompts::SourceFact> = o
                    .sources
                    .iter()
                    .map(|s| prompts::SourceFact {
                        text: &s.text,
                        context: s.context.as_deref(),
                        occurred_start: s.occurred_start,
                        occurred_end: s.occurred_end,
                        mentioned_at: s.mentioned_at,
                    })
                    .collect();
                prompts::observation_entry(
                    &o.row.uuid,
                    &o.row.text,
                    o.row.proof_count,
                    o.row.occurred_start,
                    o.row.occurred_end,
                    o.row.mentioned_at,
                    &sources,
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

    // The reject rules, checked before anything is applied. Both messages use
    // `{:?}` because they reach `finish_run`'s `error` column and the status
    // response, and the uuid in them is LLM-controlled text.
    let mut seen: HashSet<&str> = HashSet::new();
    for u in &parsed.updates {
        if !seen.insert(u.observation_id.as_str()) {
            return Err(format!(
                "two updates target observation {:?}; the second would silently overwrite the first",
                u.observation_id
            ));
        }
    }
    // Rule 7's structural backstop. Counted on the *raw* list, before the
    // per-entry drops below: a plan that asked for six deletes and happens to
    // have four dropped for unpooled ids is still a plan that stopped being
    // conservative.
    if parsed.deletes.len() > MAX_DELETES_PER_PLAN {
        return Err(format!(
            "plan asks to delete {} observations (max {MAX_DELETES_PER_PLAN}); \
             rule 7 makes deletes exceptional",
            parsed.deletes.len()
        ));
    }

    // `source_fact_ids` must resolve to facts *in this batch*. A uuid we did
    // not show the model is a hallucination or a leak from another batch;
    // either way the entry's provenance is wrong, and provenance is what
    // `proof_count` and every "which facts back this?" answer are built on.
    // Empty is a failure too, not "no sources": an observation with no
    // provenance has `proof_count = 0`, gives an operator nothing to audit,
    // and is indistinguishable from a hallucination. Same principle as an
    // unknown uuid — the entry's provenance is wrong.
    let resolve = |ids: &[String]| -> Option<Vec<i64>> {
        if ids.is_empty() {
            return None;
        }
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
                id = ?u.observation_id,
                "dropping an update with no reason or no text"
            );
            continue;
        }
        if !observations.contains(u.observation_id.as_str()) {
            tracing::warn!(
                id = ?u.observation_id,
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
            tracing::warn!(id = ?d.observation_id, "dropping a delete with no reason");
            continue;
        }
        if !observations.contains(d.observation_id.as_str()) {
            tracing::warn!(
                id = ?d.observation_id,
                "dropping a delete of an unpooled observation"
            );
            continue;
        }
        // Rule 7 is "be very conservative with deletes"; deleting something
        // the same reply just rewrote is the least conservative reading of a
        // self-contradictory plan.
        if updated.contains(d.observation_id.as_str()) {
            tracing::warn!(
                id = ?d.observation_id,
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
    // **Delay, not tokio's default Burst.** With Burst, a round that outruns
    // `interval_secs` makes every missed tick fire immediately afterwards, so
    // rounds run back to back — the 300s spacing evaporates exactly when
    // rounds are slow, which is exactly when it is load-bearing. Delay
    // re-bases the schedule on when the round actually finished.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = ticker.tick() => tick_once(&state, secs).await,
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
async fn tick_once(state: &AppState, interval_secs: u64) {
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
        // **A per-round deadline.** One bounded call can burn the client's
        // whole `TOTAL_DEADLINE_CAP` (600s), times `max_attempts`, times the
        // batch count: hours per round against a hung Ollama, with the bank's
        // slot held throughout. Abandoning is safe by construction — the
        // round's `running` ledger row keeps a NULL watermark, and
        // `store::watermark` is a `MAX` over rows that recorded one, so an
        // abandoned round contributes nothing and its facts are simply
        // re-selected next tick. Dropping the future releases the bank slot;
        // `fail_stale_runs` closes the orphaned row at next startup.
        let deadline = std::time::Duration::from_secs(interval_secs.saturating_mul(2));
        match tokio::time::timeout(deadline, run_round(state, &bank.bank_id)).await {
            Ok(Ok(summary)) if summary.run_id.is_some() => {
                tracing::info!(bank = %bank.bank_id, ?summary, "consolidation round finished");
                // The observations this round produced are exactly the material
                // a mental model is synthesised from, and this is the one
                // moment they appear. Models whose trigger is
                // `@after-consolidation` are woken here rather than by a clock
                // that would only ever be asking whether this had happened.
                //
                // Guarded on `run_id.is_some()`: a tick that found nothing to
                // consolidate produced nothing to refresh from.
                crate::mental::cron::refresh_after_consolidation(state, &bank.bank_id).await;
            }
            Ok(Ok(_)) => {}
            // A manual POST holds the slot; the next tick picks the bank up.
            Ok(Err(Error::Conflict(_))) => {
                tracing::debug!(bank = %bank.bank_id, "a round is already running; skipping")
            }
            Ok(Err(e)) => {
                tracing::warn!(bank = %bank.bank_id, error = %e, "consolidation round failed")
            }
            Err(_) => tracing::warn!(
                bank = %bank.bank_id,
                secs = deadline.as_secs(),
                "consolidation round exceeded its deadline and was abandoned"
            ),
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

    fn obs_row(id: i64, uuid: &str, text: &str) -> CandidateRow {
        CandidateRow {
            id,
            uuid: uuid.to_string(),
            fact_type: FactType::Observation,
            text: text.to_string(),
            context: None,
            occurred_start: None,
            occurred_end: None,
            mentioned_at: None,
            tags: vec![],
            proof_count: 1,
            sources: vec![],
        }
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

        // `{:?}` on the uuid: the message reaches the ledger's `error` column
        // and the status response, and the uuid is LLM-controlled text.
        assert!(
            err.contains(r#"two updates target observation "o1""#),
            "{err}"
        );
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

    /// Rule 7's structural backstop. Two deletes are allowed; three are a plan
    /// that stopped being conservative, and the whole batch is refused —
    /// deleting under a watermark that has already passed is unrecoverable.
    #[test]
    fn a_plan_asking_for_too_many_deletes_is_rejected_whole() {
        let facts = facts_map(&[("f1", 1)]);
        let obs = obs_set(&["o1", "o2", "o3"]);
        let delete = |id: &str| json!({"observation_id": id, "reason": "superseded"});

        // At the cap: fine.
        let ok = validate(
            &json!({"creates": [], "updates": [], "deletes": [delete("o1"), delete("o2")]}),
            &facts,
            &obs,
        )
        .unwrap();
        assert_eq!(ok.deletes.len(), 2);

        // One over: the batch dies, including any creates it carried.
        let err = validate(
            &json!({
                "creates": [{"text": "kept?", "source_fact_ids": ["f1"], "reason": "r"}],
                "updates": [],
                "deletes": [delete("o1"), delete("o2"), delete("o3")],
            }),
            &facts,
            &obs,
        )
        .unwrap_err();
        assert!(err.contains("delete 3 observations"), "{err}");

        // Counted on the RAW list: four deletes that would mostly be dropped
        // for unpooled ids is still a plan that asked for four.
        assert!(
            validate(
                &json!({"creates": [], "updates": [], "deletes": [
                    delete("o1"), delete("nope-1"), delete("nope-2"), delete("nope-3"),
                ]}),
                &facts,
                &obs,
            )
            .is_err()
        );
    }

    /// An entry with no provenance at all writes an observation with
    /// `proof_count = 0` — nothing to audit, indistinguishable from a
    /// hallucination. Same treatment as an unknown uuid: drop the entry.
    #[test]
    fn an_entry_citing_no_source_facts_is_dropped() {
        let facts = facts_map(&[("f1", 1)]);
        let obs = obs_set(&["o1"]);

        let plan = validate(
            &json!({
                "creates": [
                    {"text": "unevidenced", "source_fact_ids": [], "reason": "r"},
                    {"text": "evidenced", "source_fact_ids": ["f1"], "reason": "r"},
                ],
                "updates": [{"text": "t", "observation_id": "o1", "source_fact_ids": [], "reason": "r"}],
                "deletes": [],
            }),
            &facts,
            &obs,
        )
        .unwrap();

        assert_eq!(plan.creates, vec![("evidenced".to_string(), vec![1])]);
        assert!(plan.updates.is_empty());
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

    /// Source facts reach the assembled prompt — the anchor against the
    /// summary drift the first live round exhibited. Fails if
    /// `observation_entry` stops emitting `source_memories` or if `assemble`
    /// stops passing them.
    #[test]
    fn pooled_source_facts_reach_the_assembled_prompt() {
        let pool = vec![PooledObservation {
            row: obs_row(0, "o0", "the retain worker commits per chunk"),
            sources: vec![obs_row(
                90,
                "s0",
                "the retain worker was changed to one BEGIN IMMEDIATE per chunk",
            )],
        }];

        let (_, user, _) =
            assemble("", &[row(1, "f1", "a new retain fact")], pool).expect("must fit");

        assert!(user.contains("source_memories"), "{user}");
        assert!(
            user.contains("the retain worker was changed to one BEGIN IMMEDIATE per chunk"),
            "{user}"
        );
    }

    /// The observation pool sheds from the tail, lowest-ranked first, and the
    /// prompt that survives is under budget.
    #[test]
    fn the_pool_is_shed_from_the_tail_until_the_prompt_fits() {
        let big = "existing observation text ".repeat(4_000); // ~20k tokens each
        let pool: Vec<PooledObservation> = (0..3)
            .map(|i| PooledObservation {
                row: CandidateRow {
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
                    sources: vec![],
                },
                sources: vec![],
            })
            .collect();

        let (system, user, kept) =
            assemble("", &[row(1, "f1", "short fact")], pool).expect("the head must survive");

        assert_eq!(
            kept.iter().map(|o| o.row.id).collect::<Vec<_>>(),
            vec![0],
            "the nearest twin is the last thing shed"
        );
        assert!(token_count(&system) + token_count(&user) <= CONSOLIDATION_PROMPT_MAX_TOKENS);
    }
}
