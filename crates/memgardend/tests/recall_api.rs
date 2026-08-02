//! End-to-end hybrid recall (CE-6, PR B4): the HTTP contract, the Korean
//! guard, filtering, the token budget and the injected block.
//!
//! Most tests run with the embedder **absent** (`embedding.enabled = false`),
//! i.e. BM25-only. That is deliberate and not a gap: the embedder is a 133MB
//! download, and the arm that actually carries Korean is FTS (Phase A
//! decision #7). The hybrid path — both arms live — is covered by the
//! `#[ignore]`d `hybrid_recall_bench`, which loads the real model.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use memgarden_core::types::FactType;
use memgarden_store::models::NewNode;
use memgarden_store::{Db, banks, nodes};
use memgardend::{routes, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

type EmbedderSlot = Arc<std::sync::RwLock<Option<Arc<memgardend::embed::Embedder>>>>;

fn test_app(tweak: impl FnOnce(&mut memgarden_core::config::Config)) -> (axum::Router, Arc<Db>) {
    let (app, db, _) = test_app_parts(tweak);
    (app, db)
}

/// Adds the embedder slot the router holds — writing an `Embedder` into it
/// turns the semantic arm on for an already-built app (the bench).
fn test_app_parts(
    tweak: impl FnOnce(&mut memgarden_core::config::Config),
) -> (axum::Router, Arc<Db>, EmbedderSlot) {
    test_app_on(Arc::new(Db::open_memory().unwrap()), tweak)
}

fn test_app_on(
    db: Arc<Db>,
    tweak: impl FnOnce(&mut memgarden_core::config::Config),
) -> (axum::Router, Arc<Db>, EmbedderSlot) {
    let mut cfg = memgarden_core::config::Config::defaults().unwrap();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    cfg.embedding.enabled = false;
    cfg.ollama.base_url = "http://127.0.0.1:1".to_string();
    cfg.ollama.request_timeout_secs = 1;
    cfg.ollama.max_retries = 0;
    tweak(&mut cfg);

    let ollama = Arc::new(memgardend::ollama::OllamaClient::new(cfg.ollama.clone()).unwrap());
    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
    std::mem::forget(retain_rx);
    let embedder: EmbedderSlot = Arc::new(std::sync::RwLock::new(None));
    let state = AppState {
        db: db.clone(),
        cfg: Arc::new(cfg),
        started_at_ms: memgarden_core::now_ms(),
        embedder: embedder.clone(),
        ollama,
        retain_tx,
    };
    (routes::router(state), db, embedder)
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "127.0.0.1:9100")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn recall(app: &axum::Router, body: Value) -> Value {
    let (status, value) = post(app, "/v1/banks/b1/recall", body).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

fn texts(v: &Value) -> Vec<String> {
    v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].as_str().unwrap().to_string())
        .collect()
}

fn seed(db: &Db, fact_type: FactType, text: &str, tags: &[&str]) -> i64 {
    let mut node = NewNode::new("b1", fact_type, text);
    node.mentioned_at = Some(1_782_898_200_000); // 2026-07-01T09:30:00Z
    let id = nodes::insert(db, node).unwrap();
    if !tags.is_empty() {
        nodes::add_tags(db, id, tags).unwrap();
    }
    id
}

// ---------------------------------------------------------------------------
// The Korean guard — the #1 named risk of this PR
// ---------------------------------------------------------------------------

/// A raw Korean query must reach results through the endpoint. This is the
/// end-to-end half of the Phase A guard (`fts_query_string` + `prefix='2 3 4'`);
/// an English-only recall that silently degrades Korean fails here.
#[tokio::test]
async fn korean_query_recalls_end_to_end() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();
    seed(
        &db,
        FactType::World,
        "메모리 회수 파이프라인은 BM25와 벡터 후보를 융합한다",
        &[],
    );
    seed(
        &db,
        FactType::World,
        "an entirely unrelated english fact",
        &[],
    );

    // Single token — the plain prefix case.
    let out = recall(&app, json!({ "query": "파이프라인" })).await;
    assert_eq!(out["counts"]["returned"], 1, "{out}");
    assert!(texts(&out)[0].contains("메모리 회수"));

    // Five tokens — 0 hits before Critic Revision R1 changed the join to OR.
    let out = recall(
        &app,
        json!({ "query": "메모리 회수 파이프라인 하이브리드 검색" }),
    )
    .await;
    assert_eq!(out["counts"]["returned"], 1, "{out}");
}

