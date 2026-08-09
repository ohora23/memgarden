use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use memgarden_store::{nodes, search};

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
///
/// `// ponytail: rebuilds `vec_nodes` only. `vec_mental_models` (CE-10) is
/// equally derived — `mental_models.embedding` is its source of truth — so it
/// is equally repairable, but this route silently skips it and there is no
/// other repair path. ~15 lines to add when something actually corrupts a
/// vector index; until then the CE-10 write paths keep the two in one
/// transaction, which is why nothing has needed repairing.`
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

#[derive(Debug, Serialize)]
pub struct RelinkResponse {
    pub nodes: usize,
    pub links_written: usize,
}

/// `POST /v1/banks/{bank_id}/relink` — re-runs the semantic-link pass over
/// every already-embedded node in one bank.
///
/// Repairs a database built before CE-7, where the fact_type oracle was built
/// from the embedding batch alone and so every semantic edge joined two nodes
/// of one `embedding.batch_size` batch, capping out-degree at `batch_size - 1`.
/// The fix only reaches nodes embedded after it; this reaches the rest. It is
/// also the answer to the narrower shape noted in
/// `tests/graph_api.rs::a_semantic_link_reaches_a_node_embedded_in_an_earlier_batch`:
/// the pass only writes edges *out of* the nodes handed to it, so an early
/// node's out-edges are otherwise fixed at the moment it drained.
///
/// Purely additive and idempotent — `graph::insert_links` is
/// `ON CONFLICT DO NOTHING`, so nothing is deleted first and a second run
/// writes 0. Not `reindex`, which rebuilds `vec_nodes` and leaves `links`
/// untouched.
///
/// // ponytail: synchronous like `reindex`, and unlike it this one runs a
/// // k=100 KNN per node — ~3k nodes is seconds, but a bank large enough to
/// // outlive the client's timeout wants 202 + a job id. Chunk at a time, so
/// // an interrupted run just resumes on the next call.
pub async fn relink_bank(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
) -> Result<Json<RelinkResponse>, ApiError> {
    // Bounds the KNN fan-out held in memory at once; correctness no longer
    // depends on it (the fact_type oracle covers each node's neighbours too).
    const CHUNK: usize = 500;

    let mut after_id = 0i64;
    let mut resp = RelinkResponse {
        nodes: 0,
        links_written: 0,
    };
    loop {
        let db = state.db.clone();
        let bank = bank_id.clone();
        let batch =
            tokio::task::spawn_blocking(move || nodes::embedded_after(&db, &bank, after_id, CHUNK))
                .await
                .map_err(join_err)??;
        let Some((last, ..)) = batch.last() else {
            break;
        };
        after_id = *last;
        resp.nodes += batch.len();
        resp.links_written += crate::embed_task::on_batch_embedded(&state.db, batch).await;
    }
    Ok(Json(resp))
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
