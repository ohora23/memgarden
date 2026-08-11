//! `GET /ui/*` — the explorer's static assets (E1).
//!
//! # Why the daemon serves these at all
//!
//! PRD R2 called for a separate web-UI server with its own lifecycle. This
//! serves them from `memgardend` instead, and `docs/design/e1-memory-explorer.md`
//! §Decisions carries the argument: a second process reintroduces the process
//! sprawl this rebuild exists to remove, while the independence R2 wanted —
//! change the UI without restarting the daemon — is not actually bought by a
//! second process, because these are static files.
//!
//! Serving them from the same origin also means [`crate::middleware::check_host`]
//! passes on the `Host` header a browser sends anyway, so the UI needs no
//! token and there is no CORS configuration to get wrong.
//!
//! # Why compiled in rather than read from disk
//!
//! `include_str!` rather than a directory served at runtime:
//!
//! * **No path handling, so no path traversal.** The router matches three
//!   exact paths and everything else is a 404. A directory server has to be
//!   *correct* about `..`, symlinks and percent-encoding; this has nothing to
//!   be correct about.
//! * **No install step.** `cargo install memgardend` yields a working UI. A
//!   runtime directory needs a path in the config and files copied to it, and
//!   a fresh install would otherwise serve 404s until someone did that.
//! * **No new dependency.** `tower-http`'s `fs` feature would work and pulls
//!   in a file-serving stack for three files.
//!
//! The cost is a rebuild to see a UI edit, which for one crate is seconds.
//! E2's ego-graph needed no renderer to vendor — it is SVG in `app.js` — so
//! this is still three files. Whenever a vendored one does arrive for E3 it is
//! one more `include_str!`, and it must be vendored rather than fetched from a
//! CDN, because a local-first memory system that needs the network to draw its
//! own graph is a contradiction.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};

const INDEX_HTML: &str = include_str!("../../ui/index.html");
const APP_JS: &str = include_str!("../../ui/app.js");
const STYLE_CSS: &str = include_str!("../../ui/style.css");

/// Vendored, not fetched — see `ui/vendor/README.md` for versions, licenses
/// and why these two are committed as built files. They are the only assets
/// here that are worth caching hard, but they are served on the same
/// `no-cache` terms as the rest: the binary is what versions them, so a copy
/// that outlives an upgrade is the failure worth avoiding, and revalidation
/// over loopback is free.
const SIGMA_JS: &str = include_str!("../../ui/vendor/sigma-3.0.3.min.js");
const GRAPHOLOGY_JS: &str = include_str!("../../ui/vendor/graphology-0.26.0.umd.min.js");
/// The layout, which sigma does not carry: d3-force computes coordinates and
/// nothing else, and its three dependencies all attach to the same `d3`
/// global, so they load as four ordinary script tags. See `vendor/README.md`
/// for why this rather than `graphology-library`.
const D3_DISPATCH_JS: &str = include_str!("../../ui/vendor/d3-dispatch-3.0.1.min.js");
const D3_QUADTREE_JS: &str = include_str!("../../ui/vendor/d3-quadtree-3.0.1.min.js");
const D3_TIMER_JS: &str = include_str!("../../ui/vendor/d3-timer-3.0.1.min.js");
const D3_FORCE_JS: &str = include_str!("../../ui/vendor/d3-force-3.0.0.min.js");

/// `no-cache` rather than a long max-age: the assets are versioned by the
/// binary, so a cached copy outliving an upgrade is exactly the failure mode
/// worth avoiding, and they are served off loopback where revalidation is free.
const NO_CACHE: &str = "no-cache";

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, NO_CACHE),
        ],
        body,
    )
        .into_response()
}

pub async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

pub async fn app_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

pub async fn style_css() -> Response {
    asset("text/css; charset=utf-8", STYLE_CSS)
}

pub async fn sigma_js() -> Response {
    asset("text/javascript; charset=utf-8", SIGMA_JS)
}

pub async fn graphology_js() -> Response {
    asset("text/javascript; charset=utf-8", GRAPHOLOGY_JS)
}

pub async fn d3_dispatch_js() -> Response {
    asset("text/javascript; charset=utf-8", D3_DISPATCH_JS)
}

pub async fn d3_quadtree_js() -> Response {
    asset("text/javascript; charset=utf-8", D3_QUADTREE_JS)
}

pub async fn d3_timer_js() -> Response {
    asset("text/javascript; charset=utf-8", D3_TIMER_JS)
}

pub async fn d3_force_js() -> Response {
    asset("text/javascript; charset=utf-8", D3_FORCE_JS)
}

/// `/ui` → `/ui/`, so a relative asset reference resolves the same either way.
pub async fn index_redirect() -> Redirect {
    Redirect::permanent("/ui/")
}

/// Anything else under `/ui/`. A 404 here is a missing asset, not a missing
/// memory, so it does not go through `ApiError`'s JSON shape.
pub async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "no such asset").into_response()
}
