//! Consolidation (CE-9a): observation storage with create-time semantic
//! dedup, ported from `engine/consolidation/consolidator.py:116-300`.
//!
//! A new observation is stored, then probed against the bank's existing
//! observations by cosine on its own embedding. Anything at or above
//! `consolidation.dedup_threshold` (0.97, `config.py:1157`) gets one focused
//! "merge or keep" LLM call — the model reads both texts, so a difference in
//! a number, a negation or a named entity is respected where a similarity
//! score alone would not be. `>= 1.0` disables the whole path
//! (`consolidator.py:180-182`).
//!
//! **Every failure mode defaults to `keep`** (`consolidator.py:129-147`).
//! Keeping a near-duplicate costs one redundant row; a wrong merge destroys
//! the details of two observations at once and there is nothing to restore
//! them from.
//!
//! ## The prompt is token-bounded by construction
//!
//! On 2026-08-02 the legacy system's consolidation pinned a GPU for over an
//! hour: the assembled prompt outgrew Ollama's `num_ctx`, the runner
//! truncated the input (`keep=4`, which ate the system prompt), the model
//! rambled past the client timeout, the call aborted, and the identical
//! payload was retried forever. The fix there was three config caps — i.e.
//! the bound lived in configuration, where a later edit can remove it.
//!
//! Here the bound is code. [`DEDUP_PROMPT_MAX_TOKENS`] is a `const`, every
//! prompt is measured with `retain::token_count` (the same cl100k counter
//! that bounds retain chunks) *before* the call, and an over-budget pair is
//! never sent — see [`adjudicate`] for the shed order.

use std::sync::Arc;

use serde_json::{Value, json};

use memgarden_core::config::ConsolidationConfig;
use memgarden_core::error::{Error, Result};
use memgarden_store::{Db, consolidate as store};

use crate::ollama::OllamaClient;
use crate::retain::token_count;

/// Existing observations probed by the new observation's own embedding
/// (`consolidator.py:116`). Small on purpose: only the nearest few can
/// possibly clear a 0.97 threshold.
pub const DEDUP_TOP_K: usize = 5;

/// Hard ceiling on the assembled dedup prompt (system + user), in cl100k
/// tokens. **Not configurable — see the module docs for why.**
///
/// Margin: Ollama's own default `num_ctx` is 4096 and memgardend never
/// overrides it, so 2048 leaves 2× headroom at the smallest context this
/// daemon could ever be pointed at, and 8× against the live fork daemon's
/// 16384. The reply shares that window, and `ollama.num_predict` (8192 by
/// default) is generous for a two-field JSON object, so the check is against
/// half the smallest context rather than all of it.
///
/// The pair that fills this budget is enormous by construction: the template
/// alone is ~200 tokens, leaving ~1900 for two observation texts. A
/// consolidated observation is one or two sentences.
pub const DEDUP_PROMPT_MAX_TOKENS: u64 = 2048;

/// `consolidator.py:150-171`, verbatim. `{new}` / `{existing}` are the only
/// substitutions; the doubled braces in the Python source are literal JSON
/// braces and appear singly here.
const DEDUP_PROMPT: &str = r#"You reconcile long-term memory observations. A NEW observation is about to be stored, and it is highly similar to an EXISTING one:

[NEW] {new}
[EXISTING] {existing}

Respond with ONLY one valid JSON object matching one of these shapes:

For duplicate facts:
{"action": "merge", "text": "...", "reason": "..."}

For distinct facts:
{"action": "keep", "text": "", "reason": "..."}

Do NOT use key=value lines, markdown fences, or any text outside the JSON object.

If they assert the SAME fact (wording aside), set "action" to "merge" and provide "text": a single observation that preserves EVERY detail from both. If they differ in ANY important detail — a number/quantity, a named entity or language, a negation, or a condition — set "action" to "keep" and "text" to an empty string."#;

/// Legacy sends the dedup prompt as a lone `user` message
/// (`consolidator.py:246`). `OllamaClient` always sends a system message, so
/// this path sends an empty one rather than inventing instructions legacy
/// does not have — and it means the incident's "truncation ate the system
/// prompt" failure has nothing to eat here either.
const DEDUP_SYSTEM: &str = "";

