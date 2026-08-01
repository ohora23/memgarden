use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;

use crate::state::AppState;

/// Pure liveness ping: touches nothing, always 200.
pub async fn livez() -> &'static str {
    "ok"
}

/// Liveness + DB reachability: SELECT 1, schema version, row counts.
/// `DEGRADED` is reserved for CE-5+ (Ollama reachability) and not emitted
/// yet.
pub async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let uptime_ms = memgarden_core::now_ms() - state.started_at_ms;
    let db = state.db.clone();

    let checked =
        tokio::task::spawn_blocking(move || -> memgarden_core::error::Result<(i64, i64, i64)> {
            let conn = db.read()?;
            conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
            let schema_version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
            let banks: i64 = conn
                .query_row("SELECT COUNT(*) FROM banks", [], |r| r.get(0))
                .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
            let nodes: i64 = conn
                .query_row("SELECT COUNT(*) FROM memory_nodes", [], |r| r.get(0))
                .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
            Ok((schema_version, banks, nodes))
        })
        .await;

    let db_size_bytes = std::fs::metadata(&state.cfg.db_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let db_path = state.cfg.db_path.display().to_string();
    let version = env!("CARGO_PKG_VERSION");

    match checked {
        Ok(Ok((schema_version, banks, nodes))) => (
            StatusCode::OK,
            Json(json!({
                "status": "HEALTHY",
                "version": version,
                "schema_version": schema_version,
                "uptime_ms": uptime_ms,
                "db_path": db_path,
                "db_size_bytes": db_size_bytes,
                "banks": banks,
                "nodes": nodes,
            })),
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "UNHEALTHY",
                "version": version,
                "schema_version": null,
                "uptime_ms": uptime_ms,
                "db_path": db_path,
                "db_size_bytes": db_size_bytes,
                "banks": null,
                "nodes": null,
            })),
        ),
    }
}
