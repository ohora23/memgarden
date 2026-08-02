use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use memgarden_store::search;

use crate::error::{ApiError, join_err};
use crate::json::ApiJson;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct ReindexResponse {
    pub rebuilt: usize,
}

/// `POST /v1/banks/{bank_id}/reindex` — rebuilds `vec_nodes` for one bank
/// from `memory_nodes.embedding` (the CE-2 deferral). 200, not 202 (NIT 17):
/// the rebuild commits per 500-row chunk and this handler awaits the whole
/// thing before responding.
pub async fn reindex_bank(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
) -> Result<Json<ReindexResponse>, ApiError> {
    let db = state.db.clone();
    let rebuilt =
        tokio::task::spawn_blocking(move || search::rebuild_vec_index(&db, Some(&bank_id)))
            .await
            .map_err(join_err)??;
    Ok(Json(ReindexResponse { rebuilt }))
}

#[derive(Debug, Deserialize)]
pub struct EmbedDebugRequest {
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct EmbedDebugResponse {
    pub embedding: Vec<f32>,
    pub dim: usize,
}

/// `POST /v1/embed` — debug-only text -> vector endpoint, gated behind
/// `embedding.debug_endpoint` (default off). Disabled means 404, not 403, so
/// its existence isn't probeable from the outside.
pub async fn embed_debug(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<EmbedDebugRequest>,
) -> Result<Json<EmbedDebugResponse>, ApiError> {
    if !state.cfg.embedding.debug_endpoint {
        return Err(ApiError::not_found("route not found"));
    }
    let embedder = state
        .embedder
        .read()
        .expect("embedder lock poisoned")
        .clone()
        .ok_or_else(|| ApiError::unavailable("embedding model not ready"))?;

    let vectors = tokio::task::spawn_blocking(move || embedder.embed_batch(&[body.text]))
        .await
        .map_err(join_err)?
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let embedding = vectors.into_iter().next().unwrap_or_default();
    Ok(Json(EmbedDebugResponse {
        dim: embedding.len(),
        embedding,
    }))
}
