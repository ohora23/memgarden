//! End-to-end retain ingest (CE-5b, PR B3): the HTTP contract, the ledger
//! row, and the background worker writing nodes.
//!
//! Extraction is driven by a **stub Ollama** — a tiny axum app bound to a
//! loopback port that the daemon's real `OllamaClient` talks to over real
//! HTTP. That keeps the production code free of a trait/mock abstraction
//! introduced solely for tests, while still exercising the whole path:
//! chunking, retries, per-chunk failure handling (Critic Revision R14), node
//! writes and the `+10ms` ordering offsets.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use memgarden_core::config::Config;
use memgarden_store::Db;
use memgardend::{retain, routes, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Stub Ollama
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StubState {
    calls: Arc<AtomicUsize>,
    /// 1-based call ordinals that should answer HTTP 500 instead of facts.
    fail_on: Arc<Vec<usize>>,
    /// 1-based call ordinals that extract cleanly and find nothing.
    empty_on: Arc<Vec<usize>>,
}

/// Spawns a stub `/api/chat` on a free loopback port; returns its base URL and
/// the call counter.
async fn spawn_stub_ollama(fail_on: Vec<usize>) -> (String, Arc<AtomicUsize>) {
    spawn_stub_ollama_with(fail_on, vec![]).await
}

async fn spawn_stub_ollama_with(
    fail_on: Vec<usize>,
    empty_on: Vec<usize>,
) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let state = StubState {
        calls: calls.clone(),
        fail_on: Arc::new(fail_on),
        empty_on: Arc::new(empty_on),
    };
    let app = axum::Router::new()
        .route("/api/generate", axum::routing::post(stub_chat))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), calls)
}

async fn stub_chat(
    axum::extract::State(state): axum::extract::State<StubState>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let n = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    if state.fail_on.contains(&n) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "stub failure").into_response();
    }
    if state.empty_on.contains(&n) {
        return axum::Json(json!({ "response": json!({ "facts": [] }).to_string() }))
            .into_response();
    }
    // The ledger call reuses this stub and wants a different shape. It is
    // told apart by its system prompt rather than by call order, because
    // order is exactly what the counting tests are asserting.
    //
    // `body["system"]`, not `body["messages"]`: `chat_json_inner` posts to
    // `/api/generate`, whose body is `{system, prompt, format, options}`.
    // (The `marker` below reads `messages` and has therefore always been
    // empty — pre-existing, and left alone.)
    let system = body["system"].as_str().unwrap_or("");
    if system.contains("CURRENT WORKING STATE") {
        return axum::Json(json!({
            "response": json!({
                "goal": "stub goal",
                "open": "stub open",
                "next_action": "stub next",
            })
            .to_string()
        }))
        .into_response();
    }

    // Two facts per chunk, one of each fact_type, so the test can check the
    // `assistant` -> `experience` rename and the ordering offsets.
    let user = body["messages"][1]["content"].as_str().unwrap_or("");
    let marker: String = user.chars().rev().take(8).collect();
    let facts = json!({
        "facts": [
            { "what": format!("chunk fact A [{marker}]"), "fact_type": "world",
              "fact_kind": "event", "occurred_start": "2024-06-10" },
            { "what": format!("chunk fact B [{marker}]"), "fact_type": "assistant",
              "fact_kind": "conversation" },
        ]
    });
    axum::Json(json!({ "response": facts.to_string() })).into_response()
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Harness {
    app: axum::Router,
    db: Arc<Db>,
}

fn build(
    ollama_url: &str,
    tweak: impl FnOnce(&mut Config),
) -> (
    Harness,
    tokio::sync::mpsc::Receiver<retain::RetainTask>,
    AppState,
) {
    let db = Arc::new(Db::open_memory().unwrap());
    let mut cfg = Config::defaults().unwrap();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    cfg.embedding.enabled = false;
    cfg.ollama.base_url = ollama_url.to_string();
    cfg.ollama.request_timeout_secs = 5;
    cfg.ollama.max_retries = 0;
    cfg.retain.include_tool_calls = true;
    // Off by default in tests, ON in production. Nearly every test in this
    // file asserts `calls == chunks_total`, and the ledger's one extra call
    // per job would make that invariant read as "one per chunk, plus one",
    // which is a worse thing for those tests to be pinning. The wiring gets
    // its own test below, which turns it back on.
    cfg.retain.write_task_ledger = false;
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
        events: memgardend::events::channel(),
    };
    let app = routes::router(state.clone());
    (Harness { app, db }, retain_rx, state)
}

