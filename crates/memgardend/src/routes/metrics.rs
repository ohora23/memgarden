use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use memgarden_core::metrics::METRICS;
use memgarden_store::metrics_store;
use memgarden_store::models::LedgerEntry;

use crate::error::{ApiError, join_err};
use crate::json::ApiJson;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct MetricsResponse {
    #[serde(flatten)]
    pub snapshot: memgarden_core::metrics::MetricsSnapshot,
    pub uptime_ms: i64,
    /// CE-11. `true` only when `[reranker] enabled` **and** the model is
    /// actually in the slot.
    ///
    /// This exists because `rerank::load_at_startup` swallows a load failure
    /// into a log line and leaves recall silently on the RRF passthrough —
    /// deliberately, since an absent ranking refinement is not a degraded
    /// memory system and `/healthz` DEGRADED should stay meaningful. But
    /// "configured on, silently running off" is exactly the state an operator
    /// must be able to see, and CE-11's own bench harness had to hand-roll a
    /// load precisely because nothing reported it. One boolean closes it.
    pub reranker_loaded: bool,
}

/// Not behind the timing middleware (see routes::router) — measuring this
/// route would be self-measurement noise in its own numbers.
pub async fn get_metrics(State(state): State<AppState>) -> Json<MetricsResponse> {
    let snapshot = METRICS.snapshot();
    let uptime_ms = memgarden_core::now_ms() - snapshot.started_at_ms;
    let reranker_loaded = state
        .reranker
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    Json(MetricsResponse {
        snapshot,
        uptime_ms,
        reranker_loaded,
    })
}

/// One `metric_snapshots` row. `payload` is re-emitted as a JSON value
/// rather than a string, because the column has `CHECK (json_valid(payload))`
/// and a browser that has to `JSON.parse` a field of a JSON response is being
/// handed the database's storage format instead of an API.
#[derive(Debug, Serialize)]
pub struct SnapshotResponse {
    pub id: i64,
    pub created_at: i64,
    pub payload: serde_json::Value,
}

/// `GET /v1/metrics/history?limit=` (E5, MX-2) — the counters as they were,
/// oldest last.
///
/// `metrics_task` has been writing these rows since MX-1 and nothing could
/// read them; this is the read side, and it is what the dashboard's trend
/// line is drawn from. It replaces the legacy `history.jsonl` that
/// `metrics_task`'s own doc comment names.
///
/// A row whose payload will not parse is **skipped, not fatal**: the column
/// check makes that near-impossible, but a history view that 500s because one
/// old row is malformed is worse than one that is one point short.
pub async fn list_history(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<SnapshotResponse>>, ApiError> {
    let db = state.db.clone();
    let rows = tokio::task::spawn_blocking(move || metrics_store::recent_snapshots(&db, q.limit))
        .await
        .map_err(join_err)??;
    Ok(Json(
        rows.into_iter()
            .filter_map(|(id, created_at, payload)| {
                serde_json::from_str(&payload)
                    .ok()
                    .map(|payload| SnapshotResponse {
                        id,
                        created_at,
                        payload,
                    })
            })
            .collect(),
    ))
}

/// What `POST /v1/ledger` writes into the `detail` column — the manual-case
/// shape, not a description of every row's (see
/// migrations/0001_init.sql: benefit_ledger.detail is free-form JSON, and
/// CE-5b's automatic rows carry a different set of keys entirely).
/// Write-only: the read path returns `detail` untouched.
#[derive(Debug, Serialize)]
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

/// `detail` is returned whole, as the object it is stored as.
///
/// It used to be flattened into the five fields `LedgerDetail` names, and
/// that silently deleted every automatic row's contents: a `retain_cap_saving`
/// row (CE-5b) records `{raw_tokens, capped_tokens, saved, ratio}`, none of
/// which is a field of the manual case shape, so the ledger API answered with
/// nulls where the measurement was. The whole point of AC-6 is that the
/// ledger collects itself; an endpoint that can only read what a human typed
/// into it defeats that.
///
/// So the reader is no longer allowed an opinion about which keys exist.
/// `kind` says how to read `detail`, which is the contract the table itself
/// has (`benefit_ledger.detail` is free-form JSON with three writers), and
/// `POST` keeps its typed request shape — being strict about what is written
/// and permissive about what is read is the right way round.
#[derive(Debug, Serialize)]
pub struct LedgerResponse {
    pub id: i64,
    pub kind: String,
    pub bank_id: Option<String>,
    pub detail: serde_json::Value,
    pub created_at: i64,
}

impl From<LedgerEntry> for LedgerResponse {
    fn from(e: LedgerEntry) -> Self {
        LedgerResponse {
            id: e.id,
            kind: e.kind,
            bank_id: e.bank_id,
            // A row whose detail is absent or unparseable becomes `{}`, not a
            // failed response: the column's CHECK makes the latter unlikely,
            // and one bad row must not hide the rest of the ledger.
            detail: e
                .detail
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Object(Default::default())),
            created_at: e.created_at,
        }
    }
}

/// Shared by `/v1/ledger` and `/v1/metrics/history`: both are newest-first
/// lists with one knob, and the store clamps the value to `1..=1000`.
#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    50
}

pub async fn list_ledger(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<LedgerResponse>>, ApiError> {
    let db = state.db.clone();
    let found = tokio::task::spawn_blocking(move || metrics_store::list_ledger(&db, q.limit))
        .await
        .map_err(join_err)??;
    Ok(Json(found.into_iter().map(LedgerResponse::from).collect()))
}

pub async fn create_ledger(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateLedgerRequest>,
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
