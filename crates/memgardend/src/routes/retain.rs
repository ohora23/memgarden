//! `POST /v1/banks/{bank_id}/retain` and `GET /v1/retain/{job_id}` (CE-5b).
//!
//! The Phase C hook posts the raw transcript and nothing else (plan decision
//! #4): every cap, the `file:` tags and the `retain_cap_saving` ledger row
//! are applied here, server-side.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use memgarden_core::metrics::METRICS;
use memgarden_store::{banks, documents, metrics_store, retain_jobs};

use crate::error::{ApiError, join_err};
use crate::json::ApiJson;
use crate::retain::{IngestPlan, RetainTask};
use crate::state::AppState;

/// Hard ceiling on one retain request body. Every cap MemGarden applies —
/// the backfill cap, both tool-input caps — runs *after* the body is parsed,
/// so this limit has to accommodate the **uncapped** transcript a client
/// sends, not the much smaller thing that survives capping. That is the
/// whole reason it is 16x axum's default: the 102MB-transcript incident is
/// exactly this shape. The counterweight is that one request must never be
/// able to OOM the daemon, which is what puts the ceiling at 32MB. Pair with
/// `retain::MAX_QUEUED_BYTES`, which bounds the *sum* of what is waiting in
/// the queue.
/// The route-level `DefaultBodyLimit` in `routes/mod.rs` enforces it.
pub const MAX_RETAIN_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct RetainRequest {
    /// Raw Claude Code messages: `[{role, content}, ...]`, `content` either a
    /// string or a content-block array.
    pub messages: Vec<Value>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Document key within the bank; defaults to `session_id`. Re-sending
    /// byte-identical content under the same key is a no-op.
    #[serde(default)]
    pub document_id: Option<String>,
    /// Working directory, used to relativize `file:` tags.
    #[serde(default)]
    pub cwd: Option<String>,
    /// `true` only for a session's FIRST retain. Gates the backfill cap
    /// (`retain.max_initial_messages`), which keeps the last N messages.
    ///
    /// **Required** — deliberately not `#[serde(default)]`. A defaulted
    /// value means a caller that forgets the field silently takes the
    /// uncapped branch, which is exactly the 102MB-transcript case the cap
    /// exists to prevent. An absent field is a 400.
    pub is_initial: bool,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// Unix milliseconds; the extractor's "Event Date" and every fact's
    /// `mentioned_at` base. Defaults to now.
    #[serde(default)]
    pub event_date: Option<i64>,
    /// Extraction mission. Precedence: this field -> the bank's
    /// `disposition.retain_mission` -> `[profile] retain_mission`.
    #[serde(default)]
    pub mission: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RetainResponse {
    /// `accepted` (queued), `skipped` (nothing worth retaining) or
    /// `duplicate` (byte-identical content already stored).
    pub status: &'static str,
    pub job_id: Option<String>,
    pub document_id: Option<i64>,
    pub raw_tokens: u64,
    pub capped_tokens: u64,
    pub saved_tokens: u64,
    pub saving_ratio: f64,
}

pub async fn retain(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    ApiJson(body): ApiJson<RetainRequest>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    METRICS.retain_requests.fetch_add(1, Ordering::Relaxed);
    let result = retain_inner(state, bank_id, body).await;
    METRICS
        .retain_latency
        .record_us(started.elapsed().as_micros() as u64);
    if result.is_err() {
        METRICS.retain_errors.fetch_add(1, Ordering::Relaxed);
    }
    result
}

async fn retain_inner(
    state: AppState,
    bank_id: String,
    body: RetainRequest,
) -> Result<Response, ApiError> {
    if body.messages.is_empty() {
        return Err(memgarden_core::Error::Invalid("messages must not be empty".to_string()).into());
    }

    // Reserve the queue slot BEFORE doing any DB work: a full queue must be
    // a clean 429, not an orphaned document + job row.
    let permit = match state.retain_tx.try_reserve() {
        Ok(permit) => permit,
        Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {
            return Err(ApiError::too_many_requests(
                "retain queue is full; retry shortly",
            ));
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {
            return Err(ApiError::unavailable("retain worker is not running"));
        }
    };

    let event_date_ms = body.event_date.unwrap_or_else(memgarden_core::now_ms);
    let job_id = Uuid::now_v7().to_string();
    let doc_key = body
        .document_id
        .clone()
        .or_else(|| body.session_id.clone())
        .unwrap_or_else(|| job_id.clone());

    // Normalization + double tokenization + four SQLite round trips: one
    // spawn_blocking, not five.
    let prepared = {
        let state = state.clone();
        let bank_id = bank_id.clone();
        let job_id = job_id.clone();
        let doc_key = doc_key.clone();
        tokio::task::spawn_blocking(move || {
            prepare(&state, &bank_id, &job_id, &doc_key, event_date_ms, &body)
        })
        .await
        .map_err(join_err)??
    };

    let (code, response) = match prepared {
        Prepared::Skipped => (
            StatusCode::OK,
            RetainResponse {
                status: "skipped",
                job_id: None,
                document_id: None,
                raw_tokens: 0,
                capped_tokens: 0,
                saved_tokens: 0,
                saving_ratio: 0.0,
            },
        ),
        Prepared::Duplicate { plan, document_id } => (
            StatusCode::OK,
            RetainResponse {
                status: "duplicate",
                job_id: None,
                document_id: Some(document_id),
                raw_tokens: plan.raw_tokens,
                capped_tokens: plan.capped_tokens,
                saved_tokens: plan.saved_tokens(),
                saving_ratio: plan.saving_ratio(),
            },
        ),
        Prepared::Queued { plan, task } => {
            let document_id = task.document_id;
            // Byte-side admission: the 32-job queue bound says nothing about
            // RAM. Rejecting here leaves the job row behind on purpose —
            // marked failed below, so the caller can see why.
            if !crate::retain::try_reserve_bytes(task.transcript.len()) {
                let job_id_owned = task.job_id.clone();
                let db = state.db.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    memgarden_store::retain_jobs::update(
                        &db,
                        &job_id_owned,
                        &memgarden_store::retain_jobs::JobProgress {
                            status: memgarden_store::retain_jobs::JobStatus::Failed,
                            error: Some("retain queue byte budget exhausted".to_string()),
                            ..Default::default()
                        },
                    )
                })
                .await;
                return Err(ApiError::too_many_requests(
                    "retain queue byte budget exhausted; retry shortly",
                ));
            }
            // The permit was reserved before any DB work, so this cannot
            // fail and cannot block.
            permit.send(*task);
            (
                StatusCode::ACCEPTED,
                RetainResponse {
                    status: "accepted",
                    job_id: Some(job_id),
                    document_id: Some(document_id),
                    raw_tokens: plan.raw_tokens,
                    capped_tokens: plan.capped_tokens,
                    saved_tokens: plan.saved_tokens(),
                    saving_ratio: plan.saving_ratio(),
                },
            )
        }
    };
    Ok((code, Json(response)).into_response())
}

