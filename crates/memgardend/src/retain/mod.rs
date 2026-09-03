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
pub mod ledger;
pub mod transcript;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tiktoken_rs::CoreBPE;

use memgarden_core::config::RetainConfig;
use memgarden_core::metrics::METRICS;
use memgarden_core::types::FactType;
use memgarden_store::graph as store_graph;
use memgarden_store::models::NewNode;
use memgarden_store::nodes::NewNodeWithTags;
use memgarden_store::retain_jobs::{JobProgress, JobStatus};
use memgarden_store::{documents, nodes, retain_jobs};

use crate::extract;
use crate::extract::parse::ParsedFact;
use crate::extract::prompts::KnownFact;
use crate::state::AppState;
use crate::temporal::parse::parse_iso_ms;
use crate::{entities, links};

/// Ordering offset applied per fact so retrieval can distinguish facts that
/// share a base date. legacy: `SECONDS_PER_FACT = 0.01`
/// (`fact_extraction.py:1922`) applied in `_add_temporal_offsets`
/// (`:2716-2740`) — 10ms in our unix-ms units. `i` is the fact's **absolute
/// index across the whole document**, not its index within its chunk
/// (Critic Revision NIT 16).
const FACT_OFFSET_MS: i64 = 10;

/// One day in ms — CE-12 turns "true through March 3" into an expiry at the
/// end of March 3 rather than its midnight.
const DAY_MS: i64 = 86_400_000;

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

    let capped_messages =
        transcript::apply_backfill_cap(messages, is_initial, cfg.max_initial_messages);
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
    /// SHA-256 of `transcript`. Written onto the document **only** when the
    /// job completes cleanly (review HIGH 1): persisting it at enqueue time
    /// meant a partially-failed job made every re-POST of the same
    /// transcript a permanent "duplicate", i.e. silent data loss.
    pub content_hash: String,
    /// HK-1a: the transcript byte position these messages carry the hook up
    /// to, as the request reported it. Written to `sessions.confirmed_offset`
    /// on a clean run — same place, and for the same reason, as the content
    /// hash. `None` when the caller is not the hook.
    pub byte_offset: Option<i64>,
    /// The other end of that range, and the **guard** for writing it: the
    /// durable cursor advances to `byte_offset` only if it has already reached
    /// `offset_from`, so a clean job cannot confirm over an earlier job's gap
    /// (migration `0008`). `None` leaves the cursor alone rather than
    /// advancing it on a guess.
    pub offset_from: Option<i64>,
}

/// Total transcript bytes currently sitting in the retain queue.
///
/// The `mpsc` bound caps the queue at 32 *jobs*, which says nothing about
/// RAM: 32 × 32MB is a gigabyte of held transcripts. This is the byte-side
/// budget (review MEDIUM 4).
///
/// // ponytail: fetch_add-then-rollback, so a burst of concurrent admissions
/// // can overshoot the cap briefly by at most (concurrent requests × body
/// // limit). Bounded by the 32-job queue; swap for a CAS loop only if that
/// // ever matters.
static QUEUED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Ceiling for `QUEUED_BYTES`. 8× the per-request body limit, so a single
/// oversized retain can never wedge the queue on its own.
pub const MAX_QUEUED_BYTES: usize = 256 * 1024 * 1024;

/// Admits `n` bytes into the queue budget, or returns `false` (caller
/// answers 429).
pub fn try_reserve_bytes(n: usize) -> bool {
    if QUEUED_BYTES.fetch_add(n, Ordering::Relaxed) + n > MAX_QUEUED_BYTES {
        QUEUED_BYTES.fetch_sub(n, Ordering::Relaxed);
        return false;
    }
    true
}

fn release_bytes(n: usize) {
    QUEUED_BYTES.fetch_sub(n, Ordering::Relaxed);
}

pub fn queued_bytes() -> usize {
    QUEUED_BYTES.load(Ordering::Relaxed)
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
    let queued = task.transcript.len();
    run_job_inner(state, task).await;
    release_bytes(queued);
}

