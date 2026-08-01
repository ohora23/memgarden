use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use memgarden_core::metrics::METRICS;
use memgarden_store::metrics_store;
use memgarden_store::models::LedgerEntry;

use crate::error::{ApiError, join_err};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct MetricsResponse {
    #[serde(flatten)]
    pub snapshot: memgarden_core::metrics::MetricsSnapshot,
    pub uptime_ms: i64,
}

/// Not behind the timing middleware (see routes::router) — measuring this
/// route would be self-measurement noise in its own numbers.
pub async fn get_metrics() -> Json<MetricsResponse> {
    let snapshot = METRICS.snapshot();
    let uptime_ms = memgarden_core::now_ms() - snapshot.started_at_ms;
    Json(MetricsResponse {
        snapshot,
        uptime_ms,
    })
}

/// The `detail` column's JSON shape — a free-form manual-case record, not
/// its own table (see migrations/0001_init.sql: benefit_ledger.detail).
#[derive(Debug, Default, Deserialize, Serialize)]
struct LedgerDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    case_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    injection_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replaced_tokens_est: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateLedgerRequest {
    pub kind: String,
    pub case_text: String,
    #[serde(default)]
    pub injection_tokens: Option<i64>,
    #[serde(default)]
    pub replaced_tokens_est: Option<i64>,
    #[serde(default)]
    pub bank_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub evidence_ref: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LedgerResponse {
    pub id: i64,
    pub kind: String,
    pub bank_id: Option<String>,
    pub case_text: Option<String>,
    pub injection_tokens: Option<i64>,
    pub replaced_tokens_est: Option<i64>,
    pub session_id: Option<String>,
    pub evidence_ref: Option<String>,
    pub created_at: i64,
}

impl From<LedgerEntry> for LedgerResponse {
    fn from(e: LedgerEntry) -> Self {
        let detail: LedgerDetail = e
            .detail
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        LedgerResponse {
            id: e.id,
            kind: e.kind,
            bank_id: e.bank_id,
            case_text: detail.case_text,
            injection_tokens: detail.injection_tokens,
            replaced_tokens_est: detail.replaced_tokens_est,
            session_id: detail.session_id,
            evidence_ref: detail.evidence_ref,
            created_at: e.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListLedgerQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    50
}

pub async fn list_ledger(
    State(state): State<AppState>,
    Query(q): Query<ListLedgerQuery>,
) -> Result<Json<Vec<LedgerResponse>>, ApiError> {
    let db = state.db.clone();
    let found = tokio::task::spawn_blocking(move || metrics_store::list_ledger(&db, q.limit))
        .await
        .map_err(join_err)??;
    Ok(Json(found.into_iter().map(LedgerResponse::from).collect()))
}

pub async fn create_ledger(
    State(state): State<AppState>,
    Json(body): Json<CreateLedgerRequest>,
) -> Result<(StatusCode, Json<LedgerResponse>), ApiError> {
    let detail = LedgerDetail {
        case_text: Some(body.case_text),
        injection_tokens: body.injection_tokens,
        replaced_tokens_est: body.replaced_tokens_est,
        session_id: body.session_id,
        evidence_ref: body.evidence_ref,
    };
    let detail_json =
        serde_json::to_string(&detail).map_err(|e| ApiError::internal(e.to_string()))?;

    let db = state.db.clone();
    let kind = body.kind;
    let bank_id = body.bank_id;
    let created = tokio::task::spawn_blocking(move || {
        metrics_store::insert_ledger(&db, &kind, bank_id.as_deref(), Some(&detail_json))
    })
    .await
    .map_err(join_err)??;

    Ok((StatusCode::CREATED, Json(LedgerResponse::from(created))))
}
