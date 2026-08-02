use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use memgarden_store::banks;

use crate::error::{ApiError, join_err};
use crate::extract::{self, parse::ParsedFact};
use crate::ollama::OllamaError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct DryRunExtractRequest {
    pub text: String,
    /// Unix milliseconds. Legacy accepts a full datetime; this endpoint's
    /// contract (task spec) takes the already-resolved epoch millis the
    /// caller computed, matching `memgarden_core::now_ms()`'s unit.
    #[serde(default)]
    pub event_date: Option<i64>,
    #[serde(default)]
    pub mission: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DryRunExtractResponse {
    pub facts: Vec<ParsedFact>,
}

/// `POST /v1/banks/{bank_id}/dry-run-extract` — runs extraction against
/// Ollama and returns the parsed facts. No writes (legacy has the same
/// debug route, `api/http.py:4092`). 404 for an unknown bank; 503 when
/// Ollama is unreachable or the request timed out waiting for a permit
/// (Critic Revision R11).
pub async fn dry_run_extract(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    Json(body): Json<DryRunExtractRequest>,
) -> Result<Json<DryRunExtractResponse>, ApiError> {
    let db = state.db.clone();
    let id = bank_id.clone();
    let found = tokio::task::spawn_blocking(move || banks::get(&db, &id))
        .await
        .map_err(join_err)??;
    if found.is_none() {
        return Err(ApiError::not_found(format!("bank not found: {bank_id}")));
    }

    let facts = extract::extract(
        &state.ollama,
        &body.text,
        body.event_date,
        body.mission.as_deref(),
    )
    .await
    .map_err(|e| {
        let message = e.to_string();
        match &e {
            // Busy/Transport/Http: Ollama itself is the problem — 503, so
            // the client knows to retry rather than treat this as a bad
            // request.
            OllamaError::Busy | OllamaError::Transport(_) | OllamaError::Http { .. } => {
                ApiError::unavailable(message)
            }
            // Parse: Ollama answered but never produced valid JSON across
            // all retries — a real extraction failure, not an availability
            // problem.
            OllamaError::Parse(_) => ApiError::internal(message),
        }
    })?;

    Ok(Json(DryRunExtractResponse { facts }))
}
