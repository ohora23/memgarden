//! HTTP timing middleware. Applied to every route except `/livez` and
//! `/metrics.json` (see routes::router) — self-measurement noise skews
//! the very numbers those two routes are meant to report accurately.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

use memgarden_core::metrics::METRICS;

pub async fn track_http(req: Request, next: Next) -> Response {
    let start = Instant::now();
    METRICS.http_requests.fetch_add(1, Ordering::Relaxed);

    let response = next.run(req).await;

    let elapsed_us = start.elapsed().as_micros() as u64;
    METRICS.http_latency.record_us(elapsed_us);
    if response.status().is_server_error() {
        METRICS.http_errors.fetch_add(1, Ordering::Relaxed);
    }
    response
}
