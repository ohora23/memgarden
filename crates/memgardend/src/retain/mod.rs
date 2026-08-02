//! Transcript retain ingest (CE-5b, PR B3).
//!
//! Split in two on purpose:
//!
//! * `plan_ingest` runs **synchronously in the request handler**. It
//!   normalizes the transcript twice — once uncapped, once with both fork
//!   caps — counts cl100k tokens for each, and hands back everything the
//!   202 response promises (`raw_tokens` / `capped_tokens` / `saved_tokens` /
//!   `saving_ratio`). The numbers have to be real at response time, and
//!   normalizing + tokenizing is microseconds-to-milliseconds work.
//! * `run_worker` does the slow half in the background: chunk, one Ollama
//!   call per chunk, write nodes. Driven by a bounded `mpsc` so a flooded
//!   queue answers 429 rather than growing unboundedly in RAM.
//!
//! Embeddings are deliberately left `NULL` here — B1's backlog worker picks
//! the new nodes up on its next tick. **Divergence from legacy**, which
//! embeds inline and synchronously (`orchestrator.py:579,617`); async keeps
//! the retain write transaction short and reuses a path that already exists.

pub mod chunk;
pub mod transcript;

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tiktoken_rs::CoreBPE;

use memgarden_core::config::RetainConfig;
use memgarden_core::metrics::METRICS;
use memgarden_core::types::FactType;
use memgarden_store::models::NewNode;
use memgarden_store::nodes::NewNodeWithTags;
use memgarden_store::retain_jobs::{JobProgress, JobStatus};
use memgarden_store::{nodes, retain_jobs};

use crate::extract;
use crate::extract::parse::ParsedFact;
use crate::state::AppState;

/// Ordering offset applied per fact so retrieval can distinguish facts that
/// share a base date. legacy: `SECONDS_PER_FACT = 0.01`
/// (`fact_extraction.py:1922`) applied in `_add_temporal_offsets`
/// (`:2716-2740`) — 10ms in our unix-ms units. `i` is the fact's **absolute
/// index across the whole document**, not its index within its chunk
/// (Critic Revision NIT 16).
const FACT_OFFSET_MS: i64 = 10;

/// cl100k_base, built once for the process. 21ms of one-time work and ~1.6MB
/// of embedded BPE ranks; never per request (plan decision #5 — the
/// `gc.collect` lesson). Same lock-free-static shape as `METRICS` and
/// `embed::EmbedStatus`.
fn tokenizer() -> &'static CoreBPE {
    static TOKENIZER: OnceLock<CoreBPE> = OnceLock::new();
    // The BPE ranks are compiled into the binary; a failure here means a
    // corrupt build, not a runtime condition.
    TOKENIZER.get_or_init(|| tiktoken_rs::cl100k_base().expect("embedded cl100k_base ranks"))
}

/// Forces the one-time 21ms init at startup so it never lands on a request.
pub fn warm_tokenizer() {
    let _ = tokenizer();
}

pub fn token_count(text: &str) -> u64 {
    tokenizer().encode_with_special_tokens(text).len() as u64
}

/// Everything the handler computes up front: the capped transcript that will
/// actually be extracted, the token accounting the ledger records, and the
/// `file:` tags.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestPlan {
    pub transcript: String,
    pub message_count: usize,
    pub raw_tokens: u64,
    pub capped_tokens: u64,
    pub file_tags: Vec<String>,
    pub files_modified: Vec<String>,
    pub content_hash: String,
}

impl IngestPlan {
    pub fn saved_tokens(&self) -> u64 {
        self.raw_tokens.saturating_sub(self.capped_tokens)
    }

    /// `1 - capped/raw`, clamped at 0 for the (possible in principle) case
    /// where capping made the payload marginally larger.
    pub fn saving_ratio(&self) -> f64 {
        if self.raw_tokens == 0 {
            return 0.0;
        }
        (1.0 - (self.capped_tokens as f64 / self.raw_tokens as f64)).max(0.0)
    }
}