/// Router + a live background worker: the full daemon minus the listener.
async fn with_worker(ollama_url: &str, tweak: impl FnOnce(&mut Config)) -> Harness {
    let (harness, rx, state) = build(ollama_url, tweak);
    tokio::spawn(retain::run_worker(state, rx));
    harness
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

/// Polls the job row until it leaves `pending`/`running`. 12s is plenty for
/// the stub-Ollama tests; the live test passes its own budget.
async fn await_job(db: &Db, job_id: &str) -> memgarden_store::retain_jobs::RetainJob {
    await_job_within(db, job_id, Duration::from_secs(12)).await
}

async fn await_job_within(
    db: &Db,
    job_id: &str,
    budget: Duration,
) -> memgarden_store::retain_jobs::RetainJob {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        let job = memgarden_store::retain_jobs::get(db, job_id)
            .unwrap()
            .expect("job row must exist the moment the 202 lands");
        // Terminality comes from the type, not from a list of strings kept
        // in step by hand -- `Partial` was added and this helper hung for its
        // whole budget because the list did not know about it.
        if memgarden_store::retain_jobs::JobStatus::parse(&job.status)
            .is_some_and(|s| s.is_terminal())
        {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("retain job {job_id} did not finish within {budget:?}");
}

/// `(id, fact_type, text, mentioned_at, occurred_start)` as read back from
/// `memory_nodes` in the worker test.
type NodeRow = (i64, String, String, Option<i64>, Option<i64>);

/// A transcript long enough to split into several chunks at `chunk_size`.
fn transcript(turns: usize) -> Vec<Value> {
    (0..turns)
        .map(|i| {
            json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": format!("turn {i}: {}", "discussing the retain pipeline. ".repeat(6)),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Endpoint contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_bank_is_404() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/nope/retain",
            json!({ "messages": [{ "role": "user", "content": "a long enough message here" }], "is_initial": true }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn empty_messages_is_400() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();
    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": [], "is_initial": true }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "invalid");
}

/// Review MEDIUM: `sanitize_tags` used to run on `body.tags` only, so a
/// `session_id` carrying control characters (or 4KB of them) went into
/// `node_tags` unchecked — and the total count cap was bypassable by
/// splitting tags across sources.
#[tokio::test]
async fn every_tag_source_is_sanitized_not_just_the_caller_supplied_ones() {
    let (harness, mut rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let flood: Vec<String> = (0..40).map(|i| format!("flood{i}")).collect();
    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": [{ "role": "user", "content": "please remember the sanitizer" }],
                "is_initial": true,
                "session_id": "sess\u{7}with\u{1b}control",
                "tags": flood,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let task = rx.try_recv().expect("a task must have been queued");
    assert!(
        !task.tags.iter().any(|t| t.chars().any(char::is_control)),
        "no control characters may survive any source: {:?}",
        task.tags
    );
    assert!(
        !task.tags.iter().any(|t| t.starts_with("session:sess\u{7}")),
        "the raw session tag must be dropped, not passed through: {:?}",
        task.tags
    );
    assert!(
        task.tags.len() <= 32,
        "the count cap applies to the combined list: {}",
        task.tags.len()
    );
}

/// PR #8 review LOW: a body that is valid JSON but the wrong shape used to
/// come back as axum's own 422 with a `text/plain` payload — the one failure
/// mode on the whole API that did not speak the error envelope. `is_initial`
/// is the field this matters most for: it is required precisely so a caller
/// who forgets it cannot silently take the uncapped branch.
#[tokio::test]
async fn a_missing_required_field_is_400_in_the_error_envelope() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();
    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": [{ "role": "user", "content": "hi" }] }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "application/json"
    );
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "invalid");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is_initial"),
        "serde's own message must survive: {body}"
    );
}

#[tokio::test]
async fn nothing_to_retain_is_a_clean_skip_not_an_error() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();
    // Only injected memories: everything is stripped, nothing is left.
    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": [
                { "role": "user", "content": "<memgarden_memories>injected</memgarden_memories>" }
            ], "is_initial": true }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["status"], "skipped");
}

#[tokio::test]
async fn full_queue_is_429_and_leaves_no_rows_behind() {
    // capacity 1, no worker draining it: the second request must 429.
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |cfg| cfg.retain.queue_capacity = 1);
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let body = json!({ "messages": transcript(4), "session_id": "s1", "is_initial": true });
    let first = harness
        .app
        .clone()
        .oneshot(post("/v1/banks/b1/retain", body))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);

    let body = json!({ "messages": transcript(4), "session_id": "s2", "is_initial": true });
    let second = harness
        .app
        .clone()
        .oneshot(post("/v1/banks/b1/retain", body))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body_json(second).await["error"]["code"], "queue_full");

    // The rejected request reserved its slot before touching the DB, so it
    // created neither a document nor a job.
    let jobs: i64 = harness
        .db
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM retain_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(jobs, 1);
    let docs: i64 = harness
        .db
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(docs, 1);
}

