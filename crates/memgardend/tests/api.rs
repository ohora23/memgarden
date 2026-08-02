use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use memgardend::{routes, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

fn test_app() -> axum::Router {
    let db = memgarden_store::Db::open_memory().unwrap();
    let cfg = memgarden_core::config::Config {
        bind: "127.0.0.1:0".to_string(),
        db_path: std::path::PathBuf::from(":memory:"),
        log_level: "info".to_string(),
        metrics_snapshot_interval_secs: 60,
    };
    let state = AppState {
        db: Arc::new(db),
        cfg: Arc::new(cfg),
        started_at_ms: memgarden_core::now_ms(),
    };
    routes::router(state)
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
    assert_eq!(body["schema_version"], 1);
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