/// Normalizes twice (uncapped for the baseline, capped for real) and counts
/// tokens for both. `None` when there is nothing worth retaining.
///
/// The backfill cap is part of the *capped* pass: it is one of the two fork
/// caps the ledger is measuring, so the raw baseline sees every message.
pub fn plan_ingest(
    messages: &[Value],
    cwd: &str,
    is_initial: bool,
    cfg: &RetainConfig,
) -> Option<IngestPlan> {
    let roles = vec!["user".to_string(), "assistant".to_string()];
    let uncapped_opts = transcript::NormalizeOpts {
        roles: &roles,
        include_tool_calls: cfg.include_tool_calls,
        caps: transcript::Caps::none(),
    };
    let capped_opts = transcript::NormalizeOpts {
        roles: &roles,
        include_tool_calls: cfg.include_tool_calls,
        caps: transcript::Caps {
            tool_input_field_max: cfg.tool_input_field_max,
            tool_input_total_max: cfg.tool_input_total_max,
            tool_result_max: cfg.tool_result_max,
        },
    };

    let raw_tokens = transcript::normalize(messages, &uncapped_opts)
        .map(|(t, _)| token_count(&t))
        .unwrap_or(0);

    let capped_messages = transcript::apply_backfill_cap(messages, is_initial, cfg.max_initial_messages);
    let (transcript_text, message_count) = transcript::normalize(capped_messages, &capped_opts)?;
    let capped_tokens = token_count(&transcript_text);

    let files_modified = transcript::extract_touched_files(capped_messages, cwd);
    let file_tags: Vec<String> = files_modified
        .iter()
        .take(cfg.file_tag_cap)
        .map(|p| format!("file:{p}"))
        .collect();

    let content_hash = format!("{:x}", Sha256::digest(transcript_text.as_bytes()));

    Some(IngestPlan {
        transcript: transcript_text,
        message_count,
        raw_tokens,
        capped_tokens,
        file_tags,
        files_modified: files_modified.into_iter().take(cfg.file_tag_cap).collect(),
        content_hash,
    })
}

/// One enqueued unit of background work.
#[derive(Debug, Clone)]
pub struct RetainTask {
    pub job_id: String,
    pub bank_id: String,
    pub document_id: i64,
    pub session_id: Option<String>,
    pub transcript: String,
    pub event_date_ms: i64,
    pub mission: Option<String>,
    pub context: Option<String>,
    /// Configured tags + `session:<id>` + `file:<path>` tags, already capped.
    pub tags: Vec<String>,
}

/// Drains the retain queue until shutdown. One task at a time — extraction is
/// serialized by Ollama's single permit anyway, so a second concurrent job
/// would only add lock contention.
pub async fn run_worker(state: AppState, mut rx: tokio::sync::mpsc::Receiver<RetainTask>) {
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            task = rx.recv() => match task {
                Some(task) => run_job(&state, task).await,
                None => break,
            },
            _ = &mut shutdown => break,
        }
    }
}

