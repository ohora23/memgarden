//! The task-ledger write path (migration `0012`).
//!
//! One extra Ollama call at the end of a retain job, over the **tail** of the
//! transcript, producing the bank's current working state: goal, done, open,
//! next action. Written to `task_ledger`, one row per bank, replacing whatever
//! was there.
//!
//! **Nothing reads it.** That is the point of this stage: the rows accumulate
//! so their content can be judged before any of it reaches a prompt. The local
//! prior argues for exactly this order — MX-3 measured memory as an 11-7 loss
//! at +5% tokens on its sample, so "the extractor produces something worth
//! injecting" is a claim to verify, not to assume.
//!
//! # Why the tail, and why one call
//!
//! Working state is what is true NOW. The first chunk of a 116-hour
//! transcript describes a task that finished four days ago, and feeding the
//! whole thing back costs a second pass over material the fact extractor has
//! already read chunk by chunk. [`TAIL_CHARS`] takes the recent end only.
//!
//! One call, not one per chunk: the ledger is a single small object about the
//! whole job, and per-chunk ledgers would need merging, which is a second
//! model call to resolve contradictions between snapshots taken minutes
//! apart. A retain job already costs N extraction calls; this adds one.
//!
//! # `anchors` is not asked of the model
//!
//! It is assembled from values the daemon already holds: the session's `cwd`
//! and the `file:<path>` tags the retain request computed. A model asked for a
//! branch name it cannot see will produce a plausible one, and a fabricated
//! anchor is worse than no anchor — the whole purpose of the field is to be
//! re-checked against the filesystem later, so an invented value converts a
//! safety mechanism into a source of false confidence.
//!
//! Git branch and HEAD belong here too and are not available yet: the hook
//! reads them (every transcript record carries `gitBranch`) but does not send
//! them, and adding a request field is a change to the wire contract that this
//! stage does not need. `{"cwd": …, "paths": [...]}` is enough to tell a
//! future reader whether it is looking at the same working tree.

use serde::Deserialize;
use serde_json::{Value, json};

use memgarden_store::task_ledger::{self, LedgerUpdate};

use crate::ollama::OllamaError;
use crate::state::AppState;

use super::RetainTask;

/// How much of the transcript's end the ledger call sees.
///
/// Sized against the extractor's own chunk (`retain.chunk_size`, 3000 by
/// default) rather than against the context window: a few recent chunks is
/// enough to say what is being worked on, and every character past that is
/// older state competing with newer state for the model's attention.
pub const TAIL_CHARS: usize = 9000;

/// `num_predict` ceiling for the reply.
///
/// The reply is four short strings. The configured `ollama.num_predict` is
/// 8192, sized for extraction where one chunk legitimately produces pages;
/// letting a fixed-shape object run that long is how the 2026-08-02
/// consolidation truncation happened.
const REPLY_MAX_TOKENS: u32 = 768;

/// Per-field `maxLength` in the JSON schema.
///
/// **Not a taste decision.** Ollama compiles `maxLength: N` into a GBNF
/// grammar as N character repetitions and its parser refuses past roughly two
/// thousand on `/api/generate`; bisected at 2000 compiles / 2031 does not
/// (`mental::REFRESH_CONTENT_MAX_CHARS`). Exceeding it does not degrade the
/// reply, it fails every call with `failed to load model vocabulary required
/// for format` — which is how CE-10 ran at a 100% failure rate for two months.
const FIELD_MAX_CHARS: usize = 500;

/// Enforced at COMPILE time, not in a test. A test can be filtered out or
/// left unrun; this bound turns the whole feature into a 100% failure rate
/// when crossed, which is how CE-10 stayed broken for two months without
/// anyone noticing, so it must not be possible to build past it.
const _: () = assert!(FIELD_MAX_CHARS <= 2000, "GBNF repetition limit");

/// The model's reply. Every field is **required** in the schema and
/// non-optional here.
///
/// This codebase has been bitten three times by asking this model for
/// optional structure, most recently CE-12's `superseded_by`, which came back
/// unfilled on every call. A field the model may omit is a field it will
/// omit, so "nothing to say" has to be an empty string it is obliged to
/// produce rather than a key it is allowed to skip.
#[derive(Debug, Deserialize)]
struct LedgerReply {
    goal: String,
    done: String,
    open: String,
    next_action: String,
}

fn schema() -> Value {
    let field = json!({"type": "string", "maxLength": FIELD_MAX_CHARS});
    json!({
        "type": "object",
        "properties": {
            "goal": field,
            "done": field,
            "open": field,
            "next_action": field,
        },
        "required": ["goal", "done", "open", "next_action"],
    })
}

const SYSTEM: &str = "\
You record the CURRENT WORKING STATE of a software project from the tail of a \
work transcript. You are not summarising the conversation and you are not \
extracting facts.

Report only what is true at the END of the transcript. Work that was finished \
and moved past belongs in `done`, not in `goal`.

goal        The one thing being worked toward right now. One sentence.
done        Steps completed, newest first, one per line.
open        Unfinished steps, blockers, unresolved decisions, one per line.
next_action The single next thing to do. One sentence.

Use the transcript's own nouns — file paths, identifiers, PR numbers, branch \
names. A reader who has forgotten everything must be able to act on this.

