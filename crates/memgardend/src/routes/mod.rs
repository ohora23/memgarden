mod banks;
mod health;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/livez", get(health::livez))
        .route("/healthz", get(health::healthz))
        .route("/v1/banks", get(banks::list_banks).post(banks::create_bank))
        .route(
            "/v1/banks/{bank_id}",
            get(banks::get_bank)
                .patch(banks::patch_bank)
                .delete(banks::delete_bank),
        )
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "code": "not_found", "message": "route not found" } })),
    )
}