async fn run_job_inner(state: &AppState, task: RetainTask) {
    let cfg = &state.cfg.retain;
    let wall_timeout = Duration::from_secs(cfg.wall_timeout_secs);
    let started = Instant::now();

    // Chunking is a pure CPU pass over a transcript that can reach the body
    // limit; it does not belong on a runtime worker thread.
    let chunk_size = cfg.chunk_size;
    let transcript = task.transcript.clone();
    let chunks =
        match tokio::task::spawn_blocking(move || chunk::chunk_text(&transcript, chunk_size)).await
        {
            Ok(chunks) => chunks,
            Err(e) => {
                tracing::error!(job_id = %task.job_id, error = %e, "retain chunking task panicked");
                let progress = JobProgress {
                    status: JobStatus::Failed,
                    error: Some(format!("chunking task panicked: {e}")),
                    ..Default::default()
                };
                flush(state, &task.job_id, &progress).await;
                METRICS.retain_jobs_failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };

    let mut progress = JobProgress {
        status: JobStatus::Running,
        chunks_total: chunks.len() as i64,
        ..Default::default()
    };
    flush(state, &task.job_id, &progress).await;

    // Absolute fact index across the whole document (NIT 16) — the +10ms
    // ordering offsets must keep increasing across chunk boundaries, not
    // restart at 0 in every chunk.
    let mut abs_fact_index: usize = 0;
    let mut last_error: Option<String> = None;
    let mut aborted: Option<String> = None;

    // Critic Revision NIT 15: a long job must react to SIGTERM between
    // chunks, not only between jobs.
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);

    for (i, chunk) in chunks.iter().enumerate() {
        if started.elapsed() > wall_timeout {
            // Critic Revision R11: parity with the live hindsight daemon's
            // RETAIN_WALL_TIMEOUT=7200. Partial progress stays committed.
            aborted = Some(format!(
                "retain wall timeout after {}s at chunk {}/{}",
                wall_timeout.as_secs(),
                i,
                chunks.len()
            ));
            break;
        }
        if futures_ready(&mut shutdown) {
            aborted = Some(format!("daemon shut down at chunk {}/{}", i, chunks.len()));
            break;
        }

        // CE-5a review carry-over: a whitespace/punctuation-only chunk must
        // never reach Ollama. Counted as SKIPPED, not done — counting it as
        // done would let an all-chunks-failed job look partially successful
        // (review LOW 13).
        if chunk.trim().is_empty() || extract::parse::is_degenerate_text(chunk) {
            progress.chunks_skipped += 1;
            flush(state, &task.job_id, &progress).await;
            continue;
        }

        // CE-12: fetched before extraction because the model has to see them
        // to judge them. Empty when the feature is off, when the bank is
        // still empty, or when anything about the lookup fails — and an empty
        // list is exactly the pre-CE-12 prompt, so a degraded candidate
        // lookup costs a retraction, never a chunk.
        let known = candidate_facts(state, &task.bank_id, chunk).await;

        match extract_chunk(state, &task, chunk, &known).await {
            Ok(facts) => {
                let n = facts.len();
                match write_facts(state, &task, facts, abs_fact_index, &known).await {
                    Ok(written) => {
                        abs_fact_index += n;
                        progress.facts_written += written as i64;
                        progress.chunks_done += 1;
                    }
                    Err(e) => {
                        // A storage failure is not "the chunk failed to
                        // extract", but it is still per-chunk: record and
                        // keep going, same as R14.
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
        flush(state, &task.job_id, &progress).await;
    }

    // Review LOW 13: "nothing was written and something failed" is the real
    // all-failed condition. Skipped chunks must not mask it.
    let all_failed = progress.facts_written == 0 && progress.chunks_failed > 0;
    let clean = aborted.is_none() && !all_failed && progress.chunks_failed == 0;
    progress.status = if aborted.is_some() || all_failed {
        METRICS.retain_errors.fetch_add(1, Ordering::Relaxed);
        METRICS.retain_jobs_failed.fetch_add(1, Ordering::Relaxed);
        JobStatus::Failed
    } else if progress.chunks_failed > 0 {
        // Some facts were written and some chunk's were not. `Done` was the
        // answer here and it was the wrong one: measured on the live daemon,
        // four of the last twelve jobs finished `done` having lost chunks, 16
        // of 95 chunks overall, with the loss visible only in a counter.
        //
        // `clean` above already withholds the content hash on this path, so
        // re-posting the transcript re-ingests it rather than being dismissed
        // as a duplicate. The gap was never the recovery route — it was that
        // nothing said recovery was needed.
        JobStatus::Partial
    } else {
        JobStatus::Done
    };
    progress.error = aborted.or(last_error);

    // Review HIGH 1: the content hash is the "this transcript is fully
    // ingested" marker, so it is written HERE and only on a clean run. A job
    // that failed or skipped a chunk leaves the document hash-less, and
    // re-POSTing the same transcript starts a fresh job instead of being
    // dismissed as a duplicate.
    //
    // HK-1a rides along on exactly the same condition: `sessions.
    // confirmed_offset` is the client-visible form of the same claim, so it
    // advances here and nowhere else on this path. A job that failed a chunk
    // leaves the durable cursor behind the optimistic one, and that gap is
    // what tells the hook (and the dashboard) there is work to re-send. Both
    // writes share one `spawn_blocking`: two small transactions, one hop off
    // the reactor.
    if clean {
        let db = state.db.clone();
        let document_id = task.document_id;
        let hash = task.content_hash.clone();
        let bank_id = task.bank_id.clone();
        let session_id = task.session_id.clone();
        // Both ends. The confirm is **range-guarded** rather than
        // unconditional: `MAX` merging meant a clean job covering
        // 1008399..2163159 confirmed straight over an earlier job's unsettled
        // 0..1008399, erasing the evidence of a gap it never carried. A job
        // that cannot name its start does not confirm at all — over-reporting
        // outstanding work is the safe direction here.
        let range = task.offset_from.zip(task.byte_offset);
        let stored = tokio::task::spawn_blocking(move || {
            documents::set_content_hash(&db, document_id, &hash)?;
            if let (Some(session_id), Some(range)) = (session_id, range) {
                memgarden_store::sessions::upsert(
                    &db,
                    &bank_id,
                    &memgarden_store::sessions::SessionUpdate {
                        session_id: &session_id,
                        confirm_range: Some(range),
                        ..Default::default()
                    },
                )?;
            }
            Ok::<(), memgarden_core::Error>(())
        })
        .await;
        match stored {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(job_id = %task.job_id, error = %e, "failed to record clean-run completion")
            }
            Err(e) => {
                tracing::warn!(job_id = %task.job_id, error = %e, "completion task panicked")
            }
        }
    }

    flush(state, &task.job_id, &progress).await;
    tracing::info!(
        job_id = %task.job_id,
        bank_id = %task.bank_id,
        status = progress.status.as_str(),
        chunks_total = progress.chunks_total,
        chunks_done = progress.chunks_done,
        chunks_skipped = progress.chunks_skipped,
        chunks_failed = progress.chunks_failed,
        facts_written = progress.facts_written,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "retain job finished"
    );
}

/// Non-blocking check on the pinned shutdown future.
fn futures_ready(shutdown: &mut std::pin::Pin<&mut impl std::future::Future<Output = ()>>) -> bool {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    shutdown.as_mut().poll(&mut cx).is_ready()
}

/// One chunk's extraction, with a single retry for the two "the slot wasn't
/// available" errors (review HIGH 2). `chat_json_background` waits untimed
/// for the permit, so `Busy` should now be unreachable; `Deadline` can still
/// fire on a genuinely stuck upstream and is worth one more attempt before
/// the chunk is written off.
async fn extract_chunk(
    state: &AppState,
    task: &RetainTask,
    chunk: &str,
    known: &[KnownFact],
) -> Result<Vec<ParsedFact>, crate::ollama::OllamaError> {
    let first = extract::extract(
        &state.ollama,
        chunk,
        Some(task.event_date_ms),
        task.mission.as_deref(),
        true,
        known,
    )
    .await;
    match first {
        Err(e @ (crate::ollama::OllamaError::Busy | crate::ollama::OllamaError::Deadline(_))) => {
            tracing::warn!(job_id = %task.job_id, error = %e, "retain chunk retrying once after a slot error");
            extract::extract(
                &state.ollama,
                chunk,
                Some(task.event_date_ms),
                task.mission.as_deref(),
                true,
                known,
            )
            .await
        }
        other => other,
    }
}

/// CE-12: the existing facts this chunk might be retracting.
///
/// A vector KNN over the chunk's own text, not a second LLM call. The chunk
/// is already the topic statement — whatever it is about, the bank's nearest
/// facts are the only ones a retraction in it could plausibly reach — and one
/// embed plus one brute-force KNN costs single-digit milliseconds against an
/// extraction call that costs seconds. A second LLM pass per chunk would have
/// roughly doubled retain wall-clock, on the path AC-1 already found to be
/// throughput-bound.
///
/// Known ceiling: bge-small sees the first ~512 tokens, so on a 3,000-character
/// chunk the candidates are drawn from its opening. That is the right end —
/// chunks lead with the exchange that prompted them — but it is a limit, not a
/// property.
///
/// Every failure returns an empty list. Candidates are an *enrichment* of a
/// prompt that worked without them, so nothing here may fail a chunk.
pub async fn candidate_facts(state: &AppState, bank_id: &str, chunk: &str) -> Vec<KnownFact> {
    let k = state.cfg.retain.supersession_candidates;
    if !state.cfg.retain.detect_supersession || k == 0 {
        return Vec::new();
    }
    let Some(vector) = crate::mental::embed_one(state, chunk.to_string()).await else {
        return Vec::new();
    };
    let db = state.db.clone();
    let bank_id = bank_id.to_string();
    let rows = tokio::task::spawn_blocking(move || {
        let hits = memgarden_store::search::knn(&db, &bank_id, &vector, k)?;
        let ids: Vec<i64> = hits.into_iter().map(|(id, _)| id).collect();
        // `hydrate` applies CE-12's own filter, so an already-retracted fact
        // is never offered as a retraction target and the chains stay flat.
        memgarden_store::search::hydrate(&db, &bank_id, &ids)
    })
    .await;
    match rows {
        Ok(Ok(rows)) => rows
            .into_iter()
            .map(|row| KnownFact {
                id: row.id,
                text: row.text,
            })
            .collect(),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "supersession candidate lookup failed");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, "supersession candidate task panicked");
            Vec::new()
        }
    }
}

