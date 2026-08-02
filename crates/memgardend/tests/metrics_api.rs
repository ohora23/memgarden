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
    let cfg = memgarden_core::config::Config {
        bind: "127.0.0.1:0".to_string(),
        db_path: std::path::PathBuf::from(":memory:"),
        log_level: "info".to_string(),
        metrics_snapshot_interval_secs: 60,
        embedding: memgarden_core::config::EmbeddingConfig {
            enabled: false,
            model_dir: std::path::PathBuf::from("/tmp/memgarden-test-models"),
            intra_threads: 4,
            batch_size: 8,
            backlog_poll_secs: 5,
            debug_endpoint: false,
        },
    };
    let state = AppState {
        db: db.clone(),
        cfg: Arc::new(cfg),
        started_at_ms: memgarden_core::now_ms(),
        embedder: Arc::new(std::sync::RwLock::new(None)),
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
    metrics_task::tick(&db).unwrap();
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
    assert_eq!(body["injection_tokens"], 120);

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
        entries[0]["case_text"],
        "avoided a 400-token recall by reusing cached context"
    );
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
