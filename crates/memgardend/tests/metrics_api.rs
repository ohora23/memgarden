use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use memgarden_store::Db;
use memgardend::{metrics_task, routes, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

fn test_app() -> (axum::Router, Arc<Db>) {
    let db = Arc::new(Db::open_memory().unwrap());
    let mut cfg = memgarden_core::config::Config::defaults().unwrap();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    cfg.embedding.enabled = false;
    // Unroutable loopback port: any real Ollama call fails fast.
    cfg.ollama.base_url = "http://127.0.0.1:1".to_string();
    cfg.ollama.request_timeout_secs = 1;
    cfg.ollama.max_retries = 0;

    let ollama = Arc::new(memgardend::ollama::OllamaClient::new(cfg.ollama.clone()).unwrap());
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
    // No worker is spawned in router-only tests; keeping the receiver alive
    // is what makes the endpoint's `try_reserve` succeed.
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

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", "127.0.0.1:9100")
        .body(Body::empty())
        .unwrap()
}

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:9100")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn metrics_endpoint_reports_tracked_requests() {
    let (app, _db) = test_app();

    // A tracked request (goes through the timing middleware).
    let response = app.clone().oneshot(get_request("/healthz")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app.oneshot(get_request("/metrics.json")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    // >=1, not ==1: METRICS is a process-global static shared across every
    // test in this binary running in parallel.
    assert!(body["http_requests"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn snapshot_task_writes_row() {
    let (_app, db) = test_app();
    let count_rows = || {
        let conn = db.read().unwrap();
        conn.query_row("SELECT count(*) FROM metric_snapshots", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };

    let before = count_rows();
    metrics_task::tick(&db, 90).unwrap();
    assert_eq!(count_rows(), before + 1);
}

#[tokio::test]
async fn ledger_roundtrip() {
    let (app, _db) = test_app();

    let create = json_request(
        "POST",
        "/v1/ledger",
        json!({
            "kind": "manual",
            "case_text": "avoided a 400-token recall by reusing cached context",
            "injection_tokens": 120,
            "replaced_tokens_est": 400
        }),
    );
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["kind"], "manual");
    assert_eq!(body["detail"]["injection_tokens"], 120);

    let response = app
        .clone()
        .oneshot(get_request("/v1/ledger?limit=50"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let entries = body.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0]["detail"]["case_text"],
        "avoided a 400-token recall by reusing cached context"
    );
}

/// The read path used to flatten `detail` into the manual-case shape, which
/// deleted every automatic row's contents on the way out — a
/// `retain_cap_saving` row records none of those keys. This is the
/// regression test for that: whatever a writer put in `detail`, a reader
/// gets back.
#[tokio::test]
async fn ledger_returns_an_automatic_rows_detail_intact() {
    let (app, db) = test_app();

    memgarden_store::metrics_store::insert_ledger(
        &db,
        "retain_cap_saving",
        None,
        Some(r#"{"raw_tokens":39555,"capped_tokens":19423,"saved":20132,"ratio":0.5089}"#),
    )
    .unwrap();

    let response = app
        .oneshot(get_request("/v1/ledger?limit=1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let detail = &body[0]["detail"];
    assert_eq!(detail["raw_tokens"], 39555);
    assert_eq!(detail["capped_tokens"], 19423);
    assert_eq!(detail["saved"], 20132);
}

#[tokio::test]
async fn ledger_limit_clamped() {
    let (app, _db) = test_app();

    for i in 0..3 {
        let create = json_request(
            "POST",
            "/v1/ledger",
            json!({ "kind": "manual", "case_text": format!("case {i}") }),
        );
        let response = app.clone().oneshot(create).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    // limit=-1 must clamp to 1, not be treated as unbounded or rejected.
    let response = app
        .clone()
        .oneshot(get_request("/v1/ledger?limit=-1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn ledger_invalid_kind_is_400() {
    let (app, _db) = test_app();
    let create = json_request(
        "POST",
        "/v1/ledger",
        json!({ "kind": "not_a_real_kind", "case_text": "x" }),
    );
    let response = app.oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "invalid");
}

#[tokio::test]
async fn history_returns_parsed_payloads_newest_first() {
    let (app, db) = test_app();

    metrics_task::tick(&db, 90).unwrap();
    metrics_task::tick(&db, 90).unwrap();

    let response = app
        .clone()
        .oneshot(get_request("/v1/metrics/history?limit=50"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let rows = body.as_array().unwrap();
    assert!(rows.len() >= 2);
    assert!(
        rows[0]["id"].as_i64() > rows[1]["id"].as_i64(),
        "newest first"
    );
    // An object, not the stored string: the browser must not have to parse
    // a field of a JSON response.
    assert!(
        rows[0]["payload"]["http_requests"].is_number(),
        "payload is re-emitted as JSON, got {}",
        rows[0]["payload"]
    );
}

#[tokio::test]
async fn stats_lists_every_bank_including_an_empty_one() {
    let (app, _db) = test_app();

    let create = json_request("POST", "/v1/banks", json!({ "bank_id": "stats-bank" }));
    assert_eq!(
        app.clone().oneshot(create).await.unwrap().status(),
        StatusCode::CREATED
    );

    let response = app.oneshot(get_request("/v1/stats")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let bank = body
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["bank_id"] == "stats-bank")
        .expect("a bank with nothing in it still appears");
    assert_eq!(bank["nodes"], 0);
    assert_eq!(bank["links"], 0);
    assert_eq!(bank["documents"], 0);
}