/// Progress flush. Best-effort: a job whose row cannot be updated has still
/// done real work, and failing the whole run over a status write would be
/// worse than a stale row.
async fn flush(state: &AppState, job_id: &str, progress: &JobProgress) {
    let db = state.db.clone();
    let job_id_owned = job_id.to_string();
    let progress = progress.clone();
    let result =
        tokio::task::spawn_blocking(move || retain_jobs::update(&db, &job_id_owned, &progress))
            .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(job_id = %job_id, error = %e, "retain job progress update failed")
        }
        Err(e) => tracing::warn!(job_id = %job_id, error = %e, "retain job progress task panicked"),
    }
}

/// Writes one chunk's facts as memory nodes in a single transaction.
/// Embeddings stay NULL — see the module doc.
async fn write_facts(
    state: &AppState,
    task: &RetainTask,
    facts: Vec<ParsedFact>,
    abs_index_base: usize,
    known: &[KnownFact],
) -> memgarden_core::error::Result<usize> {
    if facts.is_empty() {
        return Ok(0);
    }
    let drafts: Vec<NodeDraft> = facts
        .iter()
        .enumerate()
        .map(|(i, fact)| NodeDraft::build(fact, task, abs_index_base + i))
        .collect();

    let db = state.db.clone();
    let bank_id = task.bank_id.clone();
    let context = task.context.clone();
    let document_id = task.document_id;
    let tags = task.tags.clone();
    // `drafts` comes back out of the closure: `write_graph` below needs each
    // draft's fact_type and event_date, and moving them in and back beats
    // cloning the batch.
    let (ids, drafts) = tokio::task::spawn_blocking(move || {
        let items: Vec<NewNodeWithTags> = drafts
            .iter()
            .map(|d| NewNodeWithTags {
                node: NewNode {
                    bank_id: &bank_id,
                    document_id: Some(document_id),
                    fact_type: d.fact_type,
                    text: &d.text,
                    context: context.as_deref(),
                    event_date: d.event_date,
                    occurred_start: d.occurred_start,
                    occurred_end: d.occurred_end,
                    mentioned_at: d.mentioned_at,
                    metadata: Some(&d.metadata),
                    expires_at: d.expires_at,
                },
                tags: &tags,
            })
            .collect();
        let ids = nodes::insert_batch(&db, &items)?;
        drop(items);
        Ok::<_, memgarden_core::Error>((ids, drafts))
    })
    .await
    .map_err(|e| memgarden_core::Error::Storage(format!("node write task panicked: {e}")))??;

    METRICS
        .nodes_written
        .fetch_add(ids.len() as u64, Ordering::Relaxed);

    // CE-7: entities, co-occurrences and the two write-time link types.
    // Best-effort — the facts are already committed and a graph that is one
    // chunk short is worth more than a chunk that never lands.
    if let Err(e) = write_graph(state, task, &facts, &drafts, &ids).await {
        tracing::warn!(job_id = %task.job_id, error = %e, "retain graph write failed");
    }

    // CE-12. After the facts are committed, never before: the retraction
    // points at a node that has to exist, and a chunk that fails between the
    // two leaves a fact unretracted rather than a dangling reference.
    let retracted = apply_supersession(state, task, &facts, &ids, known).await;

    // E4: announce *after* the graph write, so a subscriber that immediately
    // asks for these ids finds their links already there. Semantic links are
    // not among them — those land on the backlog worker's next tick, which is
    // minutes away and is not what AC-4's five seconds is about.
    let mut announced = ids.clone();
    // The retracted nodes changed too — a dashboard that is not told keeps
    // showing them as live, which is the state this whole change exists to
    // stop being invisible.
    announced.extend(retracted);
    crate::events::publish(&state.events, &task.bank_id, "nodes", announced);

    Ok(ids.len())
}

