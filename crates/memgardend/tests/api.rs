use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use memgardend::{routes, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

fn test_app() -> axum::Router {
    test_app_with_db().0
}

fn test_app_with_db() -> (axum::Router, Arc<memgarden_store::Db>) {
    let (app, db, rx) = test_app_parts(|_| {});
    // No retain worker runs in router-only tests; keeping the receiver alive
    // is what makes the endpoint's `try_reserve` succeed.
    std::mem::forget(rx);
    (app, db)
}

/// Full fixture: the router, the DB, and the retain queue's receiver so a
/// test can assert on what was enqueued (or drop it / fill it to exercise the
/// closed and full paths).
fn test_app_parts(
    tweak: impl FnOnce(&mut memgarden_core::config::Config),
) -> (
    axum::Router,
    Arc<memgarden_store::Db>,
    tokio::sync::mpsc::Receiver<memgardend::retain::RetainTask>,
) {
    let db = Arc::new(memgarden_store::Db::open_memory().unwrap());
    let mut cfg = memgarden_core::config::Config::defaults().unwrap();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    cfg.embedding.enabled = false;
    // Unroutable loopback port: any real Ollama call fails fast.
    cfg.ollama.base_url = "http://127.0.0.1:1".to_string();
    cfg.ollama.request_timeout_secs = 1;
    cfg.ollama.max_retries = 0;
    tweak(&mut cfg);

    let ollama = Arc::new(memgardend::ollama::OllamaClient::new(cfg.ollama.clone()).unwrap());
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
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
    };
    (routes::router(state), db, retain_rx)
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
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

fn get_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", "127.0.0.1:9100")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn livez_ok() {
    let app = test_app();
    let response = app.oneshot(get_request("/livez")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"ok");
}

#[tokio::test]
async fn healthz_reports_healthy() {
    let app = test_app();
    let response = app.oneshot(get_request("/healthz")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["status"], "HEALTHY");
    assert_eq!(body["schema_version"], memgarden_store::LATEST_VERSION);
}

#[tokio::test]
async fn banks_crud_roundtrip() {
    let app = test_app();

    let create = json_request(
        "POST",
        "/v1/banks",
        json!({ "bank_id": "roundtrip", "mission": "test bank" }),
    );
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["bank_id"], "roundtrip");
    assert_eq!(body["mission"], "test bank");

    let response = app
        .clone()
        .oneshot(get_request("/v1/banks/roundtrip"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["bank_id"], "roundtrip");

    let patch = json_request(
        "PATCH",
        "/v1/banks/roundtrip",
        json!({ "mission": "updated mission" }),
    );
    let response = app.clone().oneshot(patch).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["mission"], "updated mission");
    assert_eq!(body["disposition"], Value::Null);

    let delete = Request::builder()
        .method("DELETE")
        .uri("/v1/banks/roundtrip")
        .header("host", "127.0.0.1:9100")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(delete).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = app
        .clone()
        .oneshot(get_request("/v1/banks/roundtrip"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_still_409() {
    let app = test_app();

    let create = json_request("POST", "/v1/banks", json!({ "bank_id": "dup" }));
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let create_again = json_request("POST", "/v1/banks", json!({ "bank_id": "dup" }));
    let response = app.oneshot(create_again).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test]
async fn check_violation_returns_400() {
    let app = test_app();

    // disposition must be valid JSON (CHECK (disposition IS NULL OR
    // json_valid(disposition))) — a plain non-JSON string violates it and
    // must surface as 400, not 500.
    let create = json_request(
        "POST",
        "/v1/banks",
        json!({ "bank_id": "bad-disposition", "disposition": "not json" }),
    );
    let response = app.oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "invalid");
}

#[tokio::test]
async fn empty_bank_id_400() {
    let app = test_app();
    let create = json_request("POST", "/v1/banks", json!({ "bank_id": "" }));
    let response = app.oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_invalid_json_400() {
    let app = test_app();

    let create = json_request("POST", "/v1/banks", json!({ "bank_id": "patch-target" }));
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let patch = json_request(
        "PATCH",
        "/v1/banks/patch-target",
        json!({ "disposition": "not json" }),
    );
    let response = app.oneshot(patch).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "invalid");
}

#[tokio::test]
async fn patch_partial_preserves_other_fields() {
    let app = test_app();

    let create = json_request(
        "POST",
        "/v1/banks",
        json!({ "bank_id": "partial", "mission": "original mission", "disposition": "{\"x\":1}" }),
    );
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Only `mission` is present in the body; `disposition` must be left
    // untouched, not nulled out.
    let patch = json_request(
        "PATCH",
        "/v1/banks/partial",
        json!({ "mission": "updated mission" }),
    );
    let response = app.oneshot(patch).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["mission"], "updated mission");
    assert_eq!(body["disposition"], "{\"x\":1}");
}

