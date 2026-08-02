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

/// Liveness + DB reachability: SELECT 1, schema version, row counts, plus
/// the embedding subsystem's `loading`/`ready`/`disabled`/`error` status
/// (CE-4 — the first real use of `DEGRADED`, reserved since CE-3).
pub async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let uptime_ms = memgarden_core::now_ms() - state.started_at_ms;
    let db = state.db.clone();
    let cfg = state.cfg.clone();

    // db_size_bytes is computed in the same spawn_blocking closure as the
    // DB queries (not on the async runtime thread) since std::fs::metadata
    // is itself a blocking syscall; it's independent of query success so
    // it's captured unconditionally, alongside the query result.
    let checked = tokio::task::spawn_blocking(
        move || -> (memgarden_core::error::Result<(i64, i64, i64)>, u64) {
            let db_size_bytes = std::fs::metadata(&cfg.db_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let result = (|| -> memgarden_core::error::Result<(i64, i64, i64)> {
                let conn = db.read()?;
                conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                    .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
                let schema_version: i64 =
                    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
                        .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
                let banks: i64 = conn
                    .query_row("SELECT COUNT(*) FROM banks", [], |r| r.get(0))
                    .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
                let nodes: i64 = conn
                    .query_row("SELECT COUNT(*) FROM memory_nodes", [], |r| r.get(0))
                    .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
                Ok((schema_version, banks, nodes))
            })();
            (result, db_size_bytes)
        },
    )
    .await;

    let db_path = state.cfg.db_path.display().to_string();
    let version = env!("CARGO_PKG_VERSION");
    let embedding = crate::embed::embed_status();

    match checked {
        Ok((Ok((schema_version, banks, nodes)), db_size_bytes)) => {
            // DB is healthy; an embedding load error still counts as
            // service-degraded, not down — 200, not 503.
            let status = if embedding == crate::embed::EmbedStatus::Error {
                "DEGRADED"
            } else {
                "HEALTHY"
            };
            (
                StatusCode::OK,
                Json(json!({
                    "status": status,
                    "version": version,
                    "schema_version": schema_version,
                    "uptime_ms": uptime_ms,
                    "db_path": db_path,
                    "db_size_bytes": db_size_bytes,
                    "banks": banks,
                    "nodes": nodes,
                    "embedding": embedding.as_str(),
                })),
            )
        }
        Ok((Err(_), db_size_bytes)) => (
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
                "embedding": embedding.as_str(),
            })),
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "UNHEALTHY",
                "version": version,
                "schema_version": null,
                "uptime_ms": uptime_ms,
                "db_path": db_path,
                "db_size_bytes": 0,
                "banks": null,
                "nodes": null,
                "embedding": embedding.as_str(),
            })),
        ),
    }
}