/// What `store_observation` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The observation was stored as its own node (no twin, or the LLM kept
    /// them distinct, or dedup is off).
    Created { id: i64 },
    /// The observation was folded into an existing one, which now carries
    /// the merged text and `proof_count` sources. The candidate node is gone.
    Merged {
        into: i64,
        dropped: i64,
        proof_count: i64,
    },
}

/// The LLM's verdict, after `_DedupDecision`'s normalisation
/// (`consolidator.py:119-147`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Merge { text: String },
    Keep,
}

/// Builds the prompt for one pair. Public so the token-bound test can assert
/// on exactly the bytes that go over the wire.
pub fn dedup_prompt(new: &str, existing: &str) -> String {
    DEDUP_PROMPT
        .replace("{new}", new)
        .replace("{existing}", existing)
}

/// Tokens in the assembled prompt (system + user), counted with the same
/// cl100k counter that bounds retain chunks.
pub fn prompt_tokens(new: &str, existing: &str) -> u64 {
    token_count(DEDUP_SYSTEM) + token_count(&dedup_prompt(new, existing))
}

/// Parses the LLM response into a [`Decision`], **defaulting to `Keep` on
/// anything unexpected** (`consolidator.py:129-135` for an unrecognised
/// `action`, `:138-147` for an unparseable body).
///
/// `existing` supplies the fallback merge text: legacy's
/// `decision.text.strip() or best_text` (`:257`) — a model that says "merge"
/// but hands back an empty string keeps the twin's current wording rather
/// than blanking it.
pub fn parse_decision(raw: &Value, existing: &str) -> Decision {
    let action = raw
        .get("action")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase());
    match action.as_deref() {
        Some("merge") => {
            let text = raw
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            Decision::Merge {
                text: if text.is_empty() {
                    existing.to_string()
                } else {
                    text.to_string()
                },
            }
        }
        other => {
            if other != Some("keep") {
                tracing::warn!(action = ?other, "invalid consolidation dedup action; defaulting to keep");
            }
            Decision::Keep
        }
    }
}

/// Cosine similarity. Both vectors come from the same model, so a zero
/// vector is not a real input — it returns 0.0 rather than NaN so a corrupt
/// row can never clear the threshold.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        na += f64::from(*x) * f64::from(*x);
        nb += f64::from(*y) * f64::from(*y);
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Candidates at or above `threshold`, nearest first.
fn rank_candidates(
    candidates: Vec<store::ObservationVector>,
    embedding: &[f32],
    threshold: f64,
) -> Vec<(f64, store::ObservationVector)> {
    let mut scored: Vec<(f64, store::ObservationVector)> = candidates
        .into_iter()
        .map(|c| (cosine(embedding, &c.embedding), c))
        .filter(|(sim, _)| *sim >= threshold)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(DEDUP_TOP_K);
    scored
}

/// Picks the nearest above-threshold candidate whose prompt fits the budget
/// and asks the LLM to adjudicate. Returns the twin's id and the decision, or
/// `None` when nothing was adjudicated.
///
/// **Shed order, deterministic and documented** (the CE-9 carried-over
/// obligation):
///
/// 1. Candidates are considered in descending similarity.
/// 2. A candidate whose assembled prompt would exceed
///    [`DEDUP_PROMPT_MAX_TOKENS`] is *skipped whole*, and the next-nearest is
///    tried.
/// 3. If no above-threshold candidate fits, no call is made at all and the
///    outcome is `keep`.
///
/// Neither text is ever truncated to make it fit. A merge is asked to
/// "preserve EVERY detail from both"; hand the model a text with its tail cut
/// off and it will happily synthesise a "merged" observation missing that
/// tail — silent data loss, which is exactly what the default-to-keep rule
/// exists to prevent. Shedding a whole candidate loses nothing but a
/// deduplication opportunity.
async fn adjudicate(
    ollama: &OllamaClient,
    new_text: &str,
    ranked: &[(f64, store::ObservationVector)],
) -> Option<(i64, Decision)> {
    let (_, twin) = ranked
        .iter()
        .find(|(_, c)| prompt_tokens(new_text, &c.text) <= DEDUP_PROMPT_MAX_TOKENS)
        .inspect(|(sim, c)| tracing::debug!(twin = c.id, sim = *sim, "dedup adjudicating"))?;

    let user = dedup_prompt(new_text, &twin.text);
    // Background acquire (CE-5b): consolidation is not answering an HTTP
    // request, so "Ollama is busy" is a reason to queue, not to fail. The
    // client's own total deadline bounds the wait.
    let raw: std::result::Result<Value, _> = ollama
        .chat_json_background(DEDUP_SYSTEM, &user, &decision_schema())
        .await;
    let decision = match raw {
        Ok(value) => parse_decision(&value, &twin.text),
        Err(e) => {
            // The client already retried; an unparseable-after-retries reply
            // is legacy's `_dedup_decision_from_response` ValueError branch.
            tracing::warn!(error = %e, "dedup LLM call failed; defaulting to keep");
            Decision::Keep
        }
    };
    Some((twin.id, decision))
}