enum Prepared {
    /// Nothing worth retaining (empty after role filtering, or under the
    /// 10-character floor). legacy just logs and returns.
    Skipped,
    Duplicate {
        plan: IngestPlan,
        document_id: i64,
    },
    Queued {
        plan: IngestPlan,
        task: Box<RetainTask>,
    },
}

/// The whole blocking half: bank lookup, normalization + token accounting,
/// document upsert (content-hash dedup), ledger row, job row.
fn prepare(
    state: &AppState,
    bank_id: &str,
    job_id: &str,
    doc_key: &str,
    event_date_ms: i64,
    body: &RetainRequest,
) -> Result<Prepared, ApiError> {
    let bank = banks::get(&state.db, bank_id)?
        .ok_or_else(|| ApiError::not_found(format!("bank not found: {bank_id}")))?;

    let cfg = &state.cfg.retain;
    let cwd = body.cwd.as_deref().unwrap_or("");
    let Some(plan) = crate::retain::plan_ingest(&body.messages, cwd, body.is_initial, cfg) else {
        return Ok(Prepared::Skipped);
    };

    let doc_metadata = document_metadata(body, &plan);
    let upsert = documents::upsert(
        &state.db,
        bank_id,
        doc_key,
        body.session_id.as_deref(),
        &doc_metadata,
        &plan.content_hash,
    )?;

    if upsert.unchanged {
        // legacy retain dedup is an exact SHA-256 match and nothing else —
        // never cosine (port-brief gotcha #5). `unchanged` can only be true
        // once a previous job stamped the hash on completion, so a partially
        // failed ingest is never mistaken for a duplicate (review HIGH 1).
        // Nothing is ingested here, so this records no tokens and no ledger
        // row: the ledger is a record of work avoided, not of requests
        // received.
        let doc_key_log: String = doc_key.chars().take(128).collect();
        tracing::debug!(bank_id, doc_key = ?doc_key_log, "retain skipped: identical content hash");
        return Ok(Prepared::Duplicate {
            plan,
            document_id: upsert.id,
        });
    }

    METRICS
        .retain_tokens_raw
        .fetch_add(plan.raw_tokens, Ordering::Relaxed);
    METRICS
        .retain_tokens_capped
        .fetch_add(plan.capped_tokens, Ordering::Relaxed);

    // The MX-1 deferral, closed: the cap saving auto-populates the ledger.
    // Nothing saved -> no row, so the ledger stays a record of real benefit.
    if plan.saved_tokens() > 0 {
        let detail = json!({
            "raw_tokens": plan.raw_tokens,
            "capped_tokens": plan.capped_tokens,
            "saved": plan.saved_tokens(),
            "ratio": plan.saving_ratio(),
            "session_id": body.session_id,
        })
        .to_string();
        metrics_store::insert_ledger(
            &state.db,
            "retain_cap_saving",
            Some(bank_id),
            Some(&detail),
        )?;
        METRICS.retain_cap_savings.fetch_add(1, Ordering::Relaxed);
    }

    let detail = json!({
        "raw_tokens": plan.raw_tokens,
        "capped_tokens": plan.capped_tokens,
        "saved_tokens": plan.saved_tokens(),
        "saving_ratio": plan.saving_ratio(),
        "message_count": plan.message_count,
    })
    .to_string();
    retain_jobs::insert(
        &state.db,
        job_id,
        bank_id,
        Some(upsert.id),
        body.session_id.as_deref(),
        Some(&detail),
    )?;

    // Every tag source goes through `sanitize_tags` together, not just the
    // caller-supplied ones (review MEDIUM): `session_id` is raw request
    // input and a `file:` path is transcript-derived, so both can carry
    // control characters or absurd lengths, and sanitizing only `body.tags`
    // left the per-tag caps AND the total count cap bypassable.
    //
    // Order is priority under the count cap: the session tag is AC-4's
    // filter data path (Critic Revision R15, legacy template parity
    // `retain.py:201-206`) and the `file:` tags are the feature, so a caller
    // who floods `body.tags` loses their own tags first, not ours.
    let mut tags: Vec<String> = Vec::new();
    if let Some(session_id) = &body.session_id {
        tags.push(format!("session:{session_id}"));
    }
    tags.extend(plan.file_tags.iter().cloned());
    tags.extend(body.tags.iter().cloned());
    let tags = sanitize_tags(&tags);

    let task = RetainTask {
        job_id: job_id.to_string(),
        bank_id: bank_id.to_string(),
        document_id: upsert.id,
        session_id: body.session_id.clone(),
        transcript: plan.transcript.clone(),
        event_date_ms,
        mission: resolve_mission(state, &bank, body),
        context: body.context.clone(),
        tags,
        content_hash: plan.content_hash.clone(),
    };
    Ok(Prepared::Queued {
        plan,
        task: Box::new(task),
    })
}

