//! Mental models and single-shot reflect end to end (CE-10, PR B9): the CRUD
//! surface, the three refresh outcomes, and citation filtering.
//!
//! The embedder is **absent** in every hermetic test here (`embedding.enabled
//! = false`), so recall runs on the BM25 arm and mental-model KNN returns
//! nothing. That is deliberate: the vector paths are unit-tested in
//! `memgarden-store::mental_models` against fabricated vectors (which need no
//! 133MB model), and the `#[ignore]`d `live_reflect` below exercises the whole
//! thing — real embedder, real Ollama — by hand.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use memgarden_core::types::FactType;
use memgarden_store::models::NewNode;
use memgarden_store::{Db, banks, nodes};
use memgardend::{routes, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    db: Arc<Db>,
    reply: Arc<std::sync::Mutex<String>>,
    calls: Arc<AtomicUsize>,
    /// Installed by `block_stub`: the stub waits on this before answering, so
    /// a test can hold one refresh in flight while it fires a second.
    gate: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    /// The last body posted to `/api/chat`. The token bounds are only real if
    /// they reach the wire, and a stub that discards the request cannot tell.
    last_request: Arc<std::sync::Mutex<Value>>,
}

async fn harness() -> Harness {
    let reply = Arc::new(std::sync::Mutex::new(
        r#"{"content":"regenerated"}"#.to_string(),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let gate: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let last_request = Arc::new(std::sync::Mutex::new(Value::Null));
    let (r, c, g, lr) = (
        reply.clone(),
        calls.clone(),
        gate.clone(),
        last_request.clone(),
    );
    let stub = axum::Router::new().route(
        "/api/chat",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let (r, c, g, lr) = (r.clone(), c.clone(), g.clone(), lr.clone());
            async move {
                *lr.lock().unwrap() = body;
                c.fetch_add(1, Ordering::SeqCst);
                if let Some(rx) = g.lock().await.take() {
                    let _ = rx.await;
                }
                let body = r.lock().unwrap().clone();
                axum::Json(json!({ "message": { "content": body } }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });

    let mut cfg = memgarden_core::config::Config::defaults().unwrap();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    cfg.embedding.enabled = false;
    cfg.ollama.base_url = format!("http://{addr}");
    cfg.ollama.request_timeout_secs = 5;
    cfg.ollama.max_retries = 0;

    let db = Arc::new(Db::open_memory().unwrap());
    banks::create(&db, "b1", None, None).unwrap();
    banks::create(&db, "b2", None, None).unwrap();
    let ollama = Arc::new(memgardend::ollama::OllamaClient::new(cfg.ollama.clone()).unwrap());
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
    std::mem::forget(retain_rx);
    let state = AppState {
        db: db.clone(),
        cfg: Arc::new(cfg),
        started_at_ms: memgarden_core::now_ms(),
        embedder: Arc::new(std::sync::RwLock::new(None)),
        ollama,
        consolidating: Default::default(),
        refreshing: Default::default(),
        retain_tx,
    };
    Harness {
        app: routes::router(state),
        db,
        reply,
        calls,
        gate,
        last_request,
    }
}

impl Harness {
    fn set_reply(&self, body: Value) {
        *self.reply.lock().unwrap() = body.to_string();
    }

    fn llm_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The body of the most recent `/api/chat` request.
    fn last_request(&self) -> Value {
        self.last_request.lock().unwrap().clone()
    }

    /// Makes the next stub call block until the returned sender fires.
    async fn block_stub(&self) -> tokio::sync::oneshot::Sender<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self.gate.lock().await = Some(rx);
        tx
    }

    /// A fact whose **event** time is `occurred_start` and whose **write**
    /// time is forced to `created_at` — the two axes B9-1 confused. `insert`
    /// stamps `created_at` from the clock, so the test rewrites it directly.
    fn fact_with_times(&self, text: &str, occurred_start: i64, created_at: i64) -> String {
        let mut node = NewNode::new("b1", FactType::World, text);
        node.occurred_start = Some(occurred_start);
        node.mentioned_at = Some(occurred_start);
        let id = nodes::insert(&self.db, node).unwrap();
        self.db
            .write(|tx| {
                tx.execute(
                    "UPDATE memory_nodes SET created_at = ?1 WHERE id = ?2",
                    rusqlite::params![created_at, id],
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
        nodes::get(&self.db, id).unwrap().unwrap().uuid
    }

    /// A fact that BM25 will find for the queries used below.
    fn fact(&self, bank: &str, text: &str, mentioned_at: i64) -> String {
        let mut node = NewNode::new(bank, FactType::World, text);
        node.mentioned_at = Some(mentioned_at);
        let id = nodes::insert(&self.db, node).unwrap();
        nodes::get(&self.db, id).unwrap().unwrap().uuid
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        // Every route sits behind the loopback Host guard (`check_host`).
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:9100");
        let request = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn create(&self, bank: &str, body: Value) -> Value {
        let (status, created) = self
            .send(
                "POST",
                &format!("/v1/banks/{bank}/mental-models"),
                Some(body),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        created
    }
}

/// Create, read, list, patch, delete — plus the composite `(bank_id, id)` key:
/// a model is only reachable through the bank that owns it.
#[tokio::test]
async fn crud_round_trip_is_scoped_to_the_owning_bank() {
    let h = harness().await;

    let created = h
        .create(
            "b1",
            json!({
                "name": "Ollama latency",
                "sourceQuery": "ollama latency measurements",
                "trigger": "0 3 * * *",
            }),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("mm-"), "{id}");
    assert_eq!(created["bank_id"], "b1");
    // No content given -> the legacy pending sentinel, not an empty string.
    assert_eq!(created["content"], "Generating content...");
    assert_eq!(created["max_tokens"], 2048, "legacy's COALESCE default");
    assert_eq!(created["last_refreshed_at"], Value::Null);
    assert_eq!(
        created["due"], true,
        "a model with a trigger and no refresh is due"
    );

    // GET through the owning bank.
    let (status, got) = h
        .send("GET", &format!("/v1/banks/b1/mental-models/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["name"], "Ollama latency");

    // The same id through another bank is a 404, and so is a delete there.
    let (status, _) = h
        .send("GET", &format!("/v1/banks/b2/mental-models/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h
        .send("DELETE", &format!("/v1/banks/b2/mental-models/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // b2's list does not see b1's model.
    let (_, list) = h.send("GET", "/v1/banks/b2/mental-models", None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    let (_, list) = h.send("GET", "/v1/banks/b1/mental-models", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // PATCH sets only what it is given.
    let (status, patched) = h
        .send(
            "PATCH",
            &format!("/v1/banks/b1/mental-models/{id}"),
            Some(json!({"content": "p50 is 20ms"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(patched["content"], "p50 is 20ms");
    assert_eq!(patched["name"], "Ollama latency", "untouched by the patch");
    assert_eq!(patched["trigger"], "0 3 * * *");

    // DELETE, then 404.
    let (status, _) = h
        .send("DELETE", &format!("/v1/banks/b1/mental-models/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h
        .send("GET", &format!("/v1/banks/b1/mental-models/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert_eq!(h.llm_calls(), 0, "CRUD must never call the LLM");
}

#[tokio::test]
async fn write_routes_reject_bad_input() {
    let h = harness().await;

    for body in [
        json!({"name": "  "}),
        json!({"name": "n", "trigger": "@daily"}),
        json!({"name": "n", "trigger": "0 3 * * MON"}),
        json!({"name": "n", "maxTokens": 0}),
        json!({"name": "n", "maxTokens": 64000}),
        // Review round 1, MUST FIX 1: unbounded, this expanded to a ~24M-element
        // Vec per parse — re-paid for every row of every read.
        json!({"name": "n", "trigger": "0-59,".repeat(100) + "0 * * * *"}),
        // Review round 1, L5: the sentinel `refresh` refuses to write.
        json!({"name": "n", "content": "   "}),
    ] {
        let (status, _) = h
            .send("POST", "/v1/banks/b1/mental-models", Some(body.clone()))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "should reject {body}");
    }

    // Unknown bank is a 404, not a 400.
    let (status, _) = h
        .send(
            "POST",
            "/v1/banks/nope/mental-models",
            Some(json!({"name": "n"})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// `memory_engine.py:11646-11660` — with no supporting facts the refresh skips
/// the LLM **entirely** and only bumps the watermark.
#[tokio::test]
async fn refresh_with_zero_supporting_facts_makes_no_llm_call() {
    let h = harness().await;
    // A fact that the source query cannot match, so recall comes back empty.
    h.fact("b1", "unrelated gardening notes", 1_000);

    let created = h
        .create(
            "b1",
            json!({
                "name": "Ollama latency",
                "sourceQuery": "ollama latency measurements",
                "content": "the existing conclusion",
            }),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, refreshed) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    assert_eq!(h.llm_calls(), 0, "no facts must mean no LLM call at all");
    assert_eq!(
        refreshed["content"], "the existing conclusion",
        "content is preserved byte for byte"
    );
    assert!(
        refreshed["last_refreshed_at"].as_i64().unwrap() > 0,
        "the watermark still advances"
    );
    assert_eq!(
        refreshed["reflect_response"]["refresh_skipped"], "no_new_facts",
        "the skip is auditable"
    );
}

/// The happy path, and the `since` filter: only facts newer than the last
/// refresh count as supporting.
#[tokio::test]
async fn refresh_regenerates_content_from_new_facts_only() {
    let h = harness().await;
    h.fact(
        "b1",
        "ollama latency measured at 830ms before the fix",
        1_000,
    );

    let created = h
        .create(
            "b1",
            json!({"name": "Ollama latency", "sourceQuery": "ollama latency measured"}),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_string();

    h.set_reply(json!({"content": "p50 is 20ms after the CPU-force fix"}));
    let (status, refreshed) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    assert_eq!(h.llm_calls(), 1);
    assert_eq!(refreshed["content"], "p50 is 20ms after the CPU-force fix");
    assert_eq!(refreshed["reflect_response"]["supporting_facts"], 1);
    let watermark = refreshed["last_refreshed_at"].as_i64().unwrap();

    // A second refresh sees the same (now old) fact as not-new, so it skips
    // the LLM again rather than re-summarising unchanged input.
    let (status, again) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.llm_calls(), 1, "still one call in total");
    assert_eq!(again["reflect_response"]["refresh_skipped"], "no_new_facts");
    assert!(again["last_refreshed_at"].as_i64().unwrap() >= watermark);
}

/// `memory_engine.py:11724-11743` — an empty render preserves the previous
/// content **and** raises, so the caller learns the refresh did not happen.
#[tokio::test]
async fn refresh_producing_empty_content_preserves_the_old_content_and_errors() {
    let h = harness().await;
    h.fact(
        "b1",
        "ollama latency measured at 830ms before the fix",
        1_000,
    );

    let created = h
        .create(
            "b1",
            json!({
                "name": "Ollama latency",
                "sourceQuery": "ollama latency measured",
                "content": "the working document",
            }),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_string();

    // Whitespace only: legacy strips before deciding, and so does this.
    h.set_reply(json!({"content": "   \n  "}));
    let (status, err) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{err}");
    assert_eq!(h.llm_calls(), 1);

    let (_, after) = h
        .send("GET", &format!("/v1/banks/b1/mental-models/{id}"), None)
        .await;
    assert_eq!(
        after["content"], "the working document",
        "the previous content must survive an empty render"
    );
    assert_eq!(
        after["last_refreshed_at"],
        Value::Null,
        "and the watermark must NOT advance on a failed refresh"
    );
    assert_eq!(
        after["reflect_response"]["refresh_skipped"], "empty_candidate",
        "the failure is auditable"
    );
}

/// `agent.py:1312-1314` — ids the model invented are dropped, ids it actually
/// saw survive.
#[tokio::test]
async fn reflect_filters_hallucinated_citation_ids() {
    let h = harness().await;
    let real = h.fact(
        "b1",
        "ollama latency measured at 830ms before the fix",
        1_000,
    );

    h.set_reply(json!({
        "answer": "Latency was 830ms before the fix.",
        "memory_ids": [real, "uuid-the-model-made-up", real],
        "mental_model_ids": ["mm-does-not-exist"],
    }));
    let (status, out) = h
        .send(
            "POST",
            "/v1/banks/b1/reflect",
            Some(json!({"query": "ollama latency measured"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(h.llm_calls(), 1);
    assert_eq!(out["answer"], "Latency was 830ms before the fix.");
    assert_eq!(
        out["citations"]["memory_ids"],
        json!([real]),
        "the fabricated id is dropped and the real one is not duplicated"
    );
    assert_eq!(
        out["citations"]["mental_model_ids"],
        json!([]),
        "a mental model that was never retrieved cannot be cited"
    );
    assert_eq!(out["counts"]["memories"], 1);
}

/// Nothing retrieved means nothing to reason over — and no GPU spent.
#[tokio::test]
async fn reflect_with_no_retrieval_makes_no_llm_call() {
    let h = harness().await;
    h.fact("b1", "unrelated gardening notes", 1_000);

    let (status, out) = h
        .send(
            "POST",
            "/v1/banks/b1/reflect",
            Some(json!({"query": "ollama latency measurements"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(h.llm_calls(), 0);
    assert_eq!(out["answer"], "");
    assert_eq!(out["counts"]["memories"], 0);
    assert_eq!(out["citations"]["memory_ids"], json!([]));
}

/// KNN search over `vec_mental_models` needs the embedder; asking for it
/// without one is a 503, never a silently different ordering.
#[tokio::test]
async fn knn_search_without_an_embedder_is_unavailable() {
    let h = harness().await;
    h.create("b1", json!({"name": "Ollama latency"})).await;

    let (status, _) = h
        .send("GET", "/v1/banks/b1/mental-models?q=latency", None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // Without `q` the same route is the plain recency page.
    let (status, list) = h.send("GET", "/v1/banks/b1/mental-models", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
}

/// KNN mode is unpaged, so `offset` alongside `q` is refused rather than
/// silently ignored (review round 1, L3) — checked before the embedder, so it
/// is a 400 even on a daemon that could not have served the search anyway.
#[tokio::test]
async fn knn_search_rejects_an_offset_instead_of_ignoring_it() {
    let h = harness().await;
    h.create("b1", json!({"name": "Ollama latency"})).await;

    let (status, err) = h
        .send(
            "GET",
            "/v1/banks/b1/mental-models?q=latency&offset=50",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unpaged"),
        "the client must be told why: {err}"
    );

    // `offset=0` is not a paging request, so it is not an error...
    let (status, _) = h
        .send("GET", "/v1/banks/b1/mental-models?q=latency&offset=0", None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "no embedder here");
    // ...and offset without q pages normally.
    let (status, _) = h
        .send("GET", "/v1/banks/b1/mental-models?offset=50", None)
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// Cron due-ness as the API reports it (`maintenance.py:417-425`).
#[tokio::test]
async fn due_reflects_the_trigger_against_the_last_refresh() {
    let h = harness().await;

    // No trigger -> never due, however stale.
    let created = h.create("b1", json!({"name": "No schedule"})).await;
    assert_eq!(created["due"], false);

    // A trigger and no refresh -> due.
    let created = h
        .create(
            "b1",
            json!({"name": "Scheduled", "sourceQuery": "nothing matches this", "trigger": "*/5 * * * *"}),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["due"], true);

    // Refreshing (the zero-facts path, so no LLM call) moves the watermark to
    // now, which is at or after the most recent 5-minute fire.
    let (status, refreshed) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.llm_calls(), 0);
    assert_eq!(refreshed["due"], false, "just refreshed, so not due");
}

/// The whole path against the real embedder and the real Ollama. Run by hand:
///   cargo test -p memgardend --test mental_api -- --ignored live_reflect
#[tokio::test]
#[ignore]
async fn live_reflect() {
    let mut cfg = memgarden_core::config::Config::defaults().unwrap();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    cfg.embedding.enabled = true;

    let db = Arc::new(Db::open_memory().unwrap());
    banks::create(&db, "b1", None, None).unwrap();
    let embedder = memgardend::embed::Embedder::load(&cfg.embedding).unwrap();
    let ollama = Arc::new(memgardend::ollama::OllamaClient::new(cfg.ollama.clone()).unwrap());
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
    std::mem::forget(retain_rx);
    let state = AppState {
        db: db.clone(),
        cfg: Arc::new(cfg),
        started_at_ms: memgarden_core::now_ms(),
        embedder: Arc::new(std::sync::RwLock::new(Some(Arc::new(embedder)))),
        ollama,
        consolidating: Default::default(),
        refreshing: Default::default(),
        retain_tx,
    };
    let app = routes::router(state.clone());

    for text in [
        "ollama latency dropped from 830ms to 20ms after forcing the embedder onto the CPU",
        "the reranker was disabled in the live daemon as part of the same latency fix",
        "a banana is a good source of potassium",
    ] {
        let mut node = NewNode::new("b1", FactType::World, text);
        node.mentioned_at = Some(memgarden_core::now_ms());
        let id = nodes::insert(&db, node).unwrap();
        let vector = state
            .embedder
            .read()
            .unwrap()
            .clone()
            .unwrap()
            .embed_batch(&[text.to_string()])
            .unwrap();
        nodes::set_embedding(&db, id, "b1", &vector[0]).unwrap();
    }

    let started = std::time::Instant::now();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/banks/b1/reflect")
                .header("host", "127.0.0.1:9100")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"query": "why did ollama latency improve?"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let out: Value = serde_json::from_slice(&bytes).unwrap();
    println!("reflect end-to-end: {elapsed:?}\n{out:#}");
    assert!(!out["answer"].as_str().unwrap().is_empty());
}

/// **Review round 1, B9-1 (blocking).** The refresh window compares write
/// times, not event times.
///
/// `occurred_start` is an event date the extractor reads out of the text, so a
/// fact retained *today* about a 2024 event carries a 2024 timestamp. When the
/// window compared that against a wall-clock watermark, such a fact was older
/// than the watermark the moment it landed and was excluded from every future
/// refresh — silently, as a 200 with `no_new_facts`.
#[tokio::test]
async fn a_fact_about_an_old_event_retained_after_the_last_refresh_is_still_seen() {
    let h = harness().await;
    h.fact(
        "b1",
        "ollama latency measured at 830ms before the fix",
        1_000,
    );

    let created = h
        .create(
            "b1",
            json!({"name": "Ollama latency", "sourceQuery": "ollama latency measured"}),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_string();

    h.set_reply(json!({"content": "first summary"}));
    let (status, first) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(h.llm_calls(), 1);
    // The watermark is a WRITE time taken from the data, not `now`.
    let watermark = first["refresh_watermark"].as_i64().unwrap();
    assert!(
        watermark <= first["last_refreshed_at"].as_i64().unwrap(),
        "the data watermark cannot be ahead of the wall clock: {first}"
    );

    // A fact written AFTER that refresh, about an event in 2023. Its event
    // timestamps are years older than the watermark; its write time is newer.
    const EVENT_2023: i64 = 1_700_000_000_000;
    h.fact_with_times(
        "ollama latency measured again, at 20ms after the CPU-force fix",
        EVENT_2023,
        watermark + 1_000,
    );

    h.set_reply(json!({"content": "second summary"}));
    let (status, second) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(
        h.llm_calls(),
        2,
        "the newly written fact must reach the LLM, whatever its event date"
    );
    assert_eq!(second["content"], "second summary");
    assert_eq!(second["reflect_response"]["supporting_facts"], 1);
    assert_eq!(second["refresh_watermark"], watermark + 1_000);
}

/// One refresh per model at a time (review round 1, MUST FIX 2). Without the
/// claim, both callers read the same watermark and the loser's summary is
/// overwritten while its watermark advance stands.
#[tokio::test]
async fn a_second_concurrent_refresh_of_one_model_is_a_conflict() {
    let h = harness().await;
    h.fact(
        "b1",
        "ollama latency measured at 830ms before the fix",
        1_000,
    );
    let created = h
        .create(
            "b1",
            json!({"name": "Ollama latency", "sourceQuery": "ollama latency measured"}),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_string();

    // Hold the first refresh inside the LLM call.
    let release = h.block_stub().await;
    let (app, uri) = (
        h.app.clone(),
        format!("/v1/banks/b1/mental-models/{id}/refresh"),
    );
    let first = tokio::spawn(async move {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("host", "127.0.0.1:9100")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap()
    });
    // Let it get as far as the (blocked) stub.
    while h.llm_calls() == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let (status, err) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");
    assert_eq!(h.llm_calls(), 1, "the second caller never reached Ollama");

    let _ = release.send(());
    assert_eq!(first.await.unwrap().status(), StatusCode::OK);

    // The slot is released, so a later refresh is allowed again (this one
    // takes the no-new-facts path, so it costs no LLM call).
    let (status, _) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// The bounds are only real if they reach the wire (review round 1, MEDIUM 2).
/// Replacing `reply_cap(&model)` with a literal, or dropping the explicit
/// `num_ctx`, must fail a test — not sail through because the stub throws the
/// request body away.
#[tokio::test]
async fn the_reply_bounds_reach_the_ollama_request() {
    let h = harness().await;
    h.fact(
        "b1",
        "ollama latency measured at 830ms before the fix",
        1_000,
    );

    // A model whose own budget is under the const ceiling: the smaller of the
    // two must win, which also proves the value is not hard-coded.
    let created = h
        .create(
            "b1",
            json!({
                "name": "Ollama latency",
                "sourceQuery": "ollama latency measured",
                "maxTokens": 256,
            }),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_string();
    h.set_reply(json!({"content": "summary"}));
    let (status, _) = h
        .send(
            "POST",
            &format!("/v1/banks/b1/mental-models/{id}/refresh"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let body = h.last_request();
    assert_eq!(body["options"]["num_predict"], 256, "refresh: {body}");
    assert_eq!(body["options"]["num_ctx"], 8192, "refresh: {body}");
    assert_eq!(body["format"]["properties"]["content"]["maxLength"], 8192);

    // Reflect: a fixed const ceiling and its own window.
    h.set_reply(json!({"answer": "yes", "memory_ids": [], "mental_model_ids": []}));
    let (status, _) = h
        .send(
            "POST",
            "/v1/banks/b1/reflect",
            Some(json!({"query": "ollama latency measured"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let body = h.last_request();
    assert_eq!(body["options"]["num_predict"], 1024, "reflect: {body}");
    assert_eq!(body["options"]["num_ctx"], 8192, "reflect: {body}");
    assert_eq!(body["format"]["properties"]["answer"]["maxLength"], 4096);
}