If the transcript does not say, write an empty string for that field. Never \
guess, and never carry over a goal the transcript shows as finished.

Reply with ONLY this JSON object:
{\"goal\": \"...\", \"done\": \"...\", \"open\": \"...\", \"next_action\": \"...\"}

Do NOT use markdown fences or any text outside the JSON object.";

/// The `{"cwd": …, "paths": [...]}` anchor, from values the daemon already
/// holds. Never from the model — see the module docs.
fn anchors(task: &RetainTask, cwd: Option<&str>) -> String {
    let paths: Vec<&str> = task
        .tags
        .iter()
        .filter_map(|t| t.strip_prefix("file:"))
        .collect();
    json!({"cwd": cwd, "paths": paths}).to_string()
}

/// Writes the bank's ledger from this job's transcript.
///
/// Every failure is swallowed by the caller: the ledger is an addition to a
/// retain job, never a reason for one to fail. A job that ingested its facts
/// and could not write a ledger has done the thing it exists to do.
pub async fn write(
    state: &AppState,
    task: &RetainTask,
    cwd: Option<&str>,
) -> Result<(), OllamaError> {
    let tail = tail_of(&task.transcript, TAIL_CHARS);
    if tail.trim().is_empty() {
        return Ok(());
    }

    let reply: LedgerReply = state
        .ollama
        .chat_json_background_bounded(SYSTEM, tail, &schema(), REPLY_MAX_TOKENS, None)
        .await?;

    // An empty goal is the model's honest "the transcript does not say", and
    // the store refuses it. Returning early keeps that out of the logs as an
    // error, because it is not one.
    if reply.goal.trim().is_empty() {
        tracing::debug!(job_id = %task.job_id, "task ledger: no goal in transcript tail, not written");
        return Ok(());
    }

    let anchors = anchors(task, cwd);
    let db = state.db.clone();
    let bank_id = task.bank_id.clone();
    let session_id = task.session_id.clone();
    let job_id = task.job_id.clone();
    let stored = tokio::task::spawn_blocking(move || {
        task_ledger::upsert(
            &db,
            &bank_id,
            &LedgerUpdate {
                goal: &reply.goal,
                done: &reply.done,
                open: &reply.open,
                next_action: &reply.next_action,
                anchors: &anchors,
                session_id: session_id.as_deref(),
                job_id: Some(&job_id),
            },
        )
    })
    .await;

    match stored {
        Ok(Ok(_)) => {
            tracing::info!(job_id = %task.job_id, bank_id = %task.bank_id, "task ledger written")
        }
        Ok(Err(e)) => tracing::warn!(job_id = %task.job_id, error = %e, "task ledger store failed"),
        Err(e) => {
            tracing::warn!(job_id = %task.job_id, error = %e, "task ledger write task panicked")
        }
    }
    Ok(())
}

/// The last `max` characters, cut forward to the next char boundary.
///
/// Cutting mid-character would panic on a byte slice, and this transcript is
/// routinely Korean.
fn tail_of(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(tags: &[&str]) -> RetainTask {
        RetainTask {
            job_id: "j".into(),
            bank_id: "b".into(),
            document_id: 1,
            session_id: None,
            transcript: String::new(),
            event_date_ms: 0,
            mission: None,
            context: None,
            tags: tags.iter().map(|s| (*s).to_string()).collect(),
            content_hash: String::new(),
            byte_offset: None,
            offset_from: None,
        }
    }

    #[test]
    fn the_tail_is_cut_on_a_char_boundary() {
        // Every char is 3 bytes, so a cut at an arbitrary byte lands inside
        // one unless the boundary walk fixes it.
        let s = "가".repeat(100);
        let tail = tail_of(&s, 50);
        assert!(tail.len() <= 50);
        assert!(s.ends_with(tail));
    }

    #[test]
    fn a_short_transcript_is_its_own_tail() {
        assert_eq!(tail_of("short", TAIL_CHARS), "short");
    }

    #[test]
    fn anchors_carry_only_file_tags() {
        let a = anchors(
            &task(&["file:src/a.rs", "session:s1", "file:b.rs"]),
            Some("/w"),
        );
        assert_eq!(
            a, r#"{"cwd":"/w","paths":["src/a.rs","b.rs"]}"#,
            "session: tags must not leak into paths"
        );
    }

    #[test]
    fn anchors_are_valid_json_with_nothing_to_say() {
        let a = anchors(&task(&[]), None);
        assert_eq!(a, r#"{"cwd":null,"paths":[]}"#);
    }

    #[test]
    fn every_field_is_required() {
        // Optional fields are not filled by this model — measured at 0 of 11
        // on CE-12. A schema edit that relaxes one silently empties the
        // ledger, so the requirement is a test rather than a convention.
        let s = schema();
        let required: Vec<&str> = s["required"]
            .as_array()
            .expect("required")
            .iter()
            .map(|v| v.as_str().expect("str"))
            .collect();
        let properties = s["properties"].as_object().expect("properties");
        assert_eq!(required.len(), properties.len());
        for key in properties.keys() {
            assert!(required.contains(&key.as_str()), "{key} is optional");
        }
    }
}