/// Mission precedence: request -> the bank's `disposition.retain_mission` ->
/// `[profile] retain_mission`. Per-bank overrides live in existing columns;
/// no new schema (plan §PR B3).
fn resolve_mission(
    state: &AppState,
    bank: &memgarden_store::models::Bank,
    body: &RetainRequest,
) -> Option<String> {
    if let Some(m) = body.mission.as_deref().filter(|m| !m.is_empty()) {
        return Some(m.to_string());
    }
    let from_bank = bank
        .disposition
        .as_deref()
        .and_then(|d| serde_json::from_str::<Value>(d).ok())
        .and_then(|d| {
            d.get("retain_mission")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|m| !m.is_empty());
    from_bank.or_else(|| {
        Some(state.cfg.profile.retain_mission.clone()).filter(|m| !m.is_empty())
    })
}

/// Document metadata: the caller's extras plus the fields retain owns.
///
/// Deliberately does NOT carry `content_sha256` — that is stamped by
/// `documents::set_content_hash` when the job finishes cleanly (review
/// HIGH 1). `files_modified` is the comma-joined form the fork stores
/// alongside the `file:` tags (`retain.py:240`).
fn document_metadata(body: &RetainRequest, plan: &IngestPlan) -> String {
    let mut meta = body.metadata.clone();
    meta.remove("content_sha256");
    meta.insert("message_count".to_string(), json!(plan.message_count));
    if let Some(session_id) = &body.session_id {
        meta.insert("session_id".to_string(), json!(session_id));
    }
    if !plan.files_modified.is_empty() {
        meta.insert(
            "files_modified".to_string(),
            json!(plan.files_modified.join(",")),
        );
    }
    Value::Object(meta).to_string()
}

/// Caller-supplied tags are untrusted and land in `node_tags` for every fact
/// of the job. Drop the empty and the unprintable, bound the length, and cap
/// the count (security review): an unbounded tag list multiplies by fact
/// count in the database.
const MAX_TAGS: usize = 32;
const MAX_TAG_CHARS: usize = 128;

fn sanitize_tags(raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|t| t.trim())
        .filter(|t| {
            !t.is_empty() && t.chars().count() <= MAX_TAG_CHARS && !t.chars().any(|c| c.is_control())
        })
        .take(MAX_TAGS)
        .map(str::to_string)
        .collect()
}

pub async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = state.db.clone();
    let lookup = job_id.clone();
    let job = tokio::task::spawn_blocking(move || retain_jobs::get(&db, &lookup))
        .await
        .map_err(join_err)??
        .ok_or_else(|| ApiError::not_found(format!("retain job not found: {job_id}")))?;

    Ok(Json(json!({
        "job_id": job.job_id,
        "bank_id": job.bank_id,
        "document_id": job.document_id,
        "session_id": job.session_id,
        "status": job.status,
        "chunks_total": job.chunks_total,
        "chunks_done": job.chunks_done,
        "chunks_skipped": job.chunks_skipped,
        "chunks_failed": job.chunks_failed,
        "facts_written": job.facts_written,
        "error": job.error,
        "detail": job.detail.and_then(|d| serde_json::from_str::<Value>(&d).ok()),
        "created_at": job.created_at,
        "updated_at": job.updated_at,
    })))
}