fn decision_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["merge", "keep"]},
            "text": {"type": "string"},
            "reason": {"type": "string"},
        },
        "required": ["action"],
    })
}

/// Stores one observation, deduplicating it against the bank's existing
/// observations.
///
/// `embedding` is a parameter rather than something computed here: Critic
/// Revision R3 makes observation embedding **synchronous** (the deliberate
/// exception to CE-4's async-backlog rule), because the probe below reads
/// the row it just wrote. Requiring the caller to supply the vector is what
/// makes that unskippable.
pub async fn store_observation(
    db: &Arc<Db>,
    ollama: &OllamaClient,
    cfg: &ConsolidationConfig,
    bank_id: &str,
    text: &str,
    embedding: &[f32],
    source_ids: &[i64],
) -> Result<Outcome> {
    let id = {
        let (db, bank, text, embedding, sources) = (
            db.clone(),
            bank_id.to_string(),
            text.to_string(),
            embedding.to_vec(),
            source_ids.to_vec(),
        );
        blocking(move || store::insert_observation(&db, &bank, &text, &embedding, &sources)).await?
    };

    // `>= 1.0` disables the whole path (`consolidator.py:180-182`).
    if cfg.dedup_threshold >= 1.0 {
        return Ok(Outcome::Created { id });
    }

    let candidates = {
        let (db, bank) = (db.clone(), bank_id.to_string());
        blocking(move || store::observation_vectors(&db, &bank, id)).await?
    };
    let ranked = rank_candidates(candidates, embedding, cfg.dedup_threshold);
    if ranked.is_empty() {
        return Ok(Outcome::Created { id });
    }

    match adjudicate(ollama, text, &ranked).await {
        Some((twin_id, Decision::Merge { text })) => {
            let db = db.clone();
            let proof_count =
                blocking(move || store::merge_observation(&db, twin_id, id, &text)).await?;
            Ok(Outcome::Merged {
                into: twin_id,
                dropped: id,
                proof_count,
            })
        }
        _ => Ok(Outcome::Created { id }),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use memgarden_core::EMBEDDING_DIM;
    use memgarden_core::config::OllamaConfig;
    use memgarden_store::{banks, nodes};

    /// A unit vector at `angle` radians in the first two dimensions: the
    /// cosine between two of them is exactly `cos(a - b)`, which is how the
    /// threshold-boundary tests hit 0.9699 and 0.9701 on the nose.
    fn vec_at(angle: f64) -> Vec<f32> {
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[0] = angle.cos() as f32;
        v[1] = angle.sin() as f32;
        v
    }

    /// The angle whose cosine against `vec_at(0.0)` is `sim`.
    fn vec_with_similarity(sim: f64) -> Vec<f32> {
        vec_at(sim.acos())
    }

    fn cfg(threshold: f64) -> ConsolidationConfig {
        ConsolidationConfig {
            dedup_threshold: threshold,
        }
    }

    /// A stub `/api/chat` that counts calls, records the last user prompt,
    /// and answers with `body`.
    struct Stub {
        client: OllamaClient,
        calls: Arc<AtomicUsize>,
        last_user: Arc<std::sync::Mutex<String>>,
    }

    async fn stub(body: &'static str) -> Stub {
        let calls = Arc::new(AtomicUsize::new(0));
        let last_user = Arc::new(std::sync::Mutex::new(String::new()));
        let (c, u) = (calls.clone(), last_user.clone());
        let app = axum::Router::new().route(
            "/api/chat",
            axum::routing::post(move |axum::Json(req): axum::Json<Value>| {
                let (c, u) = (c.clone(), u.clone());
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let user = req["messages"][1]["content"].as_str().unwrap_or("");
                    *u.lock().unwrap() = user.to_string();
                    axum::Json(json!({ "message": { "content": body } }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let ollama = OllamaConfig {
            base_url: format!("http://{addr}"),
            model: "stub".to_string(),
            temperature: 0.1,
            num_predict: 64,
            request_timeout_secs: 10,
            max_retries: 0,
            keep_alive: "10m".to_string(),
            max_concurrent: 1,
        };
        Stub {
            client: OllamaClient::new(ollama).unwrap(),
            calls,
            last_user,
        }
    }

    fn db_with_bank() -> Arc<Db> {
        let db = Arc::new(Db::open_memory().unwrap());
        banks::create(&db, "b1", None, None).unwrap();
        db
    }

    // --- The prompt ------------------------------------------------------

    #[test]
    fn dedup_prompt_is_the_legacy_text_verbatim() {
        let p = dedup_prompt("N", "E");
        assert!(p.starts_with(
            "You reconcile long-term memory observations. A NEW observation is about to be \
             stored, and it is highly similar to an EXISTING one:\n\n[NEW] N\n[EXISTING] E\n"
        ));
        // The JSON shapes must survive as literal braces, not format holes.
        assert!(p.contains(r#"{"action": "merge", "text": "...", "reason": "..."}"#));
        assert!(p.contains(r#"{"action": "keep", "text": "", "reason": "..."}"#));
        assert!(p.contains("Do NOT use key=value lines, markdown fences,"));
        assert!(p.ends_with(r#"set "action" to "keep" and "text" to an empty string."#));
        assert!(!p.contains("{new}") && !p.contains("{existing}"));
    }

    /// The template overhead has to leave real room for two observations,
    /// otherwise the budget is a bound on nothing.
    #[test]
    fn prompt_template_overhead_leaves_room_for_two_observations() {
        let overhead = prompt_tokens("", "");
        assert!(overhead < 300, "template is {overhead} tokens");
        assert!(
            DEDUP_PROMPT_MAX_TOKENS - overhead > 1500,
            "only {} tokens left for the pair",
            DEDUP_PROMPT_MAX_TOKENS - overhead
        );
    }

    // --- Decision parsing ------------------------------------------------

    #[test]
    fn parse_decision_merge_keep_and_every_malformed_shape() {
        let merge = parse_decision(&json!({"action": "merge", "text": " both "}), "old");
        assert_eq!(
            merge,
            Decision::Merge {
                text: "both".to_string()
            }
        );
        // Casing and surrounding whitespace are normalised (`_normalize_action`).
        assert!(matches!(
            parse_decision(&json!({"action": " MERGE ", "text": "x"}), "old"),
            Decision::Merge { .. }
        ));
        // "merge" with an empty text keeps the twin's wording (`:257`).
        assert_eq!(
            parse_decision(&json!({"action": "merge", "text": "  "}), "old"),
            Decision::Merge {
                text: "old".to_string()
            }
        );
        // Everything else is `keep` — never an error, never a merge.
        for raw in [
            json!({"action": "keep", "text": ""}),
            json!({"action": "frobnicate"}),
            json!({"action": 7}),
            json!({"action": null}),
            json!({"text": "no action at all"}),
            json!({}),
            json!("merge"),
            json!([1, 2, 3]),
            json!(null),
        ] {
            assert_eq!(parse_decision(&raw, "old"), Decision::Keep, "{raw}");
        }
    }

    // --- Threshold -------------------------------------------------------

    #[tokio::test]
    async fn threshold_boundary_at_0_97() {
        for (sim, expect_call) in [(0.9699_f64, false), (0.9701_f64, true)] {
            let db = db_with_bank();
            let s = stub(r#"{"action":"keep","text":"","reason":"distinct"}"#).await;
            store::insert_observation(&db, "b1", "existing", &vec_at(0.0), &[]).unwrap();

            let out = store_observation(
                &db,
                &s.client,
                &cfg(0.97),
                "b1",
                "candidate",
                &vec_with_similarity(sim),
                &[],
            )
            .await
            .unwrap();

            assert_eq!(
                s.calls.load(Ordering::SeqCst),
                usize::from(expect_call),
                "sim {sim} should {}probe",
                if expect_call { "" } else { "not " }
            );
            assert!(matches!(out, Outcome::Created { .. }));
        }
    }

    #[tokio::test]
    async fn threshold_of_one_disables_the_whole_path() {
        let db = db_with_bank();
        let s = stub(r#"{"action":"merge","text":"merged"}"#).await;
        let existing = store::insert_observation(&db, "b1", "same", &vec_at(0.0), &[]).unwrap();

        // An exact-duplicate embedding: similarity is 1.0, so only the
        // disable rule can stop the probe.
        let out = store_observation(&db, &s.client, &cfg(1.0), "b1", "same", &vec_at(0.0), &[])
            .await
            .unwrap();

        assert_eq!(s.calls.load(Ordering::SeqCst), 0, "no probe, no LLM call");
        let Outcome::Created { id } = out else {
            panic!("expected Created, got {out:?}")
        };
        assert_ne!(id, existing, "the duplicate was stored as its own node");
    }

    // --- Merge / keep end to end -----------------------------------------

    #[tokio::test]
    async fn merge_folds_the_candidate_into_the_twin() {
        let db = db_with_bank();
        let s =
            stub(r#"{"action":"merge","text":"p95 is 20ms on CPU","reason":"same fact"}"#).await;
        let f1 = nodes::insert(
            &db,
            memgarden_store::models::NewNode::new(
                "b1",
                memgarden_core::types::FactType::World,
                "fact one",
            ),
        )
        .unwrap();
        let f2 = nodes::insert(
            &db,
            memgarden_store::models::NewNode::new(
                "b1",
                memgarden_core::types::FactType::World,
                "fact two",
            ),
        )
        .unwrap();
        let twin =
            store::insert_observation(&db, "b1", "p95 is 20ms", &vec_at(0.0), &[f1]).unwrap();

        let out = store_observation(
            &db,
            &s.client,
            &cfg(0.97),
            "b1",
            "recall p95 is 20 ms on CPU",
            &vec_with_similarity(0.999),
            &[f2],
        )
        .await
        .unwrap();

        let Outcome::Merged {
            into,
            dropped,
            proof_count,
        } = out
        else {
            panic!("expected Merged, got {out:?}")
        };
        assert_eq!(into, twin);
        assert_eq!(proof_count, 2, "sources unioned then recounted");
        assert_eq!(store::sources_of(&db, twin).unwrap(), vec![f1, f2]);
        assert_eq!(
            nodes::get(&db, twin).unwrap().unwrap().text,
            "p95 is 20ms on CPU"
        );
        assert!(nodes::get(&db, dropped).unwrap().is_none());
        // The source facts are untouched.
        assert!(nodes::get(&db, f1).unwrap().is_some());
        assert!(nodes::get(&db, f2).unwrap().is_some());
    }

    #[tokio::test]
    async fn a_malformed_llm_response_keeps_both() {
        // Valid JSON, invalid action — and a non-JSON body, which the client
        // surfaces as a hard error. Both must land on `keep`.
        for body in [
            r#"{"action":"MERGE_THEM_PLEASE","text":"x"}"#,
            "not json at all",
        ] {
            let db = db_with_bank();
            let s = stub(body).await;
            let twin = store::insert_observation(&db, "b1", "twin", &vec_at(0.0), &[]).unwrap();

            let out = store_observation(
                &db,
                &s.client,
                &cfg(0.97),
                "b1",
                "candidate",
                &vec_at(0.0),
                &[],
            )
            .await
            .unwrap();

            let Outcome::Created { id } = out else {
                panic!("{body:?} must default to keep, got {out:?}")
            };
            assert_ne!(id, twin);
            assert_eq!(nodes::get(&db, twin).unwrap().unwrap().text, "twin");
            assert!(nodes::get(&db, id).unwrap().is_some());
        }
    }

    // --- The token bound -------------------------------------------------

    /// The incident guard. An oversized pair is never sent: no call, no
    /// truncation, and the observation survives as its own node.
    #[tokio::test]
    async fn an_over_budget_pair_is_never_sent() {
        let db = db_with_bank();
        let s = stub(r#"{"action":"merge","text":"merged"}"#).await;
        // ~100k tokens of "existing", far past any context window.
        let huge = "duplicated observation text ".repeat(20_000);
        let twin = store::insert_observation(&db, "b1", &huge, &vec_at(0.0), &[]).unwrap();

        let out = store_observation(
            &db,
            &s.client,
            &cfg(0.97),
            "b1",
            "short candidate",
            &vec_at(0.0),
            &[],
        )
        .await
        .unwrap();

        assert_eq!(s.calls.load(Ordering::SeqCst), 0, "over-budget pair sent!");
        assert!(matches!(out, Outcome::Created { .. }));
        assert_eq!(
            nodes::get(&db, twin).unwrap().unwrap().text.len(),
            huge.len(),
            "the twin was not rewritten"
        );
    }

    /// The shed order, and the proof that the guard is not simply "never
    /// call": with an over-budget twin at similarity 1.0 and a fitting
    /// candidate just below it, the fitting one is adjudicated — and the
    /// prompt that actually went over the wire is under budget.
    #[tokio::test]
    async fn an_over_budget_candidate_is_shed_in_favour_of_the_next_nearest() {
        let db = db_with_bank();
        let s = stub(r#"{"action":"merge","text":"merged"}"#).await;
        let huge = "duplicated observation text ".repeat(20_000);
        store::insert_observation(&db, "b1", &huge, &vec_at(0.0), &[]).unwrap();
        let fits =
            store::insert_observation(&db, "b1", "short twin", &vec_with_similarity(0.999), &[])
                .unwrap();

        let out = store_observation(
            &db,
            &s.client,
            &cfg(0.97),
            "b1",
            "short candidate",
            &vec_at(0.0),
            &[],
        )
        .await
        .unwrap();

        assert_eq!(s.calls.load(Ordering::SeqCst), 1);
        assert!(
            matches!(out, Outcome::Merged { into, .. } if into == fits),
            "expected a merge into the fitting twin, got {out:?}"
        );
        let sent = s.last_user.lock().unwrap().clone();
        assert!(
            token_count(&sent) <= DEDUP_PROMPT_MAX_TOKENS,
            "prompt sent was {} tokens, budget {DEDUP_PROMPT_MAX_TOKENS}",
            token_count(&sent)
        );
    }

    /// The budget has to be a real discriminator over realistic input sizes,
    /// and it has to be monotone in them — otherwise "the prompt fits" is a
    /// statement about one fixture rather than about the guard.
    #[test]
    fn the_budget_straddles_realistic_pair_sizes_and_is_monotone() {
        let sizes = [0usize, 10, 200, 500, 900, 1_000, 5_000, 400_000];
        let counts: Vec<u64> = sizes
            .iter()
            .map(|&words| {
                let text = "observation ".repeat(words);
                prompt_tokens(&text, &text)
            })
            .collect();

        assert!(
            counts.windows(2).all(|w| w[0] < w[1]),
            "token count must grow with the pair: {counts:?}"
        );
        let (under, over): (Vec<u64>, Vec<u64>) =
            counts.iter().partition(|&&t| t <= DEDUP_PROMPT_MAX_TOKENS);
        assert!(
            under.len() >= 4 && over.len() >= 3,
            "the budget must sit inside the realistic range, not past it: \
             {} under / {} over, counts {counts:?}",
            under.len(),
            over.len()
        );
    }

    // --- Ranking ---------------------------------------------------------

    #[test]
    fn ranking_is_nearest_first_and_capped_at_top_k() {
        let candidates: Vec<store::ObservationVector> = (0..12)
            .map(|i| store::ObservationVector {
                id: i,
                // Similarities 0.9999 down to 0.9889 — all above 0.97.
                embedding: vec_with_similarity(0.9999 - i as f64 * 0.001),
                text: format!("obs {i}"),
            })
            .collect();

        let ranked = rank_candidates(candidates, &vec_at(0.0), 0.97);

        assert_eq!(ranked.len(), DEDUP_TOP_K);
        assert_eq!(
            ranked.iter().map(|(_, c)| c.id).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        assert!(ranked.windows(2).all(|w| w[0].0 >= w[1].0));
    }

    // --- Measured ---------------------------------------------------------

    /// The dedup probe's cost: loading a bank's observation vectors and
    /// ranking them. This is the whole non-LLM part of the path, and it is
    /// the number that decides when the `// ponytail` full-scan comment's
    /// upgrade path is due. Run:
    /// `cargo test --release -p memgardend dedup_probe_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "seeds 2000 observations and reports a timing"]
    fn dedup_probe_bench() {
        use std::time::Instant;

        let db = db_with_bank();
        for n in [500usize, 2_000] {
            let have: i64 = db
                .read()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM memory_nodes WHERE fact_type = 'observation'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            for i in (have as usize)..n {
                store::insert_observation(
                    &db,
                    "b1",
                    &format!("observation number {i} about the retain worker"),
                    &vec_at(i as f64 * 0.001),
                    &[],
                )
                .unwrap();
            }

            let probe = vec_at(0.0);
            let mut samples: Vec<u64> = Vec::with_capacity(200);
            for _ in 0..200 {
                let started = Instant::now();
                let candidates = store::observation_vectors(&db, "b1", -1).unwrap();
                let ranked = rank_candidates(candidates, &probe, 0.97);
                std::hint::black_box(ranked);
                samples.push(started.elapsed().as_micros() as u64);
            }
            samples.sort_unstable();
            println!(
                "dedup probe @ {n} observations: p50 {}us p95 {}us max {}us",
                samples[samples.len() / 2],
                samples[samples.len() * 95 / 100],
                samples[samples.len() - 1],
            );
        }
    }

    // --- Live ------------------------------------------------------------

    /// End-to-end against the real Ollama: two observations asserting the
    /// same fact in different words must merge, and the merged text must keep
    /// both details. Run:
    /// `cargo test -p memgardend live_dedup_merge -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "requires a running Ollama with the configured model"]
    async fn live_dedup_merge() {
        let ollama = OllamaClient::new(
            memgarden_core::config::Config::defaults()
                .expect("default config")
                .ollama,
        )
        .expect("client");
        let db = db_with_bank();
        let fact = nodes::insert(
            &db,
            memgarden_store::models::NewNode::new(
                "b1",
                memgarden_core::types::FactType::World,
                "source fact",
            ),
        )
        .unwrap();
        let twin = store::insert_observation(
            &db,
            "b1",
            "MemGarden's recall p95 is 20ms after forcing embeddings onto the CPU.",
            &vec_at(0.0),
            &[fact],
        )
        .unwrap();

        let started = std::time::Instant::now();
        let out = store_observation(
            &db,
            &ollama,
            &cfg(0.97),
            "b1",
            "After moving embedding inference to CPU, recall p95 for MemGarden settled at 20 ms.",
            &vec_with_similarity(0.999),
            &[],
        )
        .await
        .expect("live dedup");
        println!(
            "live_dedup_merge: {out:?} in {:.1}s\n  merged text: {:?}",
            started.elapsed().as_secs_f64(),
            nodes::get(&db, twin).unwrap().map(|n| n.text),
        );
        assert!(
            matches!(out, Outcome::Merged { into, .. } if into == twin),
            "the live model should merge two phrasings of one fact, got {out:?}"
        );
    }
}