#[tokio::test]
async fn identical_content_is_a_duplicate_only_after_a_clean_ingest() {
    let (url, _calls) = spawn_stub_ollama(vec![]).await;
    let harness = with_worker(&url, |_| {}).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();
    let body =
        json!({ "messages": transcript(4), "session_id": "same-session", "is_initial": true });

    let first = harness
        .app
        .clone()
        .oneshot(post("/v1/banks/b1/retain", body.clone()))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first = body_json(first).await;
    let first_doc = first["document_id"].clone();
    let job = await_job(&harness.db, first["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "done");

    // Only now — after the worker stamped the content hash — is a re-POST a
    // duplicate (review HIGH 1).
    let second = harness
        .app
        .clone()
        .oneshot(post("/v1/banks/b1/retain", body))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = body_json(second).await;
    assert_eq!(second_body["status"], "duplicate");
    assert_eq!(second_body["document_id"], first_doc);
    assert_eq!(second_body["job_id"], Value::Null);

    let jobs: i64 = harness
        .db
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM retain_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(jobs, 1);
}

#[tokio::test]
async fn a_partially_failed_job_is_retryable_not_a_permanent_duplicate() {
    // Review HIGH 1, the data-loss case: chunk 2 of 3 fails, so the document
    // must NOT be marked fully ingested. Re-POSTing the identical transcript
    // has to start a fresh job, not answer "duplicate" forever.
    let (url, _calls) = spawn_stub_ollama(vec![2]).await;
    let harness = with_worker(&url, |cfg| cfg.retain.chunk_size = 400).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();
    let body = json!({ "messages": transcript(6), "session_id": "retryable", "is_initial": true });

    let first = body_json(
        harness
            .app
            .clone()
            .oneshot(post("/v1/banks/b1/retain", body.clone()))
            .await
            .unwrap(),
    )
    .await;
    let job = await_job(&harness.db, first["job_id"].as_str().unwrap()).await;
    // Finished, and honest about what it lost — which is the precondition for
    // the retry this test is about: the content hash is withheld on exactly
    // this status, so the second POST is a fresh job rather than a duplicate.
    assert_eq!(job.status, "partial");
    assert_eq!(
        job.chunks_failed, 1,
        "the fixture must actually fail a chunk"
    );

    let second = harness
        .app
        .clone()
        .oneshot(post("/v1/banks/b1/retain", body.clone()))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    let second = body_json(second).await;
    assert_eq!(
        second["status"], "accepted",
        "a partial ingest must be retryable"
    );
    assert_eq!(second["document_id"], first["document_id"]);
    assert_ne!(second["job_id"], first["job_id"]);

    // The retry succeeds end to end (the stub only fails call #2), stamps the
    // hash, and only THEN is a third post a duplicate.
    let job = await_job(&harness.db, second["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "done");
    assert_eq!(job.chunks_failed, 0);
    let third = harness
        .app
        .oneshot(post("/v1/banks/b1/retain", body))
        .await
        .unwrap();
    assert_eq!(body_json(third).await["status"], "duplicate");
}

#[tokio::test]
async fn a_duplicate_records_no_second_ledger_row() {
    // The ledger measures work avoided, not requests received: a re-sent
    // transcript ingests nothing, so it must not inflate the numbers.
    let (url, _calls) = spawn_stub_ollama(vec![]).await;
    let harness = with_worker(&url, |_| {}).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();
    let body = json!({
        "messages": [
            { "role": "user", "content": "write the module" },
            { "role": "assistant", "content": [
                { "type": "tool_use", "name": "Write", "input": {
                    "file_path": "/repo/src/a.rs", "content": "fn a() {}\n".repeat(300) } },
            ]},
        ],
        "session_id": "dup-ledger",
        "cwd": "/repo",
        "is_initial": true,
    });

    let first = body_json(
        harness
            .app
            .clone()
            .oneshot(post("/v1/banks/b1/retain", body.clone()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        memgarden_store::metrics_store::list_ledger(&harness.db, 10)
            .unwrap()
            .len(),
        1
    );
    await_job(&harness.db, first["job_id"].as_str().unwrap()).await;

    let second = harness
        .app
        .clone()
        .oneshot(post("/v1/banks/b1/retain", body))
        .await
        .unwrap();
    assert_eq!(body_json(second).await["status"], "duplicate");
    assert_eq!(
        memgarden_store::metrics_store::list_ledger(&harness.db, 10)
            .unwrap()
            .len(),
        1,
        "a duplicate must not write a second retain_cap_saving row"
    );
}

#[tokio::test]
async fn coding_profile_supplies_a_default_bank_mission() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |cfg| {
        cfg.profile.name = "coding".to_string();
        cfg.profile.bank_mission = "You are a coding assistant.".to_string();
    });
    let created = harness
        .app
        .clone()
        .oneshot(post("/v1/banks", json!({ "bank_id": "inherits" })))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(created).await["mission"],
        "You are a coding assistant."
    );

    // An explicit mission still wins.
    let created = harness
        .app
        .oneshot(post(
            "/v1/banks",
            json!({ "bank_id": "explicit", "mission": "mine" }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(created).await["mission"], "mine");
}

#[tokio::test]
async fn unknown_job_id_is_404() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    let response = harness.app.oneshot(get("/v1/retain/nope")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cap_saving_writes_a_ledger_row_with_the_right_ratio() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    // A 6KB Write: tier-1 field truncation removes almost all of it.
    let messages = json!([
        { "role": "user", "content": "please write the reranker module" },
        { "role": "assistant", "content": [
            { "type": "text", "text": "writing it now" },
            { "type": "tool_use", "name": "Write", "input": {
                "file_path": "/repo/src/rerank.rs",
                "content": "fn rerank() { /* body */ }\n".repeat(250),
            }},
        ]},
    ]);
    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": messages, "session_id": "sess-ledger", "cwd": "/repo", "is_initial": true }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;

    let raw = body["raw_tokens"].as_u64().unwrap();
    let capped = body["capped_tokens"].as_u64().unwrap();
    let saved = body["saved_tokens"].as_u64().unwrap();
    let ratio = body["saving_ratio"].as_f64().unwrap();
    assert_eq!(saved, raw - capped);
    assert!(
        ratio > 0.55,
        "a 6KB Write must land in the measured -55%..-87% band, got {ratio}"
    );

    let ledger = memgarden_store::metrics_store::list_ledger(&harness.db, 10).unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].kind, "retain_cap_saving");
    assert_eq!(ledger[0].bank_id.as_deref(), Some("b1"));
    let detail: Value = serde_json::from_str(ledger[0].detail.as_deref().unwrap()).unwrap();
    assert_eq!(detail["raw_tokens"], raw);
    assert_eq!(detail["capped_tokens"], capped);
    assert_eq!(detail["saved"], saved);
    assert_eq!(detail["session_id"], "sess-ledger");
    assert!((detail["ratio"].as_f64().unwrap() - ratio).abs() < 1e-9);
}

#[tokio::test]
async fn no_ledger_row_when_nothing_was_capped() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": transcript(4), "session_id": "sess-plain", "is_initial": true }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(body["saved_tokens"], 0);
    assert_eq!(body["saving_ratio"], 0.0);
    assert!(
        memgarden_store::metrics_store::list_ledger(&harness.db, 10)
            .unwrap()
            .is_empty(),
        "the ledger must record real benefit only"
    );
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn worker_writes_nodes_with_tags_and_ordering_offsets() {
    let (url, calls) = spawn_stub_ollama(vec![]).await;
    // Small chunks so the transcript really splits and the absolute fact
    // index has to survive a chunk boundary.
    let harness = with_worker(&url, |cfg| cfg.retain.chunk_size = 400).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let mut messages = transcript(6);
    messages.push(json!({
        "role": "assistant",
        "content": [{ "type": "tool_use", "name": "Edit",
                      "input": { "file_path": "/repo/src/retain.rs" } }],
    }));
    let response = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": messages,
                "session_id": "sess-worker",
                "cwd": "/repo",
                "is_initial": true,
                "context": "claude-code",
                "tags": ["agent:claude-code", "", "  ", "bad\u{7}tag"],
                "event_date": 1_717_977_600_000i64,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let job = await_job(&harness.db, &job_id).await;
    assert_eq!(job.status, "done");
    assert!(job.chunks_total > 1, "the transcript must have split");
    assert_eq!(job.chunks_done, job.chunks_total);
    assert_eq!(job.chunks_failed, 0);
    assert_eq!(job.facts_written, job.chunks_total * 2);
    assert_eq!(calls.load(Ordering::SeqCst) as i64, job.chunks_total);

    let conn = harness.db.read().unwrap();
    let rows: Vec<NodeRow> = conn
        .prepare(
            "SELECT id, fact_type, text, mentioned_at, occurred_start
             FROM memory_nodes WHERE bank_id = 'b1' ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len() as i64, job.facts_written);

    // +10ms per fact, counted across the WHOLE document (NIT 16) — not
    // restarted per chunk.
    let base = 1_717_977_600_000i64;
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            row.3,
            Some(base + i as i64 * 10),
            "fact {i} must carry the document-absolute ordering offset"
        );
    }
    // The stub alternates world / experience ("assistant" is renamed).
    assert_eq!(rows[0].1, "world");
    assert_eq!(rows[1].1, "experience");
    // Event facts got occurred_start (offset too); conversation facts did not.
    assert_eq!(rows[0].4, Some(1_717_977_600_000));
    assert_eq!(rows[1].4, None);

    // Tags: configured + session: + file:.
    let tags = memgarden_store::nodes::tags_of(&harness.db, rows[0].0).unwrap();
    assert!(tags.contains(&"agent:claude-code".to_string()));
    assert!(tags.contains(&"session:sess-worker".to_string()));
    assert!(tags.contains(&"file:src/retain.rs".to_string()));
    // The request also carried "", "  " and a control-character tag; all
    // three are dropped before anything is written (security review).
    assert_eq!(
        tags.len(),
        3,
        "junk tags must not reach node_tags: {tags:?}"
    );

    // Embeddings are left NULL for B1's backlog worker (documented divergence).
    let pending = memgarden_store::nodes::pending_embeddings(&harness.db, 100).unwrap();
    assert_eq!(pending.len(), rows.len());

    // The document carries the comma-joined files_modified.
    let doc_meta = memgarden_store::documents::get_metadata(&harness.db, job.document_id.unwrap())
        .unwrap()
        .unwrap();
    let doc_meta: Value = serde_json::from_str(&doc_meta).unwrap();
    assert_eq!(doc_meta["files_modified"], "src/retain.rs");
    assert_eq!(doc_meta["session_id"], "sess-worker");

    // GET /v1/retain/{job_id} mirrors the row.
    let response = harness
        .app
        .clone()
        .oneshot(get(&format!("/v1/retain/{job_id}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["status"], "done");
    assert_eq!(body["facts_written"], job.facts_written);
    assert_eq!(body["detail"]["message_count"], 7);
}

#[tokio::test]
async fn one_failed_chunk_does_not_fail_the_job() {
    // Critic Revision R14: the second of three chunks fails; the job still
    // completes with the other two written.
    let (url, calls) = spawn_stub_ollama(vec![2]).await;
    let harness = with_worker(&url, |cfg| cfg.retain.chunk_size = 400).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": transcript(6), "session_id": "sess-partial", "is_initial": true }),
        ))
        .await
        .unwrap();
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let job = await_job(&harness.db, &job_id).await;
    // The invariant this test protects is "one failed chunk must not fail the
    // whole job". It used to encode that as `== "done"`, which stopped being
    // the same claim when `partial` arrived: the job is finished, it is not
    // failed, and it says out loud that it lost something.
    assert_ne!(
        job.status, "failed",
        "a partial failure is not a failed job"
    );
    assert_eq!(
        job.status, "partial",
        "a job that lost a chunk must not report the same status as one that lost nothing"
    );
    assert_eq!(job.chunks_failed, 1);
    assert_eq!(job.chunks_done, job.chunks_total - 1);
    assert_eq!(job.facts_written, (job.chunks_total - 1) * 2);
    assert!(
        job.error.is_some(),
        "the failure is recorded, not swallowed"
    );
    assert!(calls.load(Ordering::SeqCst) >= 3);
}

/// `all_failed` is "no chunk got through", not "nothing was written". A
/// transcript whose chunks legitimately hold nothing worth keeping, with one
/// chunk lost to a 500, is partial: failing it rewound the cursor and
/// re-extracted every empty chunk on the next post.
#[tokio::test]
async fn empty_chunks_beside_one_failure_are_partial_not_failed() {
    let (url, _calls) = spawn_stub_ollama_with(vec![2], vec![1, 3, 4, 5, 6, 7, 8]).await;
    let harness = with_worker(&url, |cfg| cfg.retain.chunk_size = 400).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": transcript(6), "session_id": "sess-empty", "is_initial": true }),
        ))
        .await
        .unwrap();
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let job = await_job(&harness.db, &job_id).await;
    assert_eq!(job.facts_written, 0);
    assert_eq!(job.chunks_failed, 1);
    assert_eq!(job.chunks_done, job.chunks_total - 1);
    assert_eq!(
        job.status, "partial",
        "empty extractions are chunks that got through, not failures"
    );
}