#[tokio::test]
async fn delete_twice_second_404() {
    let app = test_app();

    let create = json_request("POST", "/v1/banks", json!({ "bank_id": "del-twice" }));
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let delete = Request::builder()
        .method("DELETE")
        .uri("/v1/banks/del-twice")
        .header("host", "127.0.0.1:9100")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(delete).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let delete_again = Request::builder()
        .method("DELETE")
        .uri("/v1/banks/del-twice")
        .header("host", "127.0.0.1:9100")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(delete_again).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn host_header_rejected() {
    let app = test_app();

    let evil = Request::builder()
        .method("GET")
        .uri("/livez")
        .header("host", "evil.com")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(evil).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let allowed = Request::builder()
        .method("GET")
        .uri("/livez")
        .header("host", "127.0.0.1:9100")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(allowed).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_route_404_json() {
    let app = test_app();
    let response = app.oneshot(get_request("/nope")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn healthz_reports_embedding_status() {
    let app = test_app();
    let response = app.oneshot(get_request("/healthz")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    // The global embed-status static defaults to "loading" and nothing in
    // these router-only tests advances it (embed_task never spawned).
    assert_eq!(body["embedding"], "loading");
}

#[tokio::test]
async fn embed_debug_disabled_by_default() {
    let app = test_app();
    let request = json_request("POST", "/v1/embed", json!({ "text": "hello" }));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reindex_bank_rebuilds_vec_index() {
    use memgarden_core::EMBEDDING_DIM;
    use memgarden_core::types::FactType;
    use memgarden_store::models::NewNode;
    use memgarden_store::{nodes, search};

    let (app, db) = test_app_with_db();
    memgarden_store::banks::create(&db, "b1", None, None).unwrap();
    let id = nodes::insert(&db, NewNode::new("b1", FactType::World, "n")).unwrap();
    let v = vec![0.5f32; EMBEDDING_DIM];
    nodes::set_embedding(&db, id, "b1", &v).unwrap();
    db.write(|tx| {
        tx.execute("DELETE FROM vec_nodes", []).unwrap();
        Ok(())
    })
    .unwrap();
    assert!(search::knn(&db, "b1", &v, 10).unwrap().is_empty());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/banks/b1/reindex")
                .header("host", "127.0.0.1:9100")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["rebuilt"], 1);
    assert_eq!(search::knn(&db, "b1", &v, 10).unwrap()[0].0, id);
}

#[tokio::test]
async fn dry_run_extract_unknown_bank_404() {
    let app = test_app();
    let request = json_request(
        "POST",
        "/v1/banks/nope/dry-run-extract",
        json!({ "text": "hello" }),
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dry_run_extract_unreachable_ollama_503() {
    // test_app_with_db()'s OllamaConfig points at an unroutable loopback
    // port with max_retries=0, so this fails fast without ever touching a
    // real Ollama instance — exercising Critic Revision R11's 503 mapping,
    // not the extraction logic itself (that's parse.rs's job).
    let (app, db) = test_app_with_db();
    memgarden_store::banks::create(&db, "b1", None, None).unwrap();

    let request = json_request(
        "POST",
        "/v1/banks/b1/dry-run-extract",
        json!({ "text": "the user prefers dark mode" }),
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "unavailable");
}
