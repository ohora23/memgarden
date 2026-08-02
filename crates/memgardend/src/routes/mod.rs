mod banks;
mod consolidate;
mod embed;
mod extract;
mod graph;
mod health;
mod mental;
mod metrics;
mod recall;
mod retain;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::middleware::from_fn;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::middleware::{check_host, track_http};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    // /livez and /metrics.json skip the timing middleware: they'd otherwise
    // be measuring (and slightly skewing) their own numbers.
    let unmeasured = Router::new()
        .route("/livez", get(health::livez))
        .route("/metrics.json", get(metrics::get_metrics));

    let measured = Router::new()
        .route("/healthz", get(health::healthz))
        .route("/v1/banks", get(banks::list_banks).post(banks::create_bank))
        .route(
            "/v1/banks/{bank_id}",
            get(banks::get_bank)
                .patch(banks::patch_bank)
                .delete(banks::delete_bank),
        )
        .route(
            "/v1/ledger",
            get(metrics::list_ledger).post(metrics::create_ledger),
        )
        .route("/v1/banks/{bank_id}/reindex", post(embed::reindex_bank))
        .route("/v1/embed", post(embed::embed_debug))
        // Unlike /v1/embed this debug route is deliberately ungated: it is
        // B2's only end-to-end verification surface until B3 wires retain,
        // and AC-1's quality A/B runs against it. Input size is capped in
        // the handler; the single Ollama permit bounds concurrency.
        .route(
            "/v1/banks/{bank_id}/dry-run-extract",
            post(extract::dry_run_extract),
        )
        // The retain route (and only it) raises axum's 2MB default body
        // limit: the Phase C hook posts a raw transcript, and the caps that
        // shrink it run server-side, after parsing. See
        // retain::MAX_RETAIN_BODY_BYTES for why the ceiling is where it is.
        .route(
            "/v1/banks/{bank_id}/retain",
            post(retain::retain).layer(DefaultBodyLimit::max(retain::MAX_RETAIN_BODY_BYTES)),
        )
        .route("/v1/banks/{bank_id}/recall", post(recall::recall_bank))
        // Synchronous: a manual round is bounded by `consolidation.batch_size`
        // facts but still runs LLM calls, so callers need a matching timeout.
        .route(
            "/v1/banks/{bank_id}/consolidate",
            post(consolidate::consolidate_bank),
        )
        .route(
            "/v1/banks/{bank_id}/consolidation",
            get(consolidate::get_consolidation),
        )
        .route("/v1/banks/{bank_id}/graph", get(graph::get_graph))
        .route(
            "/v1/banks/{bank_id}/mental-models",
            get(mental::list_mental_models).post(mental::create_mental_model),
        )
        .route(
            "/v1/banks/{bank_id}/mental-models/{mm_id}",
            get(mental::get_mental_model)
                .patch(mental::patch_mental_model)
                .delete(mental::delete_mental_model),
        )
        // Synchronous, like /consolidate: one LLM call, so callers need a
        // matching client timeout.
        .route(
            "/v1/banks/{bank_id}/mental-models/{mm_id}/refresh",
            post(mental::refresh_mental_model),
        )
        .route("/v1/banks/{bank_id}/reflect", post(mental::reflect_bank))
        .route("/v1/retain/{job_id}", get(retain::get_job))
        .layer(from_fn(track_http));

    unmeasured
        .merge(measured)
        .fallback(not_found)
        // Applies to every route, including the unmeasured ones.
        .layer(from_fn(check_host))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "code": "not_found", "message": "route not found" } })),
    )
}