#[tokio::test]
async fn every_chunk_failing_fails_the_job() {
    let (url, _calls) = spawn_stub_ollama(vec![1, 2, 3, 4, 5, 6, 7, 8]).await;
    let harness = with_worker(&url, |cfg| cfg.retain.chunk_size = 400).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": transcript(6), "session_id": "sess-dead", "is_initial": true }),
        ))
        .await
        .unwrap();
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let job = await_job(&harness.db, &job_id).await;
    assert_eq!(job.status, "failed");
    assert_eq!(job.chunks_done, 0);
    assert_eq!(job.facts_written, 0);
    assert!(job.error.unwrap().contains("500"));
}

#[tokio::test]
async fn wall_timeout_fails_the_job_and_keeps_partial_progress() {
    // Critic Revision R11: `wall_timeout_secs = 0` would be rejected by
    // config validation, so use 1s against a stub that sleeps.
    let (url, _calls) = spawn_stub_ollama(vec![]).await;
    let harness = with_worker(&url, |cfg| {
        cfg.retain.chunk_size = 200;
        cfg.retain.wall_timeout_secs = 1;
    })
    .await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": transcript(200), "session_id": "sess-slow", "is_initial": true }),
        ))
        .await
        .unwrap();
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = await_job(&harness.db, &job_id).await;

    // Either it finished inside the second (a fast stub, many small chunks)
    // or the wall clock cut it off — in the latter case the partial progress
    // must be intact and the reason recorded.
    if job.status == "failed" {
        assert!(job.error.unwrap().contains("wall timeout"));
        assert!(job.chunks_done > 0, "partial progress must be preserved");
        assert!(job.chunks_done < job.chunks_total);
    } else {
        assert_eq!(job.status, "done");
    }
}