async fn run_job(state: &AppState, task: RetainTask) {
    let cfg = &state.cfg.retain;
    let chunks = chunk::chunk_text(&task.transcript, cfg.chunk_size);
    let wall_timeout = Duration::from_secs(cfg.wall_timeout_secs);
    let started = Instant::now();

    let mut progress = JobProgress {
        status: JobStatus::Running,
        chunks_total: chunks.len() as i64,
        ..Default::default()
    };
    flush(state, &task.job_id, &progress);

    // Absolute fact index across the whole document (NIT 16) — the +10ms
    // ordering offsets must keep increasing across chunk boundaries, not
    // restart at 0 in every chunk.
    let mut abs_fact_index: usize = 0;
    let mut last_error: Option<String> = None;
    let mut timed_out = false;

    for (i, chunk) in chunks.iter().enumerate() {
        if started.elapsed() > wall_timeout {
            // Critic Revision R11: parity with the live hindsight daemon's
            // RETAIN_WALL_TIMEOUT=7200. Partial progress is already
            // committed and stays recorded.
            timed_out = true;
            last_error = Some(format!(
                "retain wall timeout after {}s at chunk {}/{}",
                wall_timeout.as_secs(),
                i,
                chunks.len()
            ));
            break;
        }

        // CE-5a review carry-over: a whitespace/punctuation-only chunk must
        // never reach Ollama. It counts as done, not failed.
        if chunk.trim().is_empty() || extract::parse::is_degenerate_text(chunk) {
            progress.chunks_done += 1;
            flush(state, &task.job_id, &progress);
            continue;
        }

        // `chat_json` acquires and releases the Ollama permit inside this
        // call, so the background worker holds nothing between chunks and an
        // interactive /dry-run-extract waits at most one chunk (Critic
        // Revision R11). Open question, unchanged from CE-5a: reqwest gives
        // us no client-disconnect signal here, so a caller who hangs up
        // mid-generation still burns the permit until Ollama answers — the
        // per-call deadline in ollama.rs is the only bound.
        match extract::extract(
            &state.ollama,
            chunk,
            Some(task.event_date_ms),
            task.mission.as_deref(),
        )
        .await
        {
            Ok(facts) => {
                let n = facts.len();
                match write_facts(state, &task, &facts, abs_fact_index) {
                    Ok(written) => {
                        abs_fact_index += n;
                        progress.facts_written += written as i64;
                        progress.chunks_done += 1;
                    }
                    Err(e) => {
                        // A storage failure is not a "chunk failed to
                        // extract" case, but it is still per-chunk: record
                        // and keep going, same as R14.
                        tracing::warn!(job_id = %task.job_id, chunk = i, error = %e, "retain node write failed");
                        progress.chunks_failed += 1;
                        METRICS.retain_chunks_failed.fetch_add(1, Ordering::Relaxed);
                        last_error = Some(e.to_string());
                    }
                }
            }
            Err(e) => {
                // Critic Revision R14: one failed chunk bumps a counter and
                // the job continues; only an all-chunks failure fails the job.
                tracing::warn!(job_id = %task.job_id, chunk = i, error = %e, "retain chunk extraction failed");
                progress.chunks_failed += 1;
                METRICS.retain_chunks_failed.fetch_add(1, Ordering::Relaxed);
                last_error = Some(e.to_string());
            }
        }
        flush(state, &task.job_id, &progress);
    }

    let all_failed = progress.chunks_done == 0 && progress.chunks_failed > 0;
    progress.status = if timed_out || all_failed {
        METRICS.retain_errors.fetch_add(1, Ordering::Relaxed);
        JobStatus::Failed
    } else {
        JobStatus::Done
    };
    progress.error = last_error;
    flush(state, &task.job_id, &progress);
    tracing::info!(
        job_id = %task.job_id,
        bank_id = %task.bank_id,
        status = progress.status.as_str(),
        chunks_total = progress.chunks_total,
        chunks_done = progress.chunks_done,
        chunks_failed = progress.chunks_failed,
        facts_written = progress.facts_written,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "retain job finished"
    );
}

/// Progress flush. Best-effort: a job whose row cannot be updated has still
/// done real work, and failing the whole run over a status write would be
/// worse than a stale row.
fn flush(state: &AppState, job_id: &str, progress: &JobProgress) {
    if let Err(e) = retain_jobs::update(&state.db, job_id, progress) {
        tracing::warn!(job_id = %job_id, error = %e, "retain job progress update failed");
    }
}

