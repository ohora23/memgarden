mod banks;
mod embed;
mod extract;
mod health;
mod metrics;

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
        .route(
            "/v1/banks/{bank_id}/dry-run-extract",
            post(extract::dry_run_extract),
        )
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
