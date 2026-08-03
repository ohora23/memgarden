//! HTTP timing middleware. Applied to every route except `/livez` and
//! `/metrics.json` (see routes::router) — self-measurement noise skews
//! the very numbers those two routes are meant to report accurately.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

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

/// DNS-rebinding guard: rejects any request whose `Host` header (host part,
/// port stripped) isn't `127.0.0.1` / `localhost` / `::1`. memgardend only
/// ever binds loopback, but without this a malicious page in a browser
/// could still send same-origin requests to it by name via DNS rebinding.
/// Applied to every route, including `/livez` and `/metrics.json`.
pub async fn check_host(req: Request, next: Next) -> Response {
    let host_ok = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| matches!(host_part(h), "127.0.0.1" | "localhost" | "::1"))
        .unwrap_or(false);

    if !host_ok {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }
    next.run(req).await
}

/// Stamps this daemon's identity token on every response.
///
/// This is what lets the hook tell `memgardend` apart from anything else that
/// managed to bind 127.0.0.1:9100 — see `token.rs` for why the secret travels
/// in this direction and only this direction. Applied to every route,
/// including the unmeasured ones, because `hooks status` (C5) reads `/healthz`
/// and has the same question.
///
/// `None` when `token::init` was never called, which is every in-process test:
/// no header, and the hook only enforces the check when it has a token of its
/// own to compare against.
pub async fn stamp_token(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    if let Some(token) = crate::token::current()
        && let Ok(value) = axum::http::HeaderValue::from_str(token)
    {
        response
            .headers_mut()
            .insert(crate::token::TOKEN_HEADER, value);
    }
    response
}

/// Strips a trailing `:port` from a `Host` header value. Handles the
/// bracketed IPv6 form (`[::1]:9100` -> `::1`) separately since IPv6
/// addresses contain colons themselves.
fn host_part(header: &str) -> &str {
    if let Some(rest) = header.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    header.split(':').next().unwrap_or(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_part_strips_port() {
        assert_eq!(host_part("127.0.0.1:9100"), "127.0.0.1");
        assert_eq!(host_part("localhost"), "localhost");
        assert_eq!(host_part("[::1]:9100"), "::1");
        assert_eq!(host_part("[::1]"), "::1");
        assert_eq!(host_part("evil.com"), "evil.com");
    }
}
