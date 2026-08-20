//! `POST /v1/banks/{bank_id}/recall` (CE-6).

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;

use memgarden_core::config::MAX_RECALL_TOKENS;
use memgarden_core::metrics::METRICS;
use memgarden_core::types::FactType;
use memgarden_store::banks;

use crate::error::{ApiError, join_err};
use crate::json::ApiJson;
use crate::recall::{self, RecallOutcome, RecallParams, TagsMatch};
use crate::state::AppState;

/// A query longer than this is a bug in the caller, not a prompt. The FTS
/// term cap (12) already bounds what reaches SQLite; this bounds what
/// reaches the embedder.
const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_TAGS: usize = 32;
/// The preamble is echoed verbatim into every injection. Uncapped, one
/// oversized request would keep re-serializing megabytes into a response the
/// caller pays for. 4KB is far more prose than a preamble needs.
const MAX_PREAMBLE_BYTES: usize = 4 * 1024;

#[derive(Debug, Deserialize)]
pub struct RecallRequest {
    pub query: String,
    /// Max results returned; also drives the per-arm over-fetch. Defaults to
    /// `[recall] limit`.
    #[serde(default)]
    pub limit: Option<usize>,
    /// `low | mid | high`. Defaults to `[profile] recall_budget`. Controls
    /// how many candidates are reranked, NOT how much text is injected —
    /// that is `maxTokens`.
    #[serde(default)]
    pub budget: Option<String>,
    /// Token ceiling on the injected text. Defaults to `[recall] max_tokens`
    /// (1024, fork parity). `1..=MAX_RECALL_TOKENS`.
    #[serde(default, rename = "maxTokens")]
    pub max_tokens: Option<usize>,
    /// Defaults to `[recall] types` — all three, unlike legacy's
    /// observation-only client default (see `RecallConfig::types`).
    #[serde(default, rename = "recallTypes")]
    pub recall_types: Option<Vec<FactType>>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, rename = "tagsMatch")]
    pub tags_match: TagsMatch,
    /// Overrides `[recall] preamble` for this request.
    #[serde(default)]
    pub preamble: Option<String>,
}

/// Hybrid recall: BM25 + vector, RRF-fused, combined-scored, cut to the
/// token budget, plus the ready-to-inject `<memgarden_memories>` block.
///
/// 404 for an unknown bank; 400 for an oversized query or an empty
/// `recallTypes`. A query under `MIN_QUERY_CHARS` is **not** an error — it
/// returns an empty result set, because the Phase C hook fires on every
/// prompt and a 400 there would be noise.
pub async fn recall_bank(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    ApiJson(body): ApiJson<RecallRequest>,
) -> Result<Json<RecallOutcome>, ApiError> {
    let started = Instant::now();
    METRICS.recall_requests.fetch_add(1, Ordering::Relaxed);
    let result = recall_inner(state, bank_id, body).await;
    METRICS
        .recall_latency
        .record_us(started.elapsed().as_micros() as u64);
    if result.is_err() {
        METRICS.recall_errors.fetch_add(1, Ordering::Relaxed);
    }
    result
}

async fn recall_inner(
    state: AppState,
    bank_id: String,
    body: RecallRequest,
) -> Result<Json<RecallOutcome>, ApiError> {
    if body.query.len() > MAX_QUERY_BYTES {
        return Err(memgarden_core::Error::Invalid(format!(
            "query too long: {} bytes (max {MAX_QUERY_BYTES})",
            body.query.len()
        ))
        .into());
    }
    if body.tags.len() > MAX_TAGS {
        return Err(memgarden_core::Error::Invalid(format!(
            "too many tags: {} (max {MAX_TAGS})",
            body.tags.len()
        ))
        .into());
    }
    let fact_types = match body.recall_types {
        Some(t) if t.is_empty() => {
            return Err(memgarden_core::Error::Invalid(
                "recallTypes must not be empty".to_string(),
            )
            .into());
        }
        Some(t) => t,
        None => state.cfg.recall.types.clone(),
    };
    if let Some(preamble) = &body.preamble
        && preamble.len() > MAX_PREAMBLE_BYTES
    {
        return Err(memgarden_core::Error::Invalid(format!(
            "preamble too long: {} bytes (max {MAX_PREAMBLE_BYTES})",
            preamble.len()
        ))
        .into());
    }
    let max_tokens = body.max_tokens.unwrap_or(state.cfg.recall.max_tokens);
    if !(1..=MAX_RECALL_TOKENS).contains(&max_tokens) {
        return Err(memgarden_core::Error::Invalid(format!(
            "maxTokens must be 1..={MAX_RECALL_TOKENS}: {max_tokens}"
        ))
        .into());
    }
    let budget = body
        .budget
        .unwrap_or_else(|| state.cfg.profile.recall_budget.clone());
    if !matches!(budget.as_str(), "low" | "mid" | "high") {
        return Err(memgarden_core::Error::Invalid(format!(
            "budget must be low|mid|high: {budget}"
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

    let params = RecallParams {
        query: body.query,
        limit: body.limit.unwrap_or(state.cfg.recall.limit).clamp(1, 200),
        budget,
        max_tokens,
        fact_types,
        tags: body.tags,
        tags_match: body.tags_match,
        cap_per_source: state.cfg.recall.cap_per_source,
        semantic_alpha: state.cfg.recall.semantic_alpha,
        preamble: body
            .preamble
            .unwrap_or_else(|| state.cfg.recall.preamble.clone()),
        now_ms: memgarden_core::now_ms(),
    };

    Ok(Json(recall::recall(&state, bank_id, params).await?))
}
