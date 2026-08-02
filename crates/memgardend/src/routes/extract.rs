use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use memgarden_store::banks;

use crate::error::{ApiError, join_err};
use crate::extract::{self, parse::ParsedFact};
use crate::json::ApiJson;
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

/// Security review M1: one oversized `text` would hold the single Ollama
/// permit for the whole prompt-eval. ~10x legacy's 3000-char chunk; B3's
/// retain ingest chunks at legacy size and never comes near this.
const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_MISSION_BYTES: usize = 4 * 1024;

/// `POST /v1/banks/{bank_id}/dry-run-extract` — runs extraction against
/// Ollama and returns the parsed facts. No writes (legacy has the same
/// debug route, `api/http.py:4092`). 404 for an unknown bank; 400 for
/// oversized input; 503 for transient trouble (permit timeout — Critic
/// Revision R11 — transport, 5xx, deadline); 502 for a permanent upstream
/// refusal (4xx like model-not-found) or unparseable output across retries.
pub async fn dry_run_extract(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    ApiJson(body): ApiJson<DryRunExtractRequest>,
) -> Result<Json<DryRunExtractResponse>, ApiError> {
    if body.text.len() > MAX_TEXT_BYTES {
        return Err(memgarden_core::Error::Invalid(format!(
            "text too long: {} bytes (max {MAX_TEXT_BYTES})",
            body.text.len()
        ))
        .into());
    }
    if body
        .mission
        .as_deref()
        .is_some_and(|m| m.len() > MAX_MISSION_BYTES)
    {
        return Err(memgarden_core::Error::Invalid(format!(
            "mission too long (max {MAX_MISSION_BYTES} bytes)"
        ))
        .into());
    }

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
        false,
    )
    .await
    .map_err(|e| {
        let message = e.to_string();
        match &e {
            // Busy/Transport/5xx: Ollama is temporarily the problem — 503,
            // so the client knows to retry.
            OllamaError::Busy | OllamaError::Deadline(_) | OllamaError::Transport(_) => {
                ApiError::unavailable(message)
            }
            OllamaError::Http { status, .. } if *status >= 500 => ApiError::unavailable(message),
            // Permanent upstream refusal (e.g. 404 model-not-found; not
            // retried, see ollama.rs) or garbage across all retries — 502:
            // the upstream misbehaved, retrying blindly won't fix it.
            OllamaError::Http { .. } | OllamaError::Parse(_) => ApiError::bad_gateway(message),
        }
    })?;

    Ok(Json(DryRunExtractResponse { facts }))
}