/// Writes one chunk's facts as memory nodes in a single transaction.
/// Embeddings stay NULL — see the module doc.
fn write_facts(
    state: &AppState,
    task: &RetainTask,
    facts: &[ParsedFact],
    abs_index_base: usize,
) -> memgarden_core::error::Result<usize> {
    if facts.is_empty() {
        return Ok(0);
    }
    let drafts: Vec<NodeDraft> = facts
        .iter()
        .enumerate()
        .map(|(i, fact)| NodeDraft::build(fact, task, abs_index_base + i))
        .collect();

    let items: Vec<NewNodeWithTags> = drafts
        .iter()
        .map(|d| NewNodeWithTags {
            node: NewNode {
                bank_id: &task.bank_id,
                document_id: Some(task.document_id),
                fact_type: d.fact_type,
                text: &d.text,
                context: task.context.as_deref(),
                event_date: d.event_date,
                occurred_start: d.occurred_start,
                occurred_end: d.occurred_end,
                mentioned_at: d.mentioned_at,
                metadata: Some(&d.metadata),
            },
            tags: &task.tags,
        })
        .collect();

    let ids = nodes::insert_batch(&state.db, &items)?;
    METRICS
        .nodes_written
        .fetch_add(ids.len() as u64, Ordering::Relaxed);
    Ok(ids.len())
}

struct NodeDraft {
    text: String,
    fact_type: FactType,
    event_date: Option<i64>,
    occurred_start: Option<i64>,
    occurred_end: Option<i64>,
    mentioned_at: Option<i64>,
    metadata: String,
}

impl NodeDraft {
    fn build(fact: &ParsedFact, task: &RetainTask, abs_index: usize) -> NodeDraft {
        let offset = abs_index as i64 * FACT_OFFSET_MS;
        let occurred_start = fact
            .occurred_start
            .as_deref()
            .and_then(parse_iso_ms)
            .map(|ms| ms + offset);
        let occurred_end = fact
            .occurred_end
            .as_deref()
            .and_then(parse_iso_ms)
            .map(|ms| ms + offset);
        let mentioned_at = Some(task.event_date_ms + offset);
        // legacy: `engine/memories/pg/writes.py:80`.
        let event_date = occurred_start.or(mentioned_at);

        let mut meta = Map::new();
        meta.insert("fact_kind".to_string(), json!(fact.fact_kind));
        if let Some(where_field) = &fact.where_field {
            meta.insert("where".to_string(), json!(where_field));
        }
        if let Some(session_id) = &task.session_id {
            meta.insert("session_id".to_string(), json!(session_id));
        }
        if !fact.entities.is_empty() {
            // Entity *resolution* (a row in `entities`, co-occurrences,
            // links) is CE-7/B5. Carrying the raw strings on the node now
            // means B5 can backfill without re-running extraction.
            meta.insert("entities".to_string(), json!(fact.entities));
        }

        NodeDraft {
            text: fact.text.clone(),
            fact_type: fact.fact_type,
            event_date,
            occurred_start,
            occurred_end,
            mentioned_at,
            metadata: Value::Object(meta).to_string(),
        }
    }
}