#[tokio::test]
async fn backfill_cap_applies_only_to_the_initial_retain() {
    let (url, _calls) = spawn_stub_ollama(vec![]).await;
    let (harness, _rx, _state) = build(&url, |cfg| cfg.retain.max_initial_messages = 4);
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let messages = transcript(20);
    let initial = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": messages, "is_initial": true, "session_id": "s-init" }),
        ))
        .await
        .unwrap();
    let initial = body_json(initial).await;

    let messages = transcript(20);
    let delta = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": messages, "is_initial": false, "session_id": "s-delta" }),
        ))
        .await
        .unwrap();
    let delta = body_json(delta).await;

    // Same raw baseline (all 20 messages), but the initial retain's capped
    // payload is far smaller because only the last 4 survived.
    assert_eq!(initial["raw_tokens"], delta["raw_tokens"]);
    assert!(
        initial["capped_tokens"].as_u64().unwrap() < delta["capped_tokens"].as_u64().unwrap() / 3,
        "the last-4 backfill cap must dominate: initial={} delta={}",
        initial["capped_tokens"],
        delta["capped_tokens"]
    );
    assert!(initial["saved_tokens"].as_u64().unwrap() > 0);
    assert_eq!(delta["saved_tokens"], 0);
}

#[tokio::test]
async fn degenerate_chunks_never_reach_ollama() {
    // CE-5a review carry-over. Two independent guards keep a zero-information
    // chunk away from the (single-permit, seconds-per-call) LLM:
    //   1. the chunker never emits a blank piece, and
    //   2. run_job re-checks each chunk with the same degenerate-text
    //      predicate the fact parser uses.
    for junk in ["...", "   ", "--", "\u{2026}", "_, _, _"] {
        assert!(
            memgardend::extract::parse::is_degenerate_text(junk),
            "{junk:?} must be recognised as degenerate"
        );
    }
    assert!(!memgardend::extract::parse::is_degenerate_text(
        "[role: user]\nreal content\n[user:end]"
    ));

    let padded = format!("{}\n\n   \n\n{}", "a. ".repeat(400), "b. ".repeat(400));
    for chunk in memgardend::retain::chunk::chunk_text(&padded, 300) {
        assert!(!chunk.trim().is_empty(), "the chunker must not emit blanks");
    }

    // And end to end: a junk-only transcript still round-trips cleanly (the
    // role markers make the transcript itself non-degenerate, exactly as in
    // legacy) rather than erroring.
    let (url, _calls) = spawn_stub_ollama(vec![]).await;
    let harness = with_worker(&url, |cfg| cfg.retain.include_tool_calls = false).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();
    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": [{ "role": "user", "content": "..." }], "session_id": "s-junk", "is_initial": true }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(await_job(&harness.db, &job_id).await.status, "done");
}

#[tokio::test]
async fn the_raised_body_limit_is_scoped_to_the_retain_route() {
    // axum's 2MB default still guards every other route; only /retain is
    // allowed a real transcript (review LOW 17).
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let big = "p".repeat(3 * 1024 * 1024);
    let rejected = harness
        .app
        .clone()
        .oneshot(post("/v1/banks", json!({ "bank_id": "x", "mission": big })))
        .await
        .unwrap();
    assert_eq!(
        rejected.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "the 2MB default must still apply to /v1/banks"
    );

    let big = "p".repeat(3 * 1024 * 1024);
    let accepted = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": [{ "role": "user", "content": big }],
                "session_id": "big",
                "is_initial": true,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        accepted.status(),
        StatusCode::ACCEPTED,
        "a 3MB transcript must get through the retain route"
    );
}

#[tokio::test]
async fn metrics_expose_the_retain_counters() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();
    let response = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({ "messages": transcript(4), "session_id": "s-metrics", "is_initial": true }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let response = harness.app.oneshot(get("/metrics.json")).await.unwrap();
    let body = body_json(response).await;
    assert!(body["retain_requests"].as_u64().unwrap() >= 1);
    assert!(body["retain_tokens_raw"].as_u64().unwrap() > 0);
    assert!(body["retain_tokens_capped"].as_u64().unwrap() > 0);
    assert!(body["retain_chunks_failed"].is_number());
    assert!(body["retain_cap_savings"].is_number());
    assert!(body["retain_latency"].is_object());
}

// ---------------------------------------------------------------------------
// Live (manual)
// ---------------------------------------------------------------------------

