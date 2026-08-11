//! `POST/GET /v1/banks/{bank_id}/sessions` and the session GC (HK-1a, PR C1).
//!
//! The retain-side half of the mirror is exercised in `retain_api.rs`, where
//! the background worker lives.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use memgarden_store::Db;
use memgardend::{metrics_task, routes, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

fn build() -> (axum::Router, Arc<Db>) {
    let db = Arc::new(Db::open_memory().unwrap());
    let mut cfg = memgarden_core::config::Config::defaults().unwrap();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    cfg.embedding.enabled = false;
    cfg.ollama.base_url = "http://127.0.0.1:1".to_string();
    cfg.ollama.request_timeout_secs = 1;
    cfg.ollama.max_retries = 0;

    let ollama = Arc::new(memgardend::ollama::OllamaClient::new(cfg.ollama.clone()).unwrap());
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
    // No worker in these tests; keeping the receiver alive is what makes the
    // retain endpoint's `try_reserve` succeed if a test ever calls it.
    std::mem::forget(retain_rx);
    let state = AppState {
        db: db.clone(),
        cfg: Arc::new(cfg),
        started_at_ms: memgarden_core::now_ms(),
        embedder: Arc::new(std::sync::RwLock::new(None)),
        reranker: Default::default(),
        ollama,
        consolidating: Default::default(),
        refreshing: Default::default(),
        retain_tx,
        events: memgardend::events::channel(),
    };
    (routes::router(state), db)
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "127.0.0.1:9100")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", "127.0.0.1:9100")
        .body(Body::empty())
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn upsert_then_read_back() {
    let (app, db) = build();
    memgarden_store::banks::create(&db, "b1", None, None).unwrap();

    let created = body_json(
        app.clone()
            .oneshot(post(
                "/v1/banks/b1/sessions",
                json!({
                    "session_id": "s1",
                    "cwd": "/repo",
                    "transcript_path": "/t.jsonl",
                    "source": "startup",
                }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(created["session_id"], "s1");
    assert_eq!(created["source"], "startup");
    assert_eq!(created["byte_offset"], 0);
    assert_eq!(created["inflight_bytes"], 0);
    assert!(created["ended_at"].is_null());

    let fetched = app
        .clone()
        .oneshot(get("/v1/banks/b1/sessions/s1"))
        .await
        .unwrap();
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(body_json(fetched).await, created);

    let missing = app
        .oneshot(get("/v1/banks/b1/sessions/nope"))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upsert_into_an_unknown_bank_is_404() {
    let (app, _db) = build();
    let response = app
        .oneshot(post(
            "/v1/banks/nope/sessions",
            json!({ "session_id": "s1" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `confirmed_offset` is a claim about ingestion, and only the daemon may
/// make it. A hook that could set it would be able to mark unwritten bytes
/// durable and lose them silently, which is the exact failure the two-cursor
/// split exists to prevent.
#[tokio::test]
async fn a_client_cannot_set_the_durable_cursor() {
    let (app, db) = build();
    memgarden_store::banks::create(&db, "b1", None, None).unwrap();

    let row = body_json(
        app.oneshot(post(
            "/v1/banks/b1/sessions",
            json!({
                "session_id": "s1",
                "byte_offset": 5000,
                // Not part of the request contract. Must be ignored, not honoured.
                "confirmed_offset": 5000,
                "retains": 99,
                "messages_sent": 99,
            }),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(row["byte_offset"], 5000);
    assert_eq!(row["confirmed_offset"], 0);
    assert_eq!(row["inflight_bytes"], 5000);
    assert_eq!(row["retains"], 0);
    assert_eq!(row["messages_sent"], 0);

    // And it is really absent from the row, not merely from the response.
    let stored = memgarden_store::sessions::get(&db, "b1", "s1")
        .unwrap()
        .unwrap();
    assert_eq!(stored.confirmed_offset, 0);
}

#[tokio::test]
async fn list_honours_limit_and_active() {
    let (app, db) = build();
    memgarden_store::banks::create(&db, "b1", None, None).unwrap();
    for sid in ["s1", "s2", "s3"] {
        app.clone()
            .oneshot(post("/v1/banks/b1/sessions", json!({ "session_id": sid })))
            .await
            .unwrap();
    }
    app.clone()
        .oneshot(post(
            "/v1/banks/b1/sessions",
            json!({ "session_id": "s2", "end_reason": "logout", "ended_at": 42 }),
        ))
        .await
        .unwrap();

    let all = body_json(
        app.clone()
            .oneshot(get("/v1/banks/b1/sessions"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(all.as_array().unwrap().len(), 3);

    let active = body_json(
        app.clone()
            .oneshot(get("/v1/banks/b1/sessions?active=true"))
            .await
            .unwrap(),
    )
    .await;
    let active = active.as_array().unwrap();
    assert_eq!(active.len(), 2);
    assert!(active.iter().all(|s| s["session_id"] != "s2"));

    // The limit that reaches the query, not the constant it came from: ask
    // for one row and count what arrives.
    let one = body_json(
        app.clone()
            .oneshot(get("/v1/banks/b1/sessions?limit=1"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(one.as_array().unwrap().len(), 1);

    // Over the ceiling clamps instead of 400ing, and a `0` clamps up to 1.
    let huge = body_json(
        app.clone()
            .oneshot(get("/v1/banks/b1/sessions?limit=99999"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(huge.as_array().unwrap().len(), 3);
    let zero = body_json(
        app.oneshot(get("/v1/banks/b1/sessions?limit=0"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(zero.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn oversized_strings_are_refused() {
    let (app, db) = build();
    memgarden_store::banks::create(&db, "b1", None, None).unwrap();
    for (field, len) in [("cwd", 4097), ("transcript_path", 4097), ("source", 65)] {
        let response = app
            .clone()
            .oneshot(post(
                "/v1/banks/b1/sessions",
                json!({ "session_id": "s1", field: "x".repeat(len) }),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{field} must be bounded"
        );
    }
    // A 201-byte session id is refused by the store's own bound.
    let response = app
        .oneshot(post(
            "/v1/banks/b1/sessions",
            json!({ "session_id": "x".repeat(201) }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// The GC runs on the metrics tick. Asserted through `tick`, not through
/// `sessions::gc`, so the test fails if the wiring is removed — and it pins
/// the retention window that actually *reaches* the delete rather than the
/// constant it is derived from.
#[tokio::test]
async fn the_metrics_tick_expires_stale_sessions() {
    let (_app, db) = build();
    memgarden_store::banks::create(&db, "b1", None, None).unwrap();
    for sid in ["stale", "fresh"] {
        memgarden_store::sessions::upsert(
            &db,
            "b1",
            &memgarden_store::sessions::SessionUpdate {
                session_id: sid,
                ..Default::default()
            },
        )
        .unwrap();
    }
    let day_ms = 24 * 60 * 60 * 1000;
    let now = memgarden_core::now_ms();
    // A non-default window on purpose (C2a): the fixture ages are derived
    // from the value *passed to* `tick`, so a `tick` that ignores its
    // parameter and reaches for a hardcoded 90 collects neither row.
    let retention: i64 = 7;
    db.write(|tx| {
        for (sid, age_days) in [("stale", retention + 1), ("fresh", retention - 1)] {
            tx.execute(
                "UPDATE sessions SET last_seen_at = ?1 WHERE session_id = ?2",
                rusqlite::params![now - age_days * day_ms, sid],
            )
            .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
        }
        Ok(())
    })
    .unwrap();

    metrics_task::tick(&db, retention as u64).unwrap();

    assert!(
        memgarden_store::sessions::get(&db, "b1", "stale")
            .unwrap()
            .is_none(),
        "a session last seen {} days ago must be collected",
        retention + 1
    );
    assert!(
        memgarden_store::sessions::get(&db, "b1", "fresh")
            .unwrap()
            .is_some(),
        "a session inside the window must survive"
    );
}