/// Best-effort ISO-8601 -> unix ms. Accepts a full timestamp
/// (`2024-06-10T00:00:00Z`), a naive datetime (assumed **UTC**, legacy
/// `orchestrator.py:228-258`), or a bare date. The full relative-expression
/// resolver (`_infer_temporal_date`) is CE-8/B6.
pub fn parse_iso_ms(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(ts) = s.parse::<jiff::Timestamp>() {
        return Some(ts.as_millisecond());
    }
    if let Ok(dt) = s.parse::<jiff::civil::DateTime>() {
        return dt
            .to_zoned(jiff::tz::TimeZone::UTC)
            .ok()
            .map(|z| z.timestamp().as_millisecond());
    }
    if let Ok(date) = s.parse::<jiff::civil::Date>() {
        return date
            .to_zoned(jiff::tz::TimeZone::UTC)
            .ok()
            .map(|z| z.timestamp().as_millisecond());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RetainConfig {
        memgarden_core::config::Config::defaults().unwrap().retain
    }

    fn transcript_messages() -> Vec<Value> {
        vec![
            json!({ "role": "user", "content": "why did recall latency regress?" }),
            json!({
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Checking the reranker path." },
                    { "type": "tool_use", "name": "Write", "input": {
                        "file_path": "/repo/src/rerank.rs",
                        "content": "fn rerank() {}\n".repeat(400),
                    }},
                ],
            }),
        ]
    }

    #[test]
    fn plan_ingest_records_a_real_saving() {
        let mut cfg = cfg();
        cfg.include_tool_calls = true;
        let plan = plan_ingest(&transcript_messages(), "/repo", true, &cfg).unwrap();

        assert!(plan.raw_tokens > plan.capped_tokens);
        assert_eq!(plan.saved_tokens(), plan.raw_tokens - plan.capped_tokens);
        assert!(
            plan.saving_ratio() > 0.5,
            "a 5.6KB Write must save well over half the tokens, got {}",
            plan.saving_ratio()
        );
        assert_eq!(plan.file_tags, vec!["file:src/rerank.rs".to_string()]);
        assert_eq!(plan.content_hash.len(), 64, "hex sha256");
        assert_eq!(plan.message_count, 2);
    }

    #[test]
    fn plan_ingest_is_deterministic_and_hash_tracks_content() {
        let cfg = cfg();
        let a = plan_ingest(&transcript_messages(), "/repo", true, &cfg).unwrap();
        let b = plan_ingest(&transcript_messages(), "/repo", true, &cfg).unwrap();
        assert_eq!(a.content_hash, b.content_hash);

        let mut changed = transcript_messages();
        changed.push(json!({ "role": "user", "content": "one more question here" }));
        let c = plan_ingest(&changed, "/repo", true, &cfg).unwrap();
        assert_ne!(a.content_hash, c.content_hash);
    }

    #[test]
    fn plan_ingest_no_saving_when_nothing_is_capped() {
        let cfg = cfg();
        let messages = vec![json!({ "role": "user", "content": "a short but sufficient message" })];
        let plan = plan_ingest(&messages, "", true, &cfg).unwrap();
        assert_eq!(plan.raw_tokens, plan.capped_tokens);
        assert_eq!(plan.saved_tokens(), 0);
        assert_eq!(plan.saving_ratio(), 0.0);
    }

    #[test]
    fn plan_ingest_backfill_cap_only_on_initial() {
        let mut cfg = cfg();
        cfg.max_initial_messages = 5;
        let messages: Vec<Value> = (0..50)
            .map(|i| json!({ "role": "user", "content": format!("message number {i} with text") }))
            .collect();

        let initial = plan_ingest(&messages, "", true, &cfg).unwrap();
        assert_eq!(initial.message_count, 5);
        assert!(initial.transcript.contains("message number 49"));
        assert!(!initial.transcript.contains("message number 44"), "keeps the LAST 5");

        let delta = plan_ingest(&messages, "", false, &cfg).unwrap();
        assert_eq!(delta.message_count, 50, "delta retains are never capped");
        assert!(delta.raw_tokens > initial.capped_tokens);
    }

    #[test]
    fn plan_ingest_none_for_empty_transcript() {
        let cfg = cfg();
        assert!(plan_ingest(&[], "", true, &cfg).is_none());
        assert!(plan_ingest(&[json!({ "role": "user", "content": "" })], "", true, &cfg).is_none());
    }

    #[test]
    fn plan_ingest_caps_file_tags_at_the_configured_limit() {
        let mut cfg = cfg();
        cfg.include_tool_calls = true;
        let messages: Vec<Value> = (0..30)
            .map(|i| {
                json!({ "role": "assistant", "content": [
                    { "type": "tool_use", "name": "Edit", "input": { "file_path": format!("/repo/f{i}.rs") } }
                ]})
            })
            .collect();
        let plan = plan_ingest(&messages, "/repo", false, &cfg).unwrap();
        assert_eq!(plan.file_tags.len(), 20);
        assert_eq!(plan.file_tags[0], "file:f0.rs");
        assert_eq!(plan.files_modified.len(), 20);
    }

    #[test]
    fn token_count_is_cl100k() {
        // Sanity: the same counter legacy uses, so ledger numbers compare.
        assert_eq!(token_count(""), 0);
        assert!(token_count("hello world") >= 2);
        // Korean costs more tokens than the equivalent English, as measured
        // in the plan's Verified Environment Facts.
        assert!(token_count("메모리 시스템 지연 시간") > token_count("memory system latency"));
    }

    #[test]
    fn iso_parsing_accepts_the_three_shapes_legacy_emits() {
        let z = parse_iso_ms("2024-06-10T00:00:00Z").unwrap();
        let naive = parse_iso_ms("2024-06-10T00:00:00").unwrap();
        let date = parse_iso_ms("2024-06-10").unwrap();
        assert_eq!(z, naive, "a naive datetime is assumed UTC");
        assert_eq!(z, date);
        assert_eq!(z, 1_717_977_600_000);
        assert!(parse_iso_ms("not a date").is_none());
        assert!(parse_iso_ms("  ").is_none());
    }

    fn task() -> RetainTask {
        RetainTask {
            job_id: "j".to_string(),
            bank_id: "b".to_string(),
            document_id: 1,
            session_id: Some("sess".to_string()),
            transcript: String::new(),
            event_date_ms: 1_718_006_400_000,
            mission: None,
            context: Some("claude-code".to_string()),
            tags: vec![],
        }
    }

    fn fact(text: &str, start: Option<&str>) -> ParsedFact {
        ParsedFact {
            text: text.to_string(),
            fact_type: FactType::World,
            fact_kind: if start.is_some() { "event" } else { "conversation" }.to_string(),
            occurred_start: start.map(str::to_string),
            occurred_end: start.map(str::to_string),
            where_field: None,
            entities: vec![],
            causal_relations: vec![],
        }
    }

    #[test]
    fn fact_offsets_are_ten_ms_apart_across_the_whole_document() {
        let task = task();
        let base = task.event_date_ms;

        // Chunk 0, facts 0..2.
        let d0 = NodeDraft::build(&fact("a", None), &task, 0);
        let d1 = NodeDraft::build(&fact("b", None), &task, 1);
        // Chunk 1 starts at absolute index 2 — NOT back at 0.
        let d2 = NodeDraft::build(&fact("c", None), &task, 2);

        assert_eq!(d0.mentioned_at, Some(base));
        assert_eq!(d1.mentioned_at, Some(base + 10));
        assert_eq!(d2.mentioned_at, Some(base + 20));
        // No occurred_start -> event_date falls back to mentioned_at.
        assert_eq!(d2.event_date, Some(base + 20));
    }

    #[test]
    fn occurred_dates_get_the_offset_and_drive_event_date() {
        let task = task();
        let d = NodeDraft::build(&fact("launched", Some("2024-06-10")), &task, 3);
        assert_eq!(d.occurred_start, Some(1_717_977_600_000 + 30));
        assert_eq!(d.occurred_end, Some(1_717_977_600_000 + 30));
        assert_eq!(
            d.event_date, d.occurred_start,
            "event_date = occurred_start ?? mentioned_at"
        );
    }

    #[test]
    fn node_metadata_carries_kind_session_and_entities() {
        let task = task();
        let mut f = fact("x", None);
        f.entities = vec!["Ollama".to_string()];
        f.where_field = Some("Seoul".to_string());
        let d = NodeDraft::build(&f, &task, 0);
        let meta: Value = serde_json::from_str(&d.metadata).unwrap();
        assert_eq!(meta["fact_kind"], "conversation");
        assert_eq!(meta["session_id"], "sess");
        assert_eq!(meta["where"], "Seoul");
        assert_eq!(meta["entities"], json!(["Ollama"]));
    }
}