/// The plan's §PR B3 manual verification, automated: replay a realistic
/// Claude Code transcript through the real Ollama and print the observed
/// `saving_ratio` so it can be checked against `docs/measurement.md`'s
/// -55% / -87% band, plus the ledger row and the facts written.
///
/// Run: `cargo test -p memgardend --test retain_api live_retain -- --ignored --nocapture`
#[tokio::test]
#[ignore = "requires a running Ollama with the configured model"]
async fn live_retain() {
    let cfg = Config::defaults().expect("default config");
    let (harness, rx, state) = build(&cfg.ollama.base_url.clone(), |c| {
        c.retain.include_tool_calls = true;
        c.profile.name = "coding".to_string();
        c.ollama = cfg.ollama.clone();
    });
    tokio::spawn(retain::run_worker(state, rx));
    memgarden_store::banks::create(&harness.db, "live", None, None).unwrap();

    let messages = json!([
        { "role": "user", "content": "recall latency regressed to 830ms after wiring the reranker. fix it." },
        { "role": "assistant", "content": [
            { "type": "text", "text": "The embedding model is competing for VRAM with the resident 13GB Ollama model. Forcing CPU inference for embeddings and the reranker." },
            { "type": "tool_use", "name": "Edit", "input": {
                "file_path": "/repo/hindsight_api/engine/local_device.py",
                "old_string": "device = 'cuda'\n".repeat(120),
                "new_string": "device = 'cpu'  # VRAM contention with the resident LLM\n".repeat(120),
            }},
            { "type": "tool_result", "tool_use_id": "t1", "content": "ok\n".repeat(2000) },
            { "type": "text", "text": "Recall p50 is now 20-37ms. The per-request gc.collect in the reranker was the other half." },
        ]},
    ]);

    let started = std::time::Instant::now();
    let response = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/live/retain",
            json!({
                "messages": messages,
                "session_id": "live-session",
                "cwd": "/repo",
                "context": "claude-code",
                "is_initial": true,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    println!(
        "live_retain: raw={} capped={} saved={} ratio={:.3}",
        body["raw_tokens"], body["capped_tokens"], body["saved_tokens"], body["saving_ratio"]
    );

    let job_id = body["job_id"].as_str().unwrap().to_string();
    let job = await_job_within(&harness.db, &job_id, Duration::from_secs(600)).await;
    println!(
        "live_retain: status={} chunks={}/{} failed={} facts={} wall={:.1}s",
        job.status,
        job.chunks_done,
        job.chunks_total,
        job.chunks_failed,
        job.facts_written,
        started.elapsed().as_secs_f64()
    );
    for entry in memgarden_store::metrics_store::list_ledger(&harness.db, 5).unwrap() {
        println!("live_retain: ledger {} {:?}", entry.kind, entry.detail);
    }
    assert_eq!(job.status, "done");
    assert!(job.facts_written > 0, "live retain wrote no facts");
}

// ---------------------------------------------------------------------------
// HK-1a: the session mirror
// ---------------------------------------------------------------------------

fn session(db: &Db, session_id: &str) -> memgarden_store::sessions::Session {
    session_in(db, "b1", session_id)
}

fn session_in(db: &Db, bank_id: &str, session_id: &str) -> memgarden_store::sessions::Session {
    memgarden_store::sessions::get(db, bank_id, session_id)
        .unwrap()
        .unwrap_or_else(|| panic!("sessions row for {session_id} must exist"))
}

fn jobs_for(db: &Db, session_id: &str) -> i64 {
    db.read()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM retain_jobs WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// The happy path, and the reason there are two cursors: the optimistic one
/// moves when the job is *queued*, the durable one only when it finishes
/// clean. A single-cursor mirror cannot express the middle state this test
/// asserts.
#[tokio::test]
async fn a_retain_mirrors_the_session_and_confirms_it_on_a_clean_run() {
    let (url, _calls) = spawn_stub_ollama(vec![]).await;
    let harness = with_worker(&url, |cfg| cfg.retain.chunk_size = 4000).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": transcript(4),
                "session_id": "mirrored",
                "is_initial": true,
                "cwd": "/repo",
                "offset_from": 0,
                "byte_offset": 8192,
                "turn": 30,
                "compactions": 2,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The mirror is written by `prepare`, so it exists the moment the 202
    // lands — the same guarantee the job row gives.
    let row = session(&harness.db, "mirrored");
    assert_eq!(row.byte_offset, 8192);
    assert_eq!(row.turns, 30);
    assert_eq!(row.compactions, 2);
    assert_eq!(row.retains, 1);
    assert_eq!(row.cwd.as_deref(), Some("/repo"));
    assert!(row.messages_sent > 0);
    // Not yet ingested: 202 means queued.
    assert_eq!(row.confirmed_offset, 0);
    assert_eq!(row.inflight_bytes(), 8192);

    let job = await_job(&harness.db, &job_id).await;
    assert_eq!(job.status, "done");
    assert_eq!(job.chunks_failed, 0);

    let row = session(&harness.db, "mirrored");
    assert_eq!(
        row.confirmed_offset, 8192,
        "a clean run advances the durable cursor"
    );
    assert_eq!(row.byte_offset, 8192);
    assert_eq!(row.inflight_bytes(), 0);
}

/// The blocker case. A job that fails a chunk leaves the document hash-less
/// so the transcript can be re-sent — and the mirror has to say so, or the
/// hook and the dashboard have no way to see the hole.
#[tokio::test]
async fn a_failed_chunk_leaves_the_durable_cursor_behind() {
    let (url, _calls) = spawn_stub_ollama(vec![2]).await;
    let harness = with_worker(&url, |cfg| cfg.retain.chunk_size = 400).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": transcript(6),
                "session_id": "lagging",
                "is_initial": true,
                "byte_offset": 4096,
            }),
        ))
        .await
        .unwrap();
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let job = await_job(&harness.db, &job_id).await;
    assert_eq!(
        job.chunks_failed, 1,
        "the fixture must actually fail a chunk"
    );

    let row = session(&harness.db, "lagging");
    assert_eq!(row.byte_offset, 4096);
    assert_eq!(
        row.confirmed_offset, 0,
        "an unclean run must not confirm anything"
    );
    assert_eq!(
        row.inflight_bytes(),
        4096,
        "the gap is exactly the bytes that are not known-ingested"
    );

    // Reconciliation: the two tables agree without either duplicating the
    // other. `sessions` carries the count and the cursors; the per-chunk
    // detail behind that one retain lives only in `retain_jobs`, joined on
    // `session_id`.
    assert_eq!(jobs_for(&harness.db, "lagging"), row.retains);
    assert_eq!(job.chunks_failed, 1);
    assert_eq!(row.compactions, 0, "no compaction was reported");
}

/// `skipped` and `duplicate` are accepts that queue nothing, so they may
/// settle the durable cursor — **but only when nothing earlier is
/// outstanding** (review HIGH 1). This test walks both halves: the clean
/// ordering where they do confirm, and the ordering where a queued job is
/// unresolved and the gap must survive.
///
/// The earlier version of this test asserted the defect: it confirmed at
/// 1500 on a duplicate whose identical payload was itself proof that nothing
/// had ingested 900..1500.
#[tokio::test]
async fn skipped_and_duplicate_settle_only_when_nothing_is_outstanding() {
    let (url, _calls) = spawn_stub_ollama(vec![]).await;
    let harness = with_worker(&url, |cfg| cfg.retain.chunk_size = 4000).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    // `skipped`: the delta empties out under role filtering. Ordinary, not
    // exotic — `retain.roles` is user+assistant.
    let response = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": [{ "role": "system", "content": "a system notice nobody retains" }],
                "session_id": "settled",
                "is_initial": true,
                "byte_offset": 100,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(response).await["status"], "skipped");
    let row = session(&harness.db, "settled");
    assert_eq!(row.byte_offset, 100);
    assert_eq!(row.confirmed_offset, 100);
    assert_eq!(row.retains, 1);

    // A real retain, then the identical bytes again -> `duplicate`.
    // `offset_from` is what lets the clean run confirm: without it the worker
    // declines rather than guessing, and the `duplicate` below would then find
    // an open gap and refuse to settle.
    let body = json!({
        "messages": transcript(4),
        "session_id": "settled",
        "is_initial": false,
        "offset_from": 100,
        "byte_offset": 900,
    });
    let response = harness
        .app
        .clone()
        .oneshot(post("/v1/banks/b1/retain", body.clone()))
        .await
        .unwrap();
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(await_job(&harness.db, &job_id).await.status, "done");

    let again = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": transcript(4),
                "session_id": "settled",
                "is_initial": false,
                "byte_offset": 1500,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(again).await["status"], "duplicate");
    let row = session(&harness.db, "settled");
    assert_eq!(row.byte_offset, 1500);
    assert_eq!(
        row.confirmed_offset, 1500,
        "nothing was outstanding, so the duplicate may settle"
    );
    assert_eq!(row.retains, 3, "skipped and duplicate are both accepts");
    // Reconciliation, stated as the inequality it actually is: two of those
    // three accepts queued no job at all.
    assert_eq!(
        jobs_for(&harness.db, "settled"),
        1,
        "retains ({}) exceeds the job count by exactly the skipped + duplicate accepts",
        row.retains
    );
}

