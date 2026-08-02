//! End-to-end batch consolidation (CE-9b, PR B8): the round, the run ledger,
//! the watermark, and the two HTTP surfaces.
//!
//! The embedder is **absent** in every hermetic test here (`embedding.enabled
//! = false`), so the observation pool comes from the BM25 arm and the plans
//! under test carry no `creates` — an observation must be embedded
//! synchronously when it is created (CE-9a's R3), and that needs the 133MB
//! model. `creates` are covered three other ways: `store::apply_plan`'s unit
//! tests (the write), `round::validate`'s (the contract), and the
//! `#[ignore]`d `live_consolidation_round` below (the whole thing, against
//! the real Ollama and the real model).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use memgarden_core::types::FactType;
use memgarden_store::models::NewNode;
use memgarden_store::{Db, banks, consolidate as store, nodes};
use memgardend::{routes, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

const DIM: usize = memgarden_core::EMBEDDING_DIM;

struct Harness {
    app: axum::Router,
    db: Arc<Db>,
    /// What the stub `/api/chat` answers with. Set after seeding, because a
    /// realistic plan has to name the uuids the round will actually show the
    /// model.
    reply: Arc<std::sync::Mutex<String>>,
    calls: Arc<AtomicUsize>,
}

async fn harness() -> Harness {
    let reply = Arc::new(std::sync::Mutex::new(
        r#"{"creates":[],"updates":[],"deletes":[]}"#.to_string(),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let (r, c) = (reply.clone(), calls.clone());
    let stub = axum::Router::new().route(
        "/api/chat",
        axum::routing::post(move |axum::Json(_): axum::Json<Value>| {
            let (r, c) = (r.clone(), c.clone());
            async move {
                c.fetch_add(1, Ordering::SeqCst);
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
    let ollama = Arc::new(memgardend::ollama::OllamaClient::new(cfg.ollama.clone()).unwrap());
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
    std::mem::forget(retain_rx);
    let state = AppState {
        db: db.clone(),
        cfg: Arc::new(cfg),
        started_at_ms: memgarden_core::now_ms(),
        embedder: Arc::new(std::sync::RwLock::new(None)),
        ollama,
        retain_tx,
    };
    Harness {
        app: routes::router(state),
        db,
        reply,
        calls,
    }
}

impl Harness {
    fn set_reply(&self, body: Value) {
        *self.reply.lock().unwrap() = body.to_string();
    }

    fn fact(&self, text: &str) -> (i64, String) {
        let mut node = NewNode::new("b1", FactType::World, text);
        node.mentioned_at = Some(1_782_898_200_000);
        let id = nodes::insert(&self.db, node).unwrap();
        (id, nodes::get(&self.db, id).unwrap().unwrap().uuid)
    }

    fn observation(&self, text: &str, sources: &[i64]) -> (i64, String) {
        let id =
            store::insert_observation(&self.db, "b1", text, &vec![0.1f32; DIM], sources).unwrap();
        (id, nodes::get(&self.db, id).unwrap().unwrap().uuid)
    }

    async fn consolidate(&self) -> (StatusCode, Value) {
        self.send("POST", "/v1/banks/b1/consolidate").await
    }

    async fn status(&self) -> Value {
        let (status, body) = self.send("GET", "/v1/banks/b1/consolidation").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn send(&self, method: &str, uri: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:9100")
            .body(Body::empty())
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

/// The happy path the plan asks for, minus `creates`: an UPDATE folds a new
/// fact into an existing observation as evidence, a DELETE removes one the
/// facts superseded, the ledger records it, and the watermark moves.
#[tokio::test]
async fn a_round_applies_updates_and_deletes_and_records_the_run() {
    let h = harness().await;
    let (seed_fact, _) = h.fact("the retain worker commits one chunk per transaction");
    let (keep, keep_uuid) = h.observation("the retain worker commits per chunk", &[seed_fact]);
    let (doomed, doomed_uuid) = h.observation("the retain worker commits per chunk somehow", &[]);
    let (new_fact, new_uuid) = h.fact("the retain worker commits one chunk per BEGIN IMMEDIATE");

    h.set_reply(json!({
        "creates": [],
        "updates": [{
            "text": "The retain worker commits one chunk per BEGIN IMMEDIATE transaction.",
            "observation_id": keep_uuid,
            "source_fact_ids": [new_uuid],
            "reason": "Same canonical decision as the existing observation; attached as evidence.",
        }],
        "deletes": [{"observation_id": doomed_uuid, "reason": "Restated identically by the update."}],
    }));

    let (code, summary) = h.consolidate().await;

    assert_eq!(code, StatusCode::OK, "{summary}");
    assert_eq!(summary["facts_seen"], 2);
    assert_eq!(summary["updated"], 1);
    assert_eq!(summary["deleted"], 1);
    assert_eq!(summary["created"], 0);
    assert_eq!(summary["batches"], 1);
    assert_eq!(summary["skipped_batches"], 0);
    assert_eq!(summary["watermark"], new_fact);
    assert_eq!(h.calls.load(Ordering::SeqCst), 1, "one batch, one call");

    let updated = nodes::get(&h.db, keep).unwrap().unwrap();
    assert_eq!(
        updated.text,
        "The retain worker commits one chunk per BEGIN IMMEDIATE transaction."
    );
    assert_eq!(
        store::sources_of(&h.db, keep).unwrap(),
        vec![seed_fact, new_fact],
        "the new fact joined the provenance"
    );
    assert_eq!(store::proof_count(&h.db, keep).unwrap(), 2);
    assert!(nodes::get(&h.db, doomed).unwrap().is_none());
    // Only observations died.
    assert!(nodes::get(&h.db, seed_fact).unwrap().is_some());
    assert!(nodes::get(&h.db, new_fact).unwrap().is_some());

    let status = h.status().await;
    assert_eq!(status["watermark"], new_fact);
    assert_eq!(status["pending"], 0);
    assert_eq!(status["interval_secs"], 300);
    let run = &status["latest_run"];
    assert_eq!(run["status"], "done");
    assert_eq!(run["facts_seen"], 2);
    assert_eq!(run["updated"], 1);
    assert_eq!(run["deleted"], 1);
    assert_eq!(run["watermark"], new_fact);
    assert!(run["error"].is_null());
    assert!(run["finished_at"].is_i64());
}

/// The watermark's whole job: a second round over the same bank does nothing
/// at all — no LLM call, no ledger row, no write.
#[tokio::test]
async fn a_second_round_with_no_new_facts_is_a_no_op() {
    let h = harness().await;
    let (fact, _) = h.fact("the embedding backlog drains in batches of eight");

    let (_, first) = h.consolidate().await;
    assert_eq!(first["facts_seen"], 1);
    assert_eq!(first["watermark"], fact);
    assert!(first["run_id"].is_i64());
    let calls_after_first = h.calls.load(Ordering::SeqCst);
    assert_eq!(calls_after_first, 1);

    let (code, second) = h.consolidate().await;

    assert_eq!(code, StatusCode::OK);
    assert!(second["run_id"].is_null(), "a no-op writes no ledger row");
    assert_eq!(second["facts_seen"], 0);
    assert_eq!(second["batches"], 0);
    assert_eq!(second["watermark"], fact, "the mark stays where it was");
    assert_eq!(
        h.calls.load(Ordering::SeqCst),
        calls_after_first,
        "no LLM call for an empty round"
    );
    assert_eq!(h.status().await["latest_run"]["id"], first["run_id"]);

    // ...and a new fact starts it moving again.
    let (later, _) = h.fact("the metrics registry snapshots every sixty seconds");
    let (_, third) = h.consolidate().await;
    assert_eq!(third["facts_seen"], 1);
    assert_eq!(third["watermark"], later);
}

/// The plan's named guard. Two `updates` for one `observation_id` would have
/// the second write silently overwrite the first, so the **whole batch** is
/// rejected — retried `max_attempts` times, then skipped. Nothing is written,
/// and the round still finishes cleanly with the watermark advanced (a poison
/// batch retried every tick forever is the 2026-08-02 incident's shape).
#[tokio::test]
async fn duplicate_observation_ids_in_updates_reject_the_batch() {
    let h = harness().await;
    let (_, _) = h.fact("the sqlite-vec index partitions on bank_id");
    let (obs, obs_uuid) = h.observation("the sqlite-vec index partitions on bank_id", &[]);
    let (fact, fact_uuid) = h.fact("the sqlite-vec index partitions on the bank_id key only");

    h.set_reply(json!({
        "creates": [],
        "updates": [
            {"text": "first plan", "observation_id": obs_uuid, "source_fact_ids": [fact_uuid], "reason": "r"},
            {"text": "second plan", "observation_id": obs_uuid, "source_fact_ids": [fact_uuid], "reason": "r"},
        ],
        "deletes": [],
    }));

    let (code, summary) = h.consolidate().await;

    assert_eq!(code, StatusCode::OK, "{summary}");
    assert_eq!(summary["skipped_batches"], 1);
    assert_eq!(summary["batches"], 0);
    assert_eq!(summary["updated"], 0);
    assert_eq!(
        nodes::get(&h.db, obs).unwrap().unwrap().text,
        "the sqlite-vec index partitions on bank_id",
        "neither plan was applied — no silent overwrite"
    );
    assert_eq!(
        h.calls.load(Ordering::SeqCst),
        3,
        "consolidation.max_attempts fresh tries before the batch is abandoned"
    );
    // Forward progress: the run closes and the mark moves, so the next tick
    // does not re-send the identical payload forever.
    let status = h.status().await;
    assert_eq!(status["latest_run"]["status"], "done");
    assert_eq!(status["watermark"], fact);
    assert_eq!(status["pending"], 0);
}

/// The plan's other named guard: an entry citing a `source_fact_ids` uuid the
/// round never showed the model is dropped, and the rest of the plan applies.
#[tokio::test]
async fn an_unknown_source_fact_uuid_drops_the_entry_and_the_run_continues() {
    let h = harness().await;
    let (_, _) = h.fact("the FTS5 tokenizer uses a unicode61 prefix index");
    let (keep, keep_uuid) = h.observation("the FTS5 tokenizer uses unicode61", &[]);
    let (doomed, doomed_uuid) = h.observation("the FTS5 tokenizer is unicode61 based", &[]);
    let (fact, _) = h.fact("the FTS5 tokenizer indexes prefixes of length two three four");

    h.set_reply(json!({
        "creates": [],
        "updates": [{
            "text": "would have been applied",
            "observation_id": keep_uuid,
            // A uuid from nowhere: never presented, so the provenance is wrong.
            "source_fact_ids": ["cafebabe-0000-0000-0000-000000000000"],
            "reason": "r",
        }],
        "deletes": [{"observation_id": doomed_uuid, "reason": "restated identically"}],
    }));

    let (code, summary) = h.consolidate().await;

    assert_eq!(code, StatusCode::OK, "{summary}");
    assert_eq!(summary["updated"], 0, "the entry was dropped");
    assert_eq!(summary["deleted"], 1, "the run continued");
    assert_eq!(summary["batches"], 1);
    assert_eq!(
        h.calls.load(Ordering::SeqCst),
        1,
        "a dropped entry is not a rejected batch — no retry"
    );
    assert_eq!(
        nodes::get(&h.db, keep).unwrap().unwrap().text,
        "the FTS5 tokenizer uses unicode61"
    );
    assert!(nodes::get(&h.db, doomed).unwrap().is_none());
    assert_eq!(h.status().await["watermark"], fact);
}

/// A reply the model never manages to make sense of costs a batch, never a
/// fact: the round finishes, the mark moves, nothing is written.
#[tokio::test]
async fn an_unparseable_reply_skips_the_batch_without_failing_the_round() {
    let h = harness().await;
    *h.reply.lock().unwrap() = "I have decided not to answer in JSON today.".to_string();
    let (fact, _) = h.fact("the Ollama client holds exactly one interactive permit");

    let (code, summary) = h.consolidate().await;

    assert_eq!(code, StatusCode::OK, "{summary}");
    assert_eq!(summary["skipped_batches"], 1);
    assert_eq!(summary["created"], 0);
    assert_eq!(summary["watermark"], fact);
    assert_eq!(h.status().await["latest_run"]["status"], "done");
}

/// A round splits its facts into `llm_batch_size` groups and calls once per
/// group, sequentially (`llm_parallelism` is 1 here by construction).
#[tokio::test]
async fn facts_are_batched_at_llm_batch_size() {
    let h = harness().await;
    let mut last = 0;
    for i in 0..20 {
        last = h
            .fact(&format!(
                "the migration runner applied migration number {i}"
            ))
            .0;
    }

    let (_, summary) = h.consolidate().await;

    assert_eq!(summary["facts_seen"], 20);
    assert_eq!(summary["batches"], 3, "20 facts at llm_batch_size 8");
    assert_eq!(h.calls.load(Ordering::SeqCst), 3);
    assert_eq!(summary["watermark"], last);
}

#[tokio::test]
async fn both_endpoints_404_on_an_unknown_bank() {
    let h = harness().await;
    assert_eq!(
        h.send("POST", "/v1/banks/nope/consolidate").await.0,
        StatusCode::NOT_FOUND
    );
    let (code, body) = h.send("GET", "/v1/banks/nope/consolidation").await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

/// Status on a bank that has never consolidated: real numbers, no run.
#[tokio::test]
async fn status_reports_pending_work_before_the_first_run() {
    let h = harness().await;
    h.fact("the benefit ledger records saved tokens per retain");

    let status = h.status().await;

    assert_eq!(status["watermark"], 0);
    assert_eq!(status["pending"], 1);
    assert!(status["latest_run"].is_null());
}

/// CE-9a's correctness debt #10, and the half B7 could not test: only B8 has
/// a worker running. `update_text` nulls the embedding and re-queues the node
/// (asserted in B7); one `embed_task` tick must put the vector **back**.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "loads the real embedding model"]
async fn an_embed_task_tick_regenerates_an_invalidated_embedding() {
    let cfg_defaults = memgarden_core::config::Config::defaults().unwrap();
    let db = Arc::new(Db::open_memory().unwrap());
    banks::create(&db, "b1", None, None).unwrap();

    let mut cfg = cfg_defaults.clone();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    let embedder = {
        let c = cfg_defaults.embedding.clone();
        Arc::new(
            tokio::task::spawn_blocking(move || memgardend::embed::Embedder::load(&c))
                .await
                .unwrap()
                .expect("embedding model must be cached (run CE-4's model_smoke first)"),
        )
    };
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(4);
    std::mem::forget(retain_rx);
    let state = AppState {
        db: db.clone(),
        cfg: Arc::new(cfg),
        started_at_ms: memgarden_core::now_ms(),
        embedder: Arc::new(std::sync::RwLock::new(Some(embedder))),
        ollama: Arc::new(
            memgardend::ollama::OllamaClient::new(cfg_defaults.ollama.clone()).unwrap(),
        ),
        retain_tx,
    };

    let id = store::insert_observation(&db, "b1", "before", &vec![0.1f32; DIM], &[]).unwrap();
    nodes::update_text(&db, id, "the recall pipeline fuses four arms with RRF").unwrap();
    assert!(
        nodes::get(&db, id).unwrap().unwrap().embedding.is_none(),
        "B7's half: the stale vector is gone"
    );

    memgardend::embed_task::drain_once(&db, &state).await;

    let node = nodes::get(&db, id).unwrap().unwrap();
    let blob = node.embedding.expect("one tick must regenerate the vector");
    assert_eq!(blob.len(), DIM * 4, "a full f32 vector, not a stub");
    assert!(blob.iter().any(|b| *b != 0), "a real vector, not zeros");
    assert!(
        nodes::pending_embeddings(&db, 10)
            .unwrap()
            .iter()
            .all(|(pending, ..)| *pending != id),
        "and the node is off the backlog"
    );
}

/// The plan's manual verification, automated: ~50 facts through a real round
/// against the real Ollama and the real embedder. Run:
/// `cargo test --release -p memgardend --test consolidate_api -- --ignored --nocapture live_consolidation_round`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a running Ollama with the configured model and the embedding model"]
async fn live_consolidation_round() {
    let cfg_defaults = memgarden_core::config::Config::defaults().unwrap();
    let db = Arc::new(Db::open_memory().unwrap());
    banks::create(&db, "b1", None, None).unwrap();

    let embedder = {
        let c = cfg_defaults.embedding.clone();
        Arc::new(
            tokio::task::spawn_blocking(move || memgardend::embed::Embedder::load(&c))
                .await
                .unwrap()
                .expect("embedding model"),
        )
    };
    let mut cfg = cfg_defaults.clone();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(4);
    std::mem::forget(retain_rx);
    let state = AppState {
        db: db.clone(),
        cfg: Arc::new(cfg),
        started_at_ms: memgarden_core::now_ms(),
        embedder: Arc::new(std::sync::RwLock::new(Some(embedder))),
        ollama: Arc::new(memgardend::ollama::OllamaClient::new(cfg_defaults.ollama).unwrap()),
        retain_tx,
    };

    // ~50 facts, with deliberate near-duplicates so UPDATE and dedup both
    // have something to do.
    let subjects = [
        "the retain worker",
        "the embedding backlog",
        "the recall pipeline",
        "the sqlite-vec index",
        "the Ollama client",
    ];
    let claims = [
        "commits one chunk per BEGIN IMMEDIATE transaction",
        "drains in batches of eight to cap the ONNX mutex hold",
        "fuses four retrieval arms with reciprocal rank fusion",
        "partitions only on bank_id, so fact_type is filtered in Rust",
        "holds exactly one concurrency permit for the local 14B model",
    ];
    for i in 0..50usize {
        let text = format!(
            "{} {} (observed on run {})",
            subjects[i % subjects.len()],
            claims[(i / 5) % claims.len()],
            i
        );
        let mut node = NewNode::new("b1", FactType::World, &text);
        node.mentioned_at = Some(memgarden_core::now_ms());
        nodes::insert(&db, node).unwrap();
    }

    let started = std::time::Instant::now();
    let summary = memgardend::consolidate::round::run_round(&state, "b1")
        .await
        .expect("live round");
    println!(
        "live_consolidation_round: {summary:?}\n  wall: {:.1}s",
        started.elapsed().as_secs_f64()
    );

    let conn = db.read().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, text, proof_count FROM memory_nodes
             WHERE bank_id = 'b1' AND fact_type = 'observation' ORDER BY id",
        )
        .unwrap();
    let rows: Vec<(i64, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for (id, text, proof) in &rows {
        println!("  obs {id} (proof_count {proof}): {text}");
    }
    let run: (String, i64, i64, i64, i64, i64, Option<i64>) = conn
        .query_row(
            "SELECT status, facts_seen, created_n, updated_n, deleted_n, merged_n, watermark
             FROM consolidation_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();
    println!("  run row: {run:?}");

    assert_eq!(run.0, "done");
    assert_eq!(run.1, 50);
    assert!(!rows.is_empty(), "50 facts must produce observations");
}