/// The English mirror of the above at >12 tokens, so the term cap is on the
/// path too (Critic Revision R1's second requirement).
#[tokio::test]
async fn long_english_query_recalls_end_to_end() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();
    seed(
        &db,
        FactType::World,
        "the recall pipeline fuses BM25 and vector candidates with reciprocal rank fusion",
        &[],
    );

    let query = "remind me how does the hybrid recall pipeline actually combine \
                 keyword search with vector similarity scores";
    assert!(query.split_whitespace().count() > 12);
    let out = recall(&app, json!({ "query": query })).await;
    assert_eq!(out["counts"]["returned"], 1, "{out}");
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recall_types_filters_and_defaults_to_all_three() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();
    seed(&db, FactType::World, "deployment checklist", &[]);
    seed(&db, FactType::Observation, "deployment checklist", &[]);
    seed(&db, FactType::Experience, "deployment checklist", &[]);

    // Server default is all three — the fork improvement over legacy's
    // observation-only client default.
    let out = recall(&app, json!({ "query": "deployment" })).await;
    assert_eq!(out["counts"]["returned"], 3, "{out}");

    let out = recall(
        &app,
        json!({ "query": "deployment", "recallTypes": ["world"] }),
    )
    .await;
    assert_eq!(out["counts"]["returned"], 1);
    assert_eq!(out["results"][0]["type"], "world");

    let out = recall(
        &app,
        json!({ "query": "deployment", "recallTypes": ["world", "experience"] }),
    )
    .await;
    assert_eq!(out["counts"]["returned"], 2);

    let (status, body) = post(
        &app,
        "/v1/banks/b1/recall",
        json!({ "query": "deployment", "recallTypes": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid");

    let (status, body) = post(
        &app,
        "/v1/banks/b1/recall",
        json!({ "query": "deployment", "recallTypes": ["nonsense"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid");
}

#[tokio::test]
async fn tag_matching_modes() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();
    seed(&db, FactType::World, "tagged both alpha", &["a", "b"]);
    seed(&db, FactType::World, "tagged only alpha", &["a"]);
    seed(&db, FactType::World, "untagged alpha", &[]);

    let ask = |tags: Value, mode: &str| {
        let app = app.clone();
        let mode = mode.to_string();
        async move {
            let out = recall(
                &app,
                json!({ "query": "alpha", "tags": tags, "tagsMatch": mode }),
            )
            .await;
            let mut t = texts(&out);
            t.sort();
            t
        }
    };

    // No tags -> no filtering, in any mode.
    assert_eq!(ask(json!([]), "all_strict").await.len(), 3);

    // any: overlap OR untagged.
    assert_eq!(
        ask(json!(["b"]), "any").await,
        vec!["tagged both alpha", "untagged alpha"]
    );
    // any_strict: overlap only.
    assert_eq!(
        ask(json!(["b"]), "any_strict").await,
        vec!["tagged both alpha"]
    );
    // all: every requested tag present, untagged still allowed.
    assert_eq!(
        ask(json!(["a", "b"]), "all").await,
        vec!["tagged both alpha", "untagged alpha"]
    );
    // all_strict: every requested tag present, untagged excluded.
    assert_eq!(
        ask(json!(["a", "b"]), "all_strict").await,
        vec!["tagged both alpha"]
    );
    assert_eq!(
        ask(json!(["a"]), "all_strict").await,
        vec!["tagged both alpha", "tagged only alpha"]
    );
}

// ---------------------------------------------------------------------------
// Short / empty queries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn short_queries_short_circuit_without_erroring() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();
    seed(&db, FactType::World, "abcde is a short token", &[]);

    // 4 characters: skipped (recall.py:128). Not an error — the Phase C hook
    // fires on every prompt.
    let out = recall(&app, json!({ "query": "abcd" })).await;
    assert_eq!(out["counts"]["returned"], 0);
    assert_eq!(out["counts"]["candidates"], 0);
    assert_eq!(out["injected_text"], "");

    let out = recall(&app, json!({ "query": "" })).await;
    assert_eq!(out["counts"]["returned"], 0);

    // 5 characters: runs.
    let out = recall(&app, json!({ "query": "abcde" })).await;
    assert_eq!(out["counts"]["returned"], 1, "{out}");
}

#[tokio::test]
async fn punctuation_only_query_is_not_an_fts_syntax_error() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();
    seed(&db, FactType::World, "anything", &[]);
    // Tokenizes to an empty MATCH expression; `MATCH ''` would be a SQLite
    // syntax error, so this asserts the short-circuit is on the recall path.
    let out = recall(&app, json!({ "query": "?!?!?!" })).await;
    assert_eq!(out["counts"]["returned"], 0);
}

// ---------------------------------------------------------------------------
// Budget + injection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn token_budget_truncates_at_the_boundary() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();
    // Six equal-length facts, so which order BM25 returns them in cannot
    // change where the greedy budget cuts.
    let mut per_fact = 0u64;
    for i in 0..6 {
        let text = format!("budgetword {} {i}", "filler ".repeat(20));
        per_fact = memgardend::retain::token_count(&text);
        seed(&db, FactType::World, &text, &[]);
    }
    // The exact boundary the greedy filter must land on: it stops *before*
    // the first fact that would push the running total over 100.
    let expected_low = (100 / per_fact) as usize;
    assert!(
        (1..6).contains(&expected_low),
        "fixture must straddle the low budget: {per_fact} tokens/fact"
    );

    let low = recall(&app, json!({ "query": "budgetword", "budget": "low" })).await;
    let high = recall(&app, json!({ "query": "budgetword", "budget": "high" })).await;

    let low_n = low["counts"]["returned"].as_u64().unwrap() as usize;
    let high_n = high["counts"]["returned"].as_u64().unwrap();
    assert_eq!(
        low_n, expected_low,
        "low budget cut in the wrong place: {low}"
    );
    assert_eq!(
        low["counts"]["tokens"].as_u64().unwrap(),
        per_fact * expected_low as u64
    );
    assert!(
        low["counts"]["tokens"].as_u64().unwrap() <= 100,
        "budget overrun: {}",
        low["counts"]["tokens"]
    );
    assert_eq!(high_n, 6, "high budget fits everything: {high}");
    // Candidates counts what entered fusion, before the budget cut.
    assert_eq!(low["counts"]["candidates"], 6);

    let (status, body) = post(
        &app,
        "/v1/banks/b1/recall",
        json!({ "query": "budgetword", "budget": "enormous" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid");
}

#[tokio::test]
async fn limit_caps_results_below_the_budget() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();
    for i in 0..5 {
        seed(&db, FactType::World, &format!("limitword number {i}"), &[]);
    }
    let out = recall(&app, json!({ "query": "limitword", "limit": 2 })).await;
    assert_eq!(out["counts"]["returned"], 2);
    assert_eq!(out["results"].as_array().unwrap().len(), 2);
    // Tokens are recounted for what actually came back, not what the budget
    // would have allowed.
    assert!(out["counts"]["tokens"].as_u64().unwrap() > 0);
    // "\n- " and not "- ": the "Current time - ..." line also contains "- ".
    assert_eq!(
        out["injected_text"]
            .as_str()
            .unwrap()
            .matches("\n- ")
            .count(),
        2
    );
}

#[tokio::test]
async fn injected_text_carries_the_scores_and_the_block() {
    let (app, db) = test_app(|c| c.recall.preamble = "Relevant memories:".to_string());
    banks::create(&db, "b1", None, None).unwrap();
    seed(
        &db,
        FactType::Observation,
        "the daemon binds 127.0.0.1:9100",
        &[],
    );

    let out = recall(&app, json!({ "query": "daemon binds" })).await;
    let injected = out["injected_text"].as_str().unwrap();
    assert!(injected.starts_with("<memgarden_memories>\nRelevant memories:\nCurrent time - "));
    assert!(injected.ends_with("</memgarden_memories>"));
    assert!(
        injected.contains("- the daemon binds 127.0.0.1:9100 [observation] (2026-07-01 09:30 UTC)")
    );

    let scores = &out["results"][0]["scores"];
    // n = 1 -> the passthrough denominator guard gives base 1.0; recency is
    // real; temporal/proof ship as 0.5 stubs (Critic Revision R12).
    assert!(scores["final"].as_f64().unwrap() > 0.0);
    assert!(
        scores["keyword"].as_f64().unwrap() < 0.0,
        "raw bm25() is negative"
    );
    assert!(
        scores["semantic"].is_null(),
        "no embedder -> no semantic arm"
    );
    assert!(scores["rrf"].as_f64().unwrap() > 0.0);
    assert_eq!(scores["temporal"], 0.5);
    assert_eq!(scores["proof"], 0.5);

    // Per-request preamble override.
    let out = recall(&app, json!({ "query": "daemon binds", "preamble": "MEM:" })).await;
    assert!(out["injected_text"].as_str().unwrap().contains("\nMEM:\n"));
}

// ---------------------------------------------------------------------------
// Contract edges + metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_bank_is_404_and_bad_input_is_400() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();

    let (status, body) = post(&app, "/v1/banks/nope/recall", json!({ "query": "hello" })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");

    // Missing the required `query` field: a 400 in the JSON envelope, not
    // axum's plain-text 422 (PR #8 review LOW).
    let (status, body) = post(&app, "/v1/banks/b1/recall", json!({ "limit": 3 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("query"),
        "{body}"
    );

    let (status, _) = post(
        &app,
        "/v1/banks/b1/recall",
        json!({ "query": "x".repeat(9000) }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let tags: Vec<String> = (0..40).map(|i| format!("t{i}")).collect();
    let (status, _) = post(
        &app,
        "/v1/banks/b1/recall",
        json!({ "query": "hello there", "tags": tags }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn recall_moves_its_metrics() {
    let (app, db) = test_app(|_| {});
    banks::create(&db, "b1", None, None).unwrap();
    seed(&db, FactType::World, "metricsword one", &[]);

    let before = memgarden_core::metrics::METRICS.snapshot();
    recall(&app, json!({ "query": "metricsword" })).await;
    let (status, _) = post(
        &app,
        "/v1/banks/nope/recall",
        json!({ "query": "metricsword" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let after = memgarden_core::metrics::METRICS.snapshot();

    // The global static is shared with the other tests in this binary, so
    // only deltas are asserted, never absolute totals.
    assert!(after.recall_requests >= before.recall_requests + 2);
    assert!(after.recall_errors > before.recall_errors);
    assert!(after.recall_injected_memories > before.recall_injected_memories);
    assert!(after.recall_injected_tokens > before.recall_injected_tokens);
    assert!(after.recall_latency.is_some());
}

// ---------------------------------------------------------------------------
// AC-2 measurement: recall p50 <= 35ms / p95 <= 60ms on a realistic bank
// ---------------------------------------------------------------------------

/// Loads the real embedder and drives N recalls against a seeded bank,
/// reporting the daemon's own `recall_latency` histogram. Both arms are live
/// here — the hermetic tests above are BM25-only.
///
/// Requires the 133MB model (already cached after CE-4):
///   cargo test --release -p memgardend --test recall_api -- --ignored --nocapture hybrid_recall_bench
///
/// `NODES` / `REQUESTS` are overridable so the same test can be run against
/// a bigger bank without a recompile:
///   MEMGARDEN_BENCH_NODES=3000 MEMGARDEN_BENCH_REQUESTS=2000 cargo test ...
#[tokio::test(flavor = "multi_thread")]
#[ignore = "loads the real embedding model and takes ~1 minute"]
async fn hybrid_recall_bench() {
    use std::time::Instant;

    fn env_usize(key: &str, default: usize) -> usize {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    let nodes_n = env_usize("MEMGARDEN_BENCH_NODES", 2500);
    let requests = env_usize("MEMGARDEN_BENCH_REQUESTS", 200);

    let cfg_defaults = memgarden_core::config::Config::defaults().unwrap();
    // File-backed, unlike every other test here: production runs on a file
    // and therefore in WAL, where a reader never blocks on a writer. The
    // hermetic tests' shared-cache `:memory:` pool has no WAL, so the
    // `MEMGARDEN_BENCH_LOAD` run below would measure (and trip over) a
    // table-lock contention that cannot happen in production.
    let dir = tempfile::tempdir().unwrap();
    let (app, db, embedder_slot) = test_app_on(
        Arc::new(Db::open(dir.path().join("bench.db")).unwrap()),
        |c| c.embedding = cfg_defaults.embedding.clone(),
    );
    banks::create(&db, "b1", None, None).unwrap();

    // Seed a bank of realistic engineering facts. Text is varied enough that
    // BM25 has real work to do; every node gets an embedding so the vector
    // arm is scanning the full partition.
    let subjects = [
        "the retain worker",
        "the embedding backlog",
        "the recall pipeline",
        "the sqlite-vec index",
        "the FTS5 tokenizer",
        "the Ollama client",
        "메모리 회수 파이프라인",
        "the metrics registry",
        "the benefit ledger",
        "the migration runner",
    ];
    let verbs = [
        "was changed to",
        "must never",
        "now defaults to",
        "regressed after",
        "is bounded by",
        "was measured at",
        "conflicts with",
        "depends on",
    ];
    let objects = [
        "a single BEGIN IMMEDIATE transaction",
        "hold the write lock across an await",
        "batch size 8 to cap the mutex hold",
        "the 0002 migration landed",
        "the per-job wall clock of 7200 seconds",
        "3.3 milliseconds on CPU",
        "the interactive Ollama permit",
        "the cl100k token counter",
        "the unicode61 prefix index",
        "the bank_id partition key",
    ];

    let embedder = {
        let cfg = cfg_defaults.embedding.clone();
        Arc::new(
            tokio::task::spawn_blocking(move || memgardend::embed::Embedder::load(&cfg))
                .await
                .unwrap()
                .expect("embedding model must be cached (run CE-4's model_smoke first)"),
        )
    };

    let mut texts = Vec::with_capacity(nodes_n);
    for i in 0..nodes_n {
        texts.push(format!(
            "{} {} {} (case {i})",
            subjects[i % subjects.len()],
            verbs[(i / 7) % verbs.len()],
            objects[(i / 3) % objects.len()],
        ));
    }
    let seed_start = Instant::now();
    let pairs: Vec<(i64, String)> = texts
        .iter()
        .map(|text| {
            (
                seed(&db, FactType::World, text, &["session:bench"]),
                text.clone(),
            )
        })
        .collect();
    for chunk in pairs.chunks(64) {
        let batch_texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
        let e = embedder.clone();
        let vectors = tokio::task::spawn_blocking(move || e.embed_batch(&batch_texts))
            .await
            .unwrap()
            .unwrap();
        let batch: Vec<(i64, String, Vec<f32>)> = chunk
            .iter()
            .zip(vectors)
            .map(|((id, _), v)| (*id, "b1".to_string(), v))
            .collect();
        nodes::set_embeddings_batch(&db, &batch).unwrap();
    }
    println!(
        "bench: seeded {nodes_n} nodes (+embeddings) in {:.1}s",
        seed_start.elapsed().as_secs_f64()
    );

    // Turn the semantic arm on: the router holds a clone of this very Arc.
    *embedder_slot.write().unwrap() = Some(embedder.clone());

    let queries = [
        "why did the retain worker change to a single transaction",
        "메모리 회수 파이프라인 하이브리드 검색 지연 시간",
        "what bounds the embedding backlog batch size and the mutex hold",
        "sqlite-vec bank_id partition key",
        "how long is the per job wall clock for a retain job again",
    ];

    // Critic Revision R7 wants the loaded number too, not just idle.
    // `MEMGARDEN_BENCH_LOAD=1` runs a background ingest against the same DB
    // for the duration: it contends on exactly what a live retain contends
    // on — the single ONNX mutex (R9) and the SQLite write lock — without
    // needing a real Ollama in the loop.
    let loaded = std::env::var("MEMGARDEN_BENCH_LOAD").is_ok_and(|v| v == "1");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load_task = loaded.then(|| {
        let (db, e, stop) = (db.clone(), embedder.clone(), stop.clone());
        tokio::task::spawn_blocking(move || {
            let mut n = 0u64;
            // The deadline is a safety net, not the stop condition: if the
            // recall loop panics it never sets `stop`, and an unbounded
            // blocking loop would hang the whole test binary instead of
            // reporting the failure.
            let deadline = Instant::now() + std::time::Duration::from_secs(600);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) && Instant::now() < deadline {
                let batch_texts: Vec<String> = (0..8)
                    .map(|i| format!("background ingest fact {n}-{i} touching the write lock"))
                    .collect();
                let ids: Vec<i64> = batch_texts
                    .iter()
                    .map(|t| {
                        nodes::insert(&db, NewNode::new("b1", FactType::Observation, t)).unwrap()
                    })
                    .collect();
                let vectors = e.embed_batch(&batch_texts).unwrap();
                let batch: Vec<(i64, String, Vec<f32>)> = ids
                    .into_iter()
                    .zip(vectors)
                    .map(|(id, v)| (id, "b1".to_string(), v))
                    .collect();
                nodes::set_embeddings_batch(&db, &batch).unwrap();
                n += 1;
            }
            n * 8
        })
    });

    let before = memgarden_core::metrics::METRICS.snapshot();
    let wall = Instant::now();
    for i in 0..requests {
        let out = recall(
            &app,
            json!({ "query": queries[i % queries.len()], "budget": "mid" }),
        )
        .await;
        assert!(out["counts"]["returned"].as_u64().unwrap() > 0, "{out}");
    }
    let wall = wall.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(task) = load_task {
        println!(
            "bench: LOADED run — {} background nodes written+embedded during the loop",
            task.await.unwrap()
        );
    }
    let after = memgarden_core::metrics::METRICS.snapshot();
    let h = after.recall_latency.unwrap();

    println!(
        "bench: nodes={nodes_n} requests={requests} wall={:.1}s\n\
         bench: recall_latency p50={:.0}us p90={:.0}us p95={:.0}us p99={:.0}us max={}us mean={:.0}us\n\
         bench: under_35ms={} under_60ms={} (histogram count {}, delta {})",
        wall.as_secs_f64(),
        h.p50_us,
        h.p90_us,
        h.p95_us,
        h.p99_us,
        h.max_us,
        h.mean_us,
        h.under_35ms,
        h.under_60ms,
        h.count,
        h.count - before.recall_latency.map(|b| b.count).unwrap_or(0),
    );

    assert!(h.p50_us <= 35_000.0, "AC-2: p50 {}us > 35ms", h.p50_us);
    assert!(h.p95_us <= 60_000.0, "AC-2: p95 {}us > 60ms", h.p95_us);
}