/// The other half of HIGH 1, and the case that was silently broken: a
/// `skipped` landing at a higher offset while an earlier job is still
/// unresolved must NOT close that job's gap. No worker runs here, so the
/// first job never completes — the shape of a queued-then-failed retain.
#[tokio::test]
async fn a_later_skipped_does_not_swallow_an_unresolved_jobs_gap() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let queued = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": transcript(6),
                "session_id": "s1",
                "is_initial": true,
                "byte_offset": 5000,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(queued.status(), StatusCode::ACCEPTED);
    assert_eq!(session(&harness.db, "s1").inflight_bytes(), 5000);

    // An ordinary role-filtered delta emptying out — the plan's own words.
    let skipped = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": [{ "role": "system", "content": "tool noise nobody retains" }],
                "session_id": "s1",
                "is_initial": false,
                "byte_offset": 6000,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(skipped).await["status"], "skipped");

    let row = session(&harness.db, "s1");
    assert_eq!(row.byte_offset, 6000);
    assert_eq!(
        row.confirmed_offset, 0,
        "the unresolved job's gap must not be swallowed"
    );
    assert_eq!(row.inflight_bytes(), 6000);
    assert_eq!(
        jobs_for(&harness.db, "s1"),
        1,
        "and that one job is still sitting there unfinished"
    );
}

/// A caller that is not the hook — no `byte_offset`, no `turn` — must not
/// reset a mirror the hook has been maintaining. The monotonic merge is what
/// makes that true, and an out-of-order `async: true` `Stop` is the same
/// shape.
#[tokio::test]
async fn a_retain_without_the_hook_fields_does_not_clobber_the_mirror() {
    let (url, _calls) = spawn_stub_ollama(vec![]).await;
    let harness = with_worker(&url, |cfg| cfg.retain.chunk_size = 4000).await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let first = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": transcript(4),
                "session_id": "keeper",
                "is_initial": true,
                "byte_offset": 7000,
                "turn": 40,
                "compactions": 3,
            }),
        ))
        .await
        .unwrap();
    // Drained before the second POST on purpose. `Db::open_memory` is a
    // shared-cache database, where two connections writing the same table
    // get `SQLITE_LOCKED` — which `busy_timeout` does not retry, unlike the
    // `SQLITE_BUSY` a file database returns. The overlap is fine in
    // production and a coin flip in this harness, so the test pins the
    // merge semantics rather than SQLite's locking.
    let job_id = body_json(first).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    await_job(&harness.db, &job_id).await;

    harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": transcript(8),
                "session_id": "keeper",
                "is_initial": false,
            }),
        ))
        .await
        .unwrap();

    let row = session(&harness.db, "keeper");
    assert_eq!(row.byte_offset, 7000);
    assert_eq!(row.turns, 40);
    assert_eq!(row.compactions, 3);
    assert_eq!(row.retains, 2, "the accept is still counted");
}

/// `POST …/retain` is a public endpoint, not only the hook's. A request with
/// no `session_id` has no session to mirror.
#[tokio::test]
async fn a_retain_without_a_session_id_writes_no_session_row() {
    let (harness, mut rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": transcript(2),
                "is_initial": true,
                "byte_offset": 1234,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    rx.try_recv().expect("a task must have been queued");

    let rows: i64 = harness
        .db
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

/// Review HIGH 2: the mirror must never fail a retain, and a `session_id`
/// the mirror would reject must be caught **before** any DB work.
///
/// The defect: `sessions::upsert` enforced `MAX_SESSION_ID_BYTES` and the
/// route propagated its error with `?` — after the document, the ledger row
/// and the job row had all committed. The caller got a 400 and the job sat
/// at `pending` forever, never dispatched and never failed, which a
/// §Binding-#8 hook would poll for the rest of the session.
#[tokio::test]
async fn an_oversized_session_id_is_rejected_before_any_row_is_written() {
    let (harness, mut rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": transcript(2),
                "session_id": "x".repeat(201),
                "is_initial": true,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["error"]["code"], "invalid");

    let conn = harness.db.read().unwrap();
    for table in ["retain_jobs", "documents", "sessions", "benefit_ledger"] {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table} must be untouched by a rejected request");
    }
    assert!(
        rx.try_recv().is_err(),
        "and nothing may reach the retain queue"
    );
}

/// `chunk` rides the retain payload, not only `POST …/sessions`: the case
/// `chunk_index` exists for is a state-dir wipe *mid*-session, which
/// end-of-session mirroring cannot reach.
#[tokio::test]
async fn the_retain_payload_mirrors_the_chunk_counter() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    for chunk in [3, 4] {
        harness
            .app
            .clone()
            .oneshot(post(
                "/v1/banks/b1/retain",
                json!({
                    "messages": transcript(2),
                    "session_id": "chunked",
                    "document_id": format!("chunked-c{chunk}"),
                    "is_initial": false,
                    "chunk": chunk,
                }),
            ))
            .await
            .unwrap();
    }
    assert_eq!(session(&harness.db, "chunked").chunk_index, 4);

    // Monotonic like the rest: a stale replay does not rewind it.
    harness
        .app
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": transcript(2),
                "session_id": "chunked",
                "document_id": "chunked-c1",
                "is_initial": false,
                "chunk": 1,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(session(&harness.db, "chunked").chunk_index, 4);
}

