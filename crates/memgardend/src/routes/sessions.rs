//! Session/turn state (HK-1a): the daemon-side mirror of the Phase C hook's
//! per-session file.
//!
//! Three endpoints, two callers:
//! * `POST /v1/banks/{bank_id}/sessions` — the `session-start` hook and the
//!   detached `session-end` child. Two calls per session, off the per-prompt
//!   path.
//! * `GET  /v1/banks/{bank_id}/sessions?limit=&active=` — the dashboard (DB-1).
//! * `GET  /v1/banks/{bank_id}/sessions/{session_id}` — the hook's recovery
//!   source when its local state file is missing (C2b).
//!
//! The retain path writes the same row, but through `routes/retain.rs`
//! rather than here — see that file and `store::sessions` for which writer
//! owns which field.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use memgarden_store::sessions::{self as store, Session, SessionUpdate};

use crate::error::{ApiError, join_err};
use crate::json::ApiJson;
use crate::state::AppState;

/// Page-size ceiling, same shape as the mental-model list.
const MAX_LIST_LIMIT: usize = 200;
const DEFAULT_LIST_LIMIT: usize = 50;

/// Bounded because they are stored verbatim and echoed on every read. Paths
/// only; nothing here is interpreted as a filesystem location by the daemon.
const MAX_PATH_BYTES: usize = 4096;
/// `startup|resume|clear|compact|fork` / `clear|resume|logout|…`. The daemon
/// deliberately does **not** enum-check these: Claude Code owns both
/// vocabularies and adds to them (`bypass_permissions_disabled` is recent),
/// and a mirror that 400s on a value it has not heard of would break the
/// hook on a Claude Code upgrade. Bounded, not validated.
const MAX_REASON_BYTES: usize = 64;

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub bank_id: String,
    pub session_id: String,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    pub source: Option<String>,
    pub end_reason: Option<String>,
    pub turns: i64,
    pub retains: i64,
    pub chunk_index: i64,
    /// Optimistic cursor: bytes the hook has POSTed.
    pub byte_offset: i64,
    /// Durable cursor: bytes whose ingestion is settled.
    pub confirmed_offset: i64,
    /// `byte_offset - confirmed_offset`. Served rather than left to the
    /// caller because it is the whole point of carrying two cursors, and a
    /// dashboard that has to compute it is a dashboard that will forget to.
    pub inflight_bytes: i64,
    pub messages_sent: i64,
    pub compactions: i64,
    pub started_at: i64,
    pub last_seen_at: i64,
    pub ended_at: Option<i64>,
}

impl From<Session> for SessionResponse {
    fn from(s: Session) -> Self {
        let inflight_bytes = s.inflight_bytes();
        SessionResponse {
            bank_id: s.bank_id,
            session_id: s.session_id,
            cwd: s.cwd,
            transcript_path: s.transcript_path,
            source: s.source,
            end_reason: s.end_reason,
            turns: s.turns,
            retains: s.retains,
            chunk_index: s.chunk_index,
            byte_offset: s.byte_offset,
            confirmed_offset: s.confirmed_offset,
            inflight_bytes,
            messages_sent: s.messages_sent,
            compactions: s.compactions,
            started_at: s.started_at,
            last_seen_at: s.last_seen_at,
            ended_at: s.ended_at,
        }
    }
}

/// Everything the hook is allowed to mirror.
///
/// **`confirmed_offset` is deliberately absent, and this is a security
/// property rather than an oversight.** The durable cursor's only meaning is
/// "ingestion is a settled fact for these bytes", and that fact is
/// established by the retain worker, never asserted by a client. A field here
/// would let a buggy hook mark unwritten bytes as durable and silently lose
/// them. `retains` and `messages_sent` are absent for the same reason: the
/// daemon counts what it accepted.
#[derive(Debug, Deserialize)]
pub struct UpsertSessionRequest {
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// SessionStart source. First write wins.
    #[serde(default)]
    pub source: Option<String>,
    /// SessionEnd reason. Last write wins.
    #[serde(default)]
    pub end_reason: Option<String>,
    #[serde(default)]
    pub ended_at: Option<i64>,
    /// Cumulative absolutes from the hook's own state file, not increments —
    /// merged with `MAX`, so a retry or an out-of-order `async: true` `Stop`
    /// is idempotent instead of double-counted.
    #[serde(default)]
    pub turns: Option<i64>,
    #[serde(default)]
    pub chunk_index: Option<i64>,
    #[serde(default)]
    pub byte_offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    /// `true` drops sessions that have reported a `SessionEnd`.
    #[serde(default)]
    pub active: Option<bool>,
}

pub async fn upsert_session(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    ApiJson(body): ApiJson<UpsertSessionRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    check_len("cwd", body.cwd.as_deref(), MAX_PATH_BYTES)?;
    check_len(
        "transcript_path",
        body.transcript_path.as_deref(),
        MAX_PATH_BYTES,
    )?;
    check_len("source", body.source.as_deref(), MAX_REASON_BYTES)?;
    check_len("end_reason", body.end_reason.as_deref(), MAX_REASON_BYTES)?;

    let db = state.db.clone();
    let row = tokio::task::spawn_blocking(move || {
        store::upsert(
            &db,
            &bank_id,
            &SessionUpdate {
                session_id: &body.session_id,
                cwd: body.cwd.as_deref(),
                transcript_path: body.transcript_path.as_deref(),
                source: body.source.as_deref(),
                end_reason: body.end_reason.as_deref(),
                ended_at: body.ended_at,
                turns: body.turns,
                chunk_index: body.chunk_index,
                byte_offset: body.byte_offset,
                ..Default::default()
            },
        )
    })
    .await
    .map_err(join_err)??;
    Ok(Json(SessionResponse::from(row)))
}

/// Most recently seen first.
///
/// // ponytail: no `banks::get` pre-check, so an unknown bank answers `[]`
/// // rather than 404 — one saved round trip on a dashboard poll. Add the
/// // check if a caller ever needs to distinguish "no sessions" from "no
/// // bank"; `POST` already 404s via the foreign key.
pub async fn list_sessions(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<SessionResponse>>, ApiError> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let active_only = q.active.unwrap_or(false);
    let db = state.db.clone();
    let found = tokio::task::spawn_blocking(move || store::list(&db, &bank_id, limit, active_only))
        .await
        .map_err(join_err)??;
    Ok(Json(found.into_iter().map(SessionResponse::from).collect()))
}

pub async fn get_session(
    State(state): State<AppState>,
    Path((bank_id, session_id)): Path<(String, String)>,
) -> Result<Json<SessionResponse>, ApiError> {
    let db = state.db.clone();
    let lookup = session_id.clone();
    let found = tokio::task::spawn_blocking(move || store::get(&db, &bank_id, &lookup))
        .await
        .map_err(join_err)??;
    found
        .map(|s| Json(SessionResponse::from(s)))
        .ok_or_else(|| ApiError::not_found(format!("session not found: {session_id}")))
}

fn check_len(field: &str, value: Option<&str>, max: usize) -> Result<(), ApiError> {
    match value {
        Some(v) if v.len() > max => Err(memgarden_core::Error::Invalid(format!(
            "{field} too long (max {max} bytes)"
        ))
        .into()),
        _ => Ok(()),
    }
}