/// `(retracted node id, replacement node id)` for every position the model
/// named — the whole of CE-12's index-to-rowid mapping, kept pure so it can be
/// tested without a daemon.
///
/// `zip` is load-bearing twice over. `ids` comes back from `insert_batch` in
/// input order, so fact *i* is node *i*; and `known.get` is a lookup rather
/// than an index, so a position that survived parsing but outruns this
/// particular list still lands nowhere.
fn supersession_pairs(facts: &[ParsedFact], ids: &[i64], known: &[KnownFact]) -> Vec<(i64, i64)> {
    facts
        .iter()
        .zip(ids)
        .flat_map(|(fact, new_id)| {
            fact.supersedes
                .iter()
                .filter_map(move |&idx| known.get(idx).map(|k| (k.id, *new_id)))
        })
        .collect()
}

/// CE-12: turns each new fact's `supersedes` positions into rows marked
/// retracted, and returns the node ids that actually changed.
///
/// Best-effort by the same rule as the graph write above: the facts are
/// committed, and a chunk that lost its retractions is worth more than a
/// chunk that never landed. Every correctness guard — same bank, not already
/// retracted, replacement not older than what it replaces — lives in
/// `nodes::mark_superseded`'s `WHERE` clause, because this side of the call
/// is holding a 14B model's answer.
async fn apply_supersession(
    state: &AppState,
    task: &RetainTask,
    facts: &[ParsedFact],
    ids: &[i64],
    known: &[KnownFact],
) -> Vec<i64> {
    let pairs = supersession_pairs(facts, ids, known);
    if pairs.is_empty() {
        return Vec::new();
    }

    let db = state.db.clone();
    let bank_id = task.bank_id.clone();
    let claimed = pairs.clone();
    let applied = tokio::task::spawn_blocking(move || {
        memgarden_store::nodes::mark_superseded(&db, &bank_id, &claimed)
    })
    .await;

    match applied {
        Ok(Ok(changed)) => {
            // Both numbers, always: `claimed > changed` is a guard doing its
            // job, and the day the gap is the whole batch it needs to be
            // readable in the log rather than inferred from a silence.
            tracing::info!(
                job_id = %task.job_id,
                bank_id = %task.bank_id,
                claimed = pairs.len(),
                changed,
                "supersession applied"
            );
            if changed == 0 {
                Vec::new()
            } else {
                pairs.iter().map(|&(old, _)| old).collect()
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(job_id = %task.job_id, error = %e, "supersession write failed");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(job_id = %task.job_id, error = %e, "supersession task panicked");
            Vec::new()
        }
    }
}

/// Entity resolution + upsert, then the temporal and causal links, for one
/// chunk's freshly written nodes (CE-7 / PR B5).
///
/// Semantic links are **not** created here: B3 writes `embedding = NULL`, so
/// a retain-time KNN would always find nothing. They are created by the
/// backlog worker right after each embedding commit instead
/// (`embed_task::on_batch_embedded`, Critic Revision R2 / legacy's streaming
/// design, `orchestrator.py:418-420`).
async fn write_graph(
    state: &AppState,
    task: &RetainTask,
    facts: &[ParsedFact],
    drafts: &[NodeDraft],
    ids: &[i64],
) -> memgarden_core::error::Result<()> {
    let db = state.db.clone();
    let bank_id = task.bank_id.clone();
    let now = memgarden_core::now_ms();

    // What the entity pass needs: (node id, raw mentions, the fact's date).
    let mentions: Vec<(i64, Vec<String>, Option<i64>)> = facts
        .iter()
        .zip(drafts)
        .zip(ids)
        .filter(|((fact, _), _)| !fact.entities.is_empty())
        .map(|((fact, draft), id)| (*id, fact.entities.clone(), draft.event_date))
        .collect();

    // What the temporal pass needs, and the window it has to look at.
    let timed: Vec<links::TimedNode> = drafts
        .iter()
        .zip(ids)
        .filter_map(|(d, id)| {
            d.event_date.map(|event_date| links::TimedNode {
                id: *id,
                fact_type: d.fact_type.as_str().to_string(),
                event_date,
            })
        })
        .collect();
    let causal = links::causal_links(facts, ids);

    tokio::task::spawn_blocking(move || {
        if !mentions.is_empty() {
            let ctx = store_graph::load_resolution_context(&db, &bank_id)?;
            // Each fact carries its *own* date into first_seen/last_seen and
            // last_cooccurred (`entity_processing.py:28`); `now` is only the
            // fallback for a fact with no date at all. A chunk-wide stamp
            // would flatten the 0.2 temporal term, which is frequently what
            // carries a resolution over the 0.6 gate (review MEDIUM 3).
            let resolved: Vec<store_graph::EntityMentions> = mentions
                .iter()
                .map(|(id, raw, date)| {
                    (
                        *id,
                        entities::resolve_fact(raw, *date, &ctx),
                        date.unwrap_or(now),
                    )
                })
                .filter(|(_, names, _)| !names.is_empty())
                .collect();
            store_graph::write_entities(&db, &bank_id, &resolved, now)?;
        }

        let mut batch = causal;
        if !timed.is_empty() {
            const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
            let lo = timed.iter().map(|n| n.event_date).min().unwrap_or(now) - WINDOW_MS;
            let hi = timed.iter().map(|n| n.event_date).max().unwrap_or(now) + WINDOW_MS;
            let window: Vec<links::TimedNode> =
                store_graph::nodes_in_window(&db, &bank_id, lo, hi)?
                    .into_iter()
                    .map(|(id, fact_type, event_date)| links::TimedNode {
                        id,
                        fact_type,
                        event_date,
                    })
                    .collect();
            batch.extend(links::temporal_links(&timed, &window));
        }
        let written = store_graph::insert_links(&db, &batch, now)?;
        // Temporal links from the retain path; the semantic pass meters its
        // own in `embed_task`. Both feed one counter because both are "links
        // this daemon wrote", which is what the dashboard claims to show.
        METRICS
            .links_written
            .fetch_add(written as u64, Ordering::Relaxed);
        Ok(())
    })
    .await
    .map_err(|e| memgarden_core::Error::Storage(format!("graph write task panicked: {e}")))?
}

struct NodeDraft {
    text: String,
    fact_type: FactType,
    event_date: Option<i64>,
    occurred_start: Option<i64>,
    occurred_end: Option<i64>,
    mentioned_at: Option<i64>,
    metadata: String,
    expires_at: Option<i64>,
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

        // CE-12. The model names the last day the fact is true, so the row
        // expires at the END of that day: a bare date parses to its own
        // midnight, which would kill "on leave until March 3" during March 2.
        // No offset is applied — an expiry is a wall-clock deadline, not a
        // position in the document's fact ordering.
        let expires_at = fact
            .expires_at
            .as_deref()
            .and_then(parse_iso_ms)
            .map(|ms| ms + DAY_MS);

        NodeDraft {
            text: fact.text.clone(),
            fact_type: fact.fact_type,
            event_date,
            occurred_start,
            occurred_end,
            mentioned_at,
            metadata: Value::Object(meta).to_string(),
            expires_at,
        }
    }
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
        assert!(
            !initial.transcript.contains("message number 44"),
            "keeps the LAST 5"
        );

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
    fn queued_byte_budget_admits_then_rejects_then_recovers() {
        // Process-wide static; this is the only test that touches it.
        assert_eq!(queued_bytes(), 0);
        assert!(try_reserve_bytes(MAX_QUEUED_BYTES));
        assert!(
            !try_reserve_bytes(1),
            "one byte over the budget must be refused (429)"
        );
        assert_eq!(
            queued_bytes(),
            MAX_QUEUED_BYTES,
            "a refused reservation must roll itself back"
        );
        release_bytes(MAX_QUEUED_BYTES);
        assert_eq!(queued_bytes(), 0);
        assert!(try_reserve_bytes(1));
        release_bytes(1);
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
            content_hash: "hash".to_string(),
            byte_offset: None,
            offset_from: None,
        }
    }

    fn fact(text: &str, start: Option<&str>) -> ParsedFact {
        ParsedFact {
            text: text.to_string(),
            fact_type: FactType::World,
            fact_kind: if start.is_some() {
                "event"
            } else {
                "conversation"
            }
            .to_string(),
            occurred_start: start.map(str::to_string),
            occurred_end: start.map(str::to_string),
            where_field: None,
            entities: vec![],
            causal_relations: vec![],
            supersedes: vec![],
            expires_at: None,
            superseded_quote: None,
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

    /// CE-12's index-to-rowid mapping. The positions are already range-checked
    /// by `parse`; what this pins is that they are matched to the *right* new
    /// node, which only holds because `insert_batch` returns ids in input
    /// order.
    #[test]
    fn supersession_pairs_map_positions_to_the_fact_that_named_them() {
        let mut a = fact("the correction", None);
        a.supersedes = vec![0, 2];
        let b = fact("an unrelated new fact", None);
        let mut c = fact("a second correction", None);
        c.supersedes = vec![1];

        let known: Vec<KnownFact> = [901, 902, 903]
            .iter()
            .map(|&id| KnownFact {
                id,
                text: format!("stored {id}"),
            })
            .collect();

        let pairs = supersession_pairs(&[a, b, c], &[11, 12, 13], &known);
        assert_eq!(pairs, vec![(901, 11), (903, 11), (902, 13)]);
    }

    /// A position that outlived the list it indexed writes nothing. `parse`
    /// already bounds them, so this is the second gate on the same mistake —
    /// and the one that holds if a future caller passes a different list to
    /// `extract` than it passes to `write_facts`.
    #[test]
    fn a_position_past_the_candidate_list_is_dropped_not_wrapped() {
        let mut a = fact("the correction", None);
        a.supersedes = vec![0, 7];
        let known = vec![KnownFact {
            id: 901,
            text: "stored".to_string(),
        }];
        assert_eq!(supersession_pairs(&[a], &[11], &known), vec![(901, 11)]);
    }

    /// The writer resolves the model's date the way it resolves
    /// `occurred_start` — and pushes it to the END of the named day, so a
    /// fact true "until March 3" is still live during March 3.
    #[test]
    fn an_expiry_date_lasts_through_the_day_it_names() {
        let mut f = fact("on leave until March 3", None);
        f.expires_at = Some("2026-03-03".to_string());
        let draft = NodeDraft::build(&f, &task(), 0);

        let march_3_midnight = parse_iso_ms("2026-03-03").unwrap();
        assert_eq!(draft.expires_at, Some(march_3_midnight + DAY_MS));
        assert!(draft.expires_at.unwrap() > parse_iso_ms("2026-03-03T23:59:59Z").unwrap());
    }

    #[test]
    fn a_fact_with_no_expiry_gets_none_and_an_unparseable_one_too() {
        assert!(
            NodeDraft::build(&fact("durable", None), &task(), 0)
                .expires_at
                .is_none()
        );
        let mut f = fact("nonsense date", None);
        f.expires_at = Some("someday".to_string());
        assert!(NodeDraft::build(&f, &task(), 0).expires_at.is_none());
    }
}