/// Binding #4 says test both shapes of real bank id. These are the first
/// endpoints the hook ever calls, and it calls them with a percent-encoded
/// `claude-code::<project>` — `::` in every case, and a space in at least
/// one live bank.
#[tokio::test]
async fn a_real_world_bank_id_survives_the_url_path() {
    let (harness, _rx, _state) = build("http://127.0.0.1:1", |_| {});
    for bank in ["claude-code::bank-b", "claude-code::bank e"] {
        memgarden_store::banks::create(&harness.db, bank, None, None).unwrap();
        let encoded = bank.replace(':', "%3A").replace(' ', "%20");

        let response = harness
            .app
            .clone()
            .oneshot(post(
                &format!("/v1/banks/{encoded}/retain"),
                json!({
                    "messages": transcript(2),
                    "session_id": "sess",
                    "is_initial": true,
                    "byte_offset": 2048,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED, "bank {bank}");
        assert_eq!(session_in(&harness.db, bank, "sess").byte_offset, 2048);

        let listed = harness
            .app
            .clone()
            .oneshot(get(&format!("/v1/banks/{encoded}/sessions/sess")))
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        assert_eq!(body_json(listed).await["bank_id"], bank);
    }
}

// ---------------------------------------------------------------------------
// Task ledger (migration 0012)
// ---------------------------------------------------------------------------

/// Waits for the bank's ledger row, which lands on its own schedule: the
/// call is spawned at POST and shares nothing with the job's status.
async fn await_ledger(db: &Db, bank_id: &str) -> memgarden_store::task_ledger::TaskLedger {
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(led) = memgarden_store::task_ledger::get(db, bank_id).unwrap() {
            return led;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no task ledger row within the budget"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The write path exists and is reached from the POST, not from the job.
///
/// No worker is started, so the queued task is never consumed: a ledger row
/// that appears anyway can only have come from the handler. That is the
/// property the first five live rows were missing — written at the end of a
/// serial worker's queue, they arrived 107 · 102 · 116 · 63 · 19 minutes
/// after the transcript they described, and four of the five named a goal
/// that was already finished.
///
/// This is the only test in the file that turns `write_task_ledger` on, and it
/// also asserts the two things the wiring can get wrong in a way nothing else
/// would notice: that the row is written at all, and that its `anchors` carry
/// the daemon's own values rather than anything the model produced.
#[tokio::test]
async fn the_task_ledger_is_written_when_a_job_is_queued_not_when_it_finishes() {
    let (url, calls) = spawn_stub_ollama(vec![]).await;
    let (harness, _rx, _state) = build(&url, |cfg| {
        cfg.retain.chunk_size = 4000;
        cfg.retain.write_task_ledger = true;
    });
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": [
                    { "role": "user", "content": "fix the cursor defect" },
                    { "role": "assistant", "content": [{ "type": "tool_use", "name": "Edit",
                        "input": { "file_path": "/repo/src/retain.rs" } }] },
                ],
                "session_id": "sess-ledger",
                "cwd": "/repo",
                "is_initial": true,
                "context": "claude-code",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let led = await_ledger(&harness.db, "b1").await;

    // Nothing consumed the queue, so the job is exactly where the POST left
    // it, and the one Ollama call that happened is the ledger's. The cost of
    // this feature is a number, and this is where it is written down.
    let job = memgarden_store::retain_jobs::get(&harness.db, &job_id)
        .unwrap()
        .expect("job row");
    assert_eq!(job.status, "pending", "no worker is running in this test");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exactly one Ollama call per job for the ledger, and it must not wait for extraction"
    );

    assert_eq!(led.goal, "stub goal");
    assert_eq!(led.open, "stub open");
    assert_eq!(led.next_action, "stub next");
    assert_eq!(led.session_id.as_deref(), Some("sess-ledger"));
    assert_eq!(led.job_id.as_deref(), Some(job_id.as_str()));

    // `anchors` is assembled by the daemon, never asked of the model: a
    // fabricated anchor turns the staleness check into false confidence.
    let anchors: Value = serde_json::from_str(&led.anchors).expect("anchors is JSON");
    assert_eq!(anchors["cwd"], "/repo");
    // Relative to `cwd`, which is why both are in the anchor: the pair
    // resolves, either alone does not.
    assert_eq!(anchors["paths"], json!(["src/retain.rs"]));
}

/// The knob is real: off, the job runs and no row appears.
///
/// Note the neighbours: `no_ledger_row_when_nothing_was_capped` in this same
/// file is about the **benefit** ledger (`metrics_store`), which is a
/// different table with a different purpose. These two tests say "task
/// ledger" in full for that reason.
#[tokio::test]
async fn no_task_ledger_is_written_when_the_knob_is_off() {
    let (url, calls) = spawn_stub_ollama(vec![]).await;
    let harness = with_worker(&url, |cfg| {
        cfg.retain.chunk_size = 4000;
        cfg.retain.write_task_ledger = false;
    })
    .await;
    memgarden_store::banks::create(&harness.db, "b1", None, None).unwrap();

    let response = harness
        .app
        .clone()
        .oneshot(post(
            "/v1/banks/b1/retain",
            json!({
                "messages": [{ "role": "user", "content": "fix the cursor defect" }],
                "session_id": "sess-off",
                "is_initial": true,
            }),
        ))
        .await
        .unwrap();
    let job_id = body_json(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = await_job(&harness.db, &job_id).await;

    assert_eq!(job.status, "done");
    assert_eq!(
        calls.load(Ordering::SeqCst) as i64,
        job.chunks_total,
        "no extra call when the knob is off"
    );
    assert_eq!(
        memgarden_store::task_ledger::get(&harness.db, "b1").unwrap(),
        None
    );

    // A settled job records where it ran. No prober runs in this harness,
    // so the status is the startup default; the point is that the field
    // exists next to the route's token accounting rather than only in a log.
    let detail: Value = serde_json::from_str(job.detail.as_deref().unwrap()).unwrap();
    assert_eq!(detail["inference"], "ready");
    assert!(
        detail["raw_tokens"].is_number(),
        "POST-time accounting kept: {detail}"
    );
}
