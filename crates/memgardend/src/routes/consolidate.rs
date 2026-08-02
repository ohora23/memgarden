//! `POST /v1/banks/{bank_id}/consolidate` and
//! `GET /v1/banks/{bank_id}/consolidation` (CE-9b).

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use memgarden_store::{banks, consolidate as store};

use crate::consolidate::round::{self, RoundSummary};
use crate::error::{ApiError, join_err};
use crate::json::ApiJson;
use crate::state::AppState;

/// Deliberately empty, and deliberately **required**.
///
/// Security review LOW 5 (CSRF shape): a POST with no body and no content type
/// is a CORS *simple* request, so any page the user visits can fire
/// `fetch(url, {method: "POST", mode: "no-cors"})` at the daemon. The Host
/// guard does not help — the browser sends `Host: 127.0.0.1:PORT` itself. The
/// attacker cannot read the response, but the round still mutates memory and
/// burns GPU. Requiring `Content-Type: application/json` makes it a
/// *preflighted* request, and the preflight is what the Host guard can refuse.
///
/// Callers send `{}`. `/v1/banks/{id}/reindex` has the same shape and is not
/// fixed here — it is a pre-existing route and a separate change.
#[derive(Debug, Deserialize, Default)]
pub struct ConsolidateRequest {}

/// The run ledger row, as the status endpoint reports it.
#[derive(Debug, Serialize)]
pub struct RunResponse {
    pub id: i64,
    pub status: String,
    pub facts_seen: i64,
    pub created: i64,
    pub updated: i64,
    pub deleted: i64,
    pub merged: i64,
    pub watermark: Option<i64>,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

impl From<store::RunRow> for RunResponse {
    fn from(r: store::RunRow) -> Self {
        RunResponse {
            id: r.id,
            status: r.status,
            facts_seen: r.facts_seen,
            created: r.created_n,
            updated: r.updated_n,
            deleted: r.deleted_n,
            merged: r.merged_n,
            watermark: r.watermark,
            error: r.error,
            started_at: r.started_at,
            finished_at: r.finished_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// Highest fact id any run has committed.
    pub watermark: i64,
    /// Facts above the watermark waiting for the next round.
    pub pending: i64,
    /// `0` when the background task is disabled.
    pub interval_secs: u64,
    pub latest_run: Option<RunResponse>,
}

/// Runs one consolidation round **synchronously** and answers with what it
/// did.
///
/// Deliberately not a 202-plus-job like `/retain`: this is the manual and
/// test surface for a path whose normal trigger is a 300 s background tick,
/// and a caller who asked for a round wants to know what the round produced.
/// One round is bounded by `consolidation.batch_size` facts, so the wall
/// clock is bounded too — but it is minutes, not milliseconds, and callers
/// need a matching client timeout.
///
/// 409 if a round is already running on the bank (the background tick counts).
/// Takes a JSON body — see [`ConsolidateRequest`].
pub async fn consolidate_bank(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    ApiJson(_): ApiJson<ConsolidateRequest>,
) -> Result<Json<RoundSummary>, ApiError> {
    require_bank(&state, &bank_id).await?;
    Ok(Json(round::run_round(&state, &bank_id).await?))
}

pub async fn get_consolidation(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    require_bank(&state, &bank_id).await?;
    let db = state.db.clone();
    let bank = bank_id.clone();
    let (watermark, pending, latest_run) = tokio::task::spawn_blocking(move || {
        let watermark = store::watermark(&db, &bank)?;
        let pending = store::count_unconsolidated(&db, &bank, watermark)?;
        let latest = store::latest_run(&db, &bank)?;
        Ok::<_, memgarden_core::Error>((watermark, pending, latest))
    })
    .await
    .map_err(join_err)??;

    Ok(Json(StatusResponse {
        watermark,
        pending,
        interval_secs: state.cfg.consolidation.interval_secs,
        latest_run: latest_run.map(RunResponse::from),
    }))
}

async fn require_bank(state: &AppState, bank_id: &str) -> Result<(), ApiError> {
    let db = state.db.clone();
    let bank = bank_id.to_string();
    let found = tokio::task::spawn_blocking(move || banks::get(&db, &bank))
        .await
        .map_err(join_err)??;
    if found.is_none() {
        return Err(ApiError::not_found(format!("bank not found: {bank_id}")));
    }
    Ok(())
}
