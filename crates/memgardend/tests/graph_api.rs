//! End-to-end entity graph (CE-7, PR B5): retain writes entities/links, the
//! graph recall arm expands off them, and `GET /v1/banks/{id}/graph` serves
//! the viewer.
//!
//! Extraction runs against a **stub Ollama** (same shape as `retain_api.rs`)
//! that emits `entities` and `causal_relations`, which the CE-5a stub does
//! not. Embeddings are off throughout, so the semantic-link pass — which is
//! driven by the backlog worker, not retain (Critic Revision R2) — is
//! exercised by `links::semantic_links`' unit tests rather than here.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use memgarden_core::config::Config;
use memgarden_core::types::FactType;
use memgarden_store::graph::NewLink;
use memgarden_store::models::NewNode;
use memgarden_store::{Db, banks, graph, nodes};
use memgardend::{retain, routes, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Two same-type facts with overlapping entities and a causal relation: the
/// smallest input that produces every write-time link type at once.
async fn stub_chat() -> axum::response::Response {
    use axum::response::IntoResponse;
    axum::Json(json!({
        "response": json!({
            "facts": [
                { "what": "the daemon lost its ollama connection",
                  "fact_type": "world", "fact_kind": "conversation",
                  "entities": ["Ollama", "메모리 시스템"] },
                { "what": "recall fell back to keyword only",
                  "fact_type": "world", "fact_kind": "conversation",
                  "entities": ["ollama"],
                  "causal_relations": [{ "target_index": 0, "relation_type": "caused_by" }] },
            ]
        }).to_string()
    }))
    .into_response()
}

async fn spawn_stub_ollama() -> String {
    let app = axum::Router::new().route("/api/generate", axum::routing::post(stub_chat));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn build(
    ollama_url: &str,
) -> (
    axum::Router,
    Arc<Db>,
    AppState,
    tokio::sync::mpsc::Receiver<retain::RetainTask>,
) {
    let db = Arc::new(Db::open_memory().unwrap());
    let mut cfg = Config::defaults().unwrap();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg.db_path = std::path::PathBuf::from(":memory:");
    cfg.embedding.enabled = false;
    cfg.ollama.base_url = ollama_url.to_string();
    cfg.ollama.request_timeout_secs = 5;
    cfg.ollama.max_retries = 0;

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
    (routes::router(state.clone()), db, state, retain_rx)
}

/// Router only — no retain worker, for the read-side tests.
fn read_only_app() -> (axum::Router, Arc<Db>) {
    let (app, db, _state, rx) = build("http://127.0.0.1:1");
    std::mem::forget(rx);
    (app, db)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", "127.0.0.1:9100")
        .body(Body::empty())
        .unwrap()
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "127.0.0.1:9100")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(app, req).await
}

fn count(db: &Db, sql: &str) -> i64 {
    db.read().unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
}

async fn await_job(db: &Db, job_id: &str) -> memgarden_store::retain_jobs::RetainJob {
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    loop {
        let job = memgarden_store::retain_jobs::get(db, job_id)
            .unwrap()
            .unwrap();
        if !matches!(job.status.as_str(), "pending" | "running") {
            return job;
        }
        assert!(std::time::Instant::now() < deadline, "job never finished");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Retain -> graph
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retain_writes_entities_cooccurrences_and_links_but_never_an_entity_row() {
    let url = spawn_stub_ollama().await;
    let (app, db, state, rx) = build(&url);
    tokio::spawn(retain::run_worker(state, rx));

    let (status, _) = post(&app, "/v1/banks", json!({ "bank_id": "b1" })).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = post(
        &app,
        "/v1/banks/b1/retain",
        json!({
            "sessionId": "sess-1",
            "is_initial": false,
            "messages": [
                { "role": "user", "content": "why did recall degrade this afternoon?" },
                { "role": "assistant", "content": "Ollama went away, so recall fell back to BM25." },
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let job = await_job(&db, body["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status.as_str(), "done", "{:?}", job.error);
    assert_eq!(job.facts_written, 2);

    // Entities: three distinct mentions collapse to two canonical names
    // ("Ollama" and "ollama" normalize to the same thing).
    assert_eq!(count(&db, "SELECT count(*) FROM entities"), 2);
    let (name, mentions): (String, i64) = db
        .read()
        .unwrap()
        .query_row(
            "SELECT canonical_name, mention_count FROM entities WHERE canonical_name LIKE 'ollama'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(name, "ollama", "canonical names are lowercased");
    assert_eq!(mentions, 2, "one mention per fact");
    // The Korean entity survives normalization untouched.
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM entities WHERE canonical_name = '메모리 시스템'"
        ),
        1
    );

    // node_entities: fact 0 has two entities, fact 1 has one.
    assert_eq!(count(&db, "SELECT count(*) FROM node_entities"), 3);
    // One co-occurrence pair, from the fact naming both entities.
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM entity_cooccurrences WHERE entity_id_1 < entity_id_2"
        ),
        1
    );

    // Links: one caused_by (fact 1 -> fact 0) and two temporal (the two
    // same-type facts are 10ms apart, bidirectional within the batch).
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM links WHERE link_type = 'caused_by'"
        ),
        1
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM links WHERE link_type = 'temporal'"
        ),
        2
    );
    let weight: f64 = db
        .read()
        .unwrap()
        .query_row(
            "SELECT weight FROM links WHERE link_type = 'caused_by'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(weight, 1.0);

    // The whole point of brief §9: retain never writes an 'entity' link row.
    assert_eq!(
        count(&db, "SELECT count(*) FROM links WHERE link_type = 'entity'"),
        0,
        "entity grounding lives in node_entities, not in a link row"
    );
    // And no self-links of any type.
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM links WHERE from_node_id = to_node_id"
        ),
        0
    );
}

// ---------------------------------------------------------------------------
// GET /v1/banks/{id}/graph
// ---------------------------------------------------------------------------

/// Two linked world facts, one observation, all tagged with a session.
fn seed_graph(db: &Db) -> Vec<i64> {
    banks::create(db, "b1", None, None).unwrap();
    let ids: Vec<i64> = [
        (FactType::World, "the daemon binds 127.0.0.1:9100"),
        (FactType::World, "recall fuses two arms"),
        (FactType::Observation, "the user prefers Korean prompts"),
    ]
    .iter()
    .map(|(ft, text)| {
        let mut node = NewNode::new("b1", *ft, text);
        node.mentioned_at = Some(1_782_898_200_000);
        node.event_date = Some(1_782_898_200_000);
        nodes::insert(db, node).unwrap()
    })
    .collect();
    nodes::add_tags(db, ids[0], &["session:sess-a"]).unwrap();
    nodes::add_tags(db, ids[1], &["session:sess-b"]).unwrap();
    graph::insert_links(
        db,
        &[NewLink {
            from_node_id: ids[0],
            to_node_id: ids[1],
            link_type: "semantic",
            weight: 0.85,
        }],
        0,
    )
    .unwrap();
    graph::write_entities(
        db,
        "b1",
        &[(ids[0], vec!["메모리 시스템".to_string()], 0)],
        0,
    )
    .unwrap();
    ids
}

#[tokio::test]
async fn graph_endpoint_returns_nodes_links_and_entities() {
    let (app, db) = read_only_app();
    let ids = seed_graph(&db);

    let (status, body) = send(&app, get("/v1/banks/b1/graph")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nodes"].as_array().unwrap().len(), 3);

    let links = body["links"].as_array().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["from"], ids[0]);
    assert_eq!(links[0]["to"], ids[1]);
    assert_eq!(links[0]["type"], "semantic");
    assert_eq!(links[0]["weight"], 0.85);

    let first = body["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == ids[0])
        .unwrap();
    assert_eq!(first["entities"], json!(["메모리 시스템"]));
    assert_eq!(first["type"], "world");
    assert!(first["uuid"].as_str().unwrap().len() > 10);
}

#[tokio::test]
async fn graph_endpoint_filters_by_type_session_and_limit() {
    let (app, db) = read_only_app();
    let ids = seed_graph(&db);

    let (_, body) = send(&app, get("/v1/banks/b1/graph?types=observation")).await;
    let nodes_out = body["nodes"].as_array().unwrap();
    assert_eq!(nodes_out.len(), 1);
    assert_eq!(nodes_out[0]["id"], ids[2]);

    let (_, body) = send(&app, get("/v1/banks/b1/graph?types=world,observation")).await;
    assert_eq!(body["nodes"].as_array().unwrap().len(), 3);

    // Critic Revision R15: the session filter reads B3's `session:{id}` tag.
    let (_, body) = send(&app, get("/v1/banks/b1/graph?session=sess-a")).await;
    let nodes_out = body["nodes"].as_array().unwrap();
    assert_eq!(nodes_out.len(), 1);
    assert_eq!(nodes_out[0]["id"], ids[0]);
    assert!(
        body["links"].as_array().unwrap().is_empty(),
        "the link's other endpoint is filtered out, so the edge must not dangle"
    );

    let (_, body) = send(&app, get("/v1/banks/b1/graph?limit=2")).await;
    assert_eq!(body["nodes"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn graph_endpoint_rejects_bad_input_and_unknown_banks() {
    let (app, db) = read_only_app();
    seed_graph(&db);

    let (status, _) = send(&app, get("/v1/banks/nope/graph")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for uri in [
        "/v1/banks/b1/graph?limit=0",
        "/v1/banks/b1/graph?limit=5000",
        "/v1/banks/b1/graph?types=bogus",
    ] {
        let (status, body) = send(&app, get(uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} -> {body}");
    }
}

// ---------------------------------------------------------------------------
// The graph recall arm
// ---------------------------------------------------------------------------

#[tokio::test]
async fn graph_arm_surfaces_a_one_hop_neighbour_bm25_cannot_reach() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();

    let mut seed = NewNode::new(
        "b1",
        FactType::World,
        "the reranker uses reciprocal rank fusion",
    );
    seed.mentioned_at = Some(1_782_898_200_000);
    let seed_id = nodes::insert(&db, seed).unwrap();

    // Shares not one term with the query — unreachable by BM25.
    let mut neighbor = NewNode::new("b1", FactType::World, "옵시디언 볼트 동기화 실패");
    neighbor.mentioned_at = Some(1_782_898_200_000);
    let neighbor_id = nodes::insert(&db, neighbor).unwrap();

    let query = json!({ "query": "reciprocal rank fusion reranker" });

    // Before any link exists, only the seed comes back.
    let (status, body) = post(&app, "/v1/banks/b1/recall", query.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<i64> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![seed_id]);

    // One hop away — and in the *reverse* direction, which the arm must also
    // traverse.
    graph::insert_links(
        &db,
        &[NewLink {
            from_node_id: neighbor_id,
            to_node_id: seed_id,
            link_type: "semantic",
            weight: 0.9,
        }],
        0,
    )
    .unwrap();

    let (_, body) = post(&app, "/v1/banks/b1/recall", query).await;
    let ids: Vec<i64> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert!(
        ids.contains(&neighbor_id),
        "the graph arm must pull in the 1-hop neighbour: {ids:?}"
    );
    assert_eq!(
        ids[0], seed_id,
        "the seed still ranks first (two arms vs one)"
    );
    assert_eq!(body["counts"]["candidates"], 2);
}

#[tokio::test]
async fn graph_arm_reaches_a_neighbour_through_a_shared_entity() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();

    let mut seed = NewNode::new(
        "b1",
        FactType::World,
        "the reranker uses reciprocal rank fusion",
    );
    seed.mentioned_at = Some(1_782_898_200_000);
    let seed_id = nodes::insert(&db, seed).unwrap();
    let mut other = NewNode::new("b1", FactType::World, "완전히 다른 이야기");
    other.mentioned_at = Some(1_782_898_200_000);
    let other_id = nodes::insert(&db, other).unwrap();

    graph::write_entities(
        &db,
        "b1",
        &[
            (seed_id, vec!["ollama".to_string()], 0),
            (other_id, vec!["ollama".to_string()], 0),
        ],
        0,
    )
    .unwrap();

    let (_, body) = post(
        &app,
        "/v1/banks/b1/recall",
        json!({ "query": "reciprocal rank fusion reranker" }),
    )
    .await;
    let ids: Vec<i64> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert!(
        ids.contains(&other_id),
        "node_entities co-membership is a graph edge too: {ids:?}"
    );
}

#[tokio::test]
async fn graph_arm_respects_type_and_tag_filters() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();

    let mut seed = NewNode::new(
        "b1",
        FactType::World,
        "the reranker uses reciprocal rank fusion",
    );
    seed.mentioned_at = Some(1_782_898_200_000);
    let seed_id = nodes::insert(&db, seed).unwrap();
    let mut neighbor = NewNode::new("b1", FactType::Observation, "옵시디언 볼트 동기화 실패");
    neighbor.mentioned_at = Some(1_782_898_200_000);
    let neighbor_id = nodes::insert(&db, neighbor).unwrap();
    graph::insert_links(
        &db,
        &[NewLink {
            from_node_id: seed_id,
            to_node_id: neighbor_id,
            link_type: "semantic",
            weight: 0.9,
        }],
        0,
    )
    .unwrap();

    // An expanded node still has to clear the same type filter as a node the
    // retrieval arms found.
    let (_, body) = post(
        &app,
        "/v1/banks/b1/recall",
        json!({ "query": "reciprocal rank fusion reranker", "recallTypes": ["world"] }),
    )
    .await;
    let ids: Vec<i64> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![seed_id]);

    let (_, body) = post(
        &app,
        "/v1/banks/b1/recall",
        json!({ "query": "reciprocal rank fusion reranker",
                "tags": ["nothing-has-this"], "tagsMatch": "any_strict" }),
    )
    .await;
    assert!(body["results"].as_array().unwrap().is_empty());
}

/// **Measured line**: `graph::arm()` latency at 3k nodes (plan target ≤5ms).
///
/// This is the *arm-internal* number — the two expansion queries plus the
/// ranking. The real recall path additionally pays a `spawn_blocking` hop and
/// a hydrate of the newly-reached nodes; that end-to-end cost is the delta in
/// `hybrid_recall_bench`, quoted alongside in the design note.
///
/// Runs twice, because a uniform entity distribution is exactly the shape
/// that hides an uncapped fan-out (review MED-3):
///   * `uniform` — 300 entities over 3000 nodes, 10 nodes each
///   * `skewed`  — the same, plus one hub entity naming every node, which is
///     what `normalize()` merging name variants actually produces
///
///   cargo test --release -p memgardend --test graph_api -- --ignored --nocapture graph_arm_bench
#[tokio::test(flavor = "multi_thread")]
#[ignore = "seeds 3000 nodes and reports a timing"]
async fn graph_arm_bench() {
    use std::time::Instant;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("bench.db")).unwrap();
    banks::create(&db, "b1", None, None).unwrap();

    const N: usize = 3000;
    let ids: Vec<i64> = (0..N)
        .map(|i| {
            let text = format!("bench node number {i}");
            let mut node = NewNode::new("b1", FactType::World, &text);
            node.mentioned_at = Some(1_782_898_200_000 + i as i64);
            nodes::insert(&db, node).unwrap()
        })
        .collect();

    // A realistic link density and mix: every node links to its 20 temporal
    // successors (the per-node cap), its nearest 5 semantically, and every
    // 4th node carries a causal edge — so all three score buckets are live,
    // which is what makes this a check on the ranking and not just the SQL.
    let mut links = Vec::new();
    for (i, &from) in ids.iter().enumerate() {
        for (j, &to) in ids.iter().skip(i + 1).take(20).enumerate() {
            links.push(NewLink {
                from_node_id: from,
                to_node_id: to,
                link_type: "temporal",
                weight: 0.5,
            });
            if j < 5 {
                links.push(NewLink {
                    from_node_id: from,
                    to_node_id: to,
                    link_type: "semantic",
                    weight: 0.75 + (j as f64) * 0.04,
                });
            }
        }
        if i % 4 == 0 && i + 1 < ids.len() {
            links.push(NewLink {
                from_node_id: from,
                to_node_id: ids[i + 1],
                link_type: "caused_by",
                weight: 1.0,
            });
        }
    }
    graph::insert_links(&db, &links, 0).unwrap();
    let entity_batch: Vec<memgarden_store::graph::EntityMentions> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, vec![format!("entity {}", i % 300)], 0i64))
        .collect();
    graph::write_entities(&db, "b1", &entity_batch, 0).unwrap();

    let seeds: Vec<i64> = ids.iter().take(20).copied().collect();
    let run = |label: &str| {
        for _ in 0..5 {
            memgardend::recall::graph::arm(&db, "b1", &seeds).unwrap();
        }
        let mut samples: Vec<u128> = Vec::new();
        for _ in 0..200 {
            let t = Instant::now();
            let hits = memgardend::recall::graph::arm(&db, "b1", &seeds).unwrap();
            samples.push(t.elapsed().as_micros());
            assert_eq!(hits.len(), memgardend::recall::graph::GRAPH_EXPANSION_CAP);
        }
        samples.sort_unstable();
        println!(
            "graph arm [{label}] @ {N} nodes / {} links: p50 {}us p95 {}us max {}us",
            links.len(),
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100],
            samples[samples.len() - 1]
        );
    };
    run("uniform");

    // Skewed: one hub entity on every node. Uncapped this is the 100x case
    // (measured 10.3ms) — with MAX_ENTITY_FANOUT it must stay flat.
    let hub: Vec<memgarden_store::graph::EntityMentions> = ids
        .iter()
        .map(|id| (*id, vec!["hub".to_string()], 0i64))
        .collect();
    graph::write_entities(&db, "b1", &hub, 0).unwrap();
    let hub_mentions: i64 = db
        .read()
        .unwrap()
        .query_row(
            "SELECT mention_count FROM entities WHERE canonical_name = 'hub'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(hub_mentions > memgarden_store::graph::MAX_ENTITY_FANOUT);
    run("skewed (hub entity on all 3000 nodes)");
}

// ---------------------------------------------------------------------------
// R2 / architect F1: the semantic-link pass itself
// ---------------------------------------------------------------------------

/// Plan line 628 mandates "retain → backlog tick → semantic link exists".
/// `on_batch_embedded` is the backlog tick's second half and had no coverage:
/// every other test writes embeddings with `set_embeddings_batch` directly,
/// which bypasses it. `links::semantic_links`' unit tests cannot reach the two
/// things that live in the hook — the `1.0 - distance` cosine conversion and
/// the `TOP_K * 5` over-fetch.
///
/// Runs without the real model: the hook takes the vectors it was handed, so
/// hand-built ones exercise the same path a loaded embedder would.
#[tokio::test]
async fn backlog_tick_creates_semantic_links() {
    let db = Arc::new(Db::open_memory().unwrap());
    banks::create(&db, "b1", None, None).unwrap();

    let dim = memgarden_core::EMBEDDING_DIM;
    let unit = |f: &dyn Fn(usize) -> f32| -> Vec<f32> {
        let v: Vec<f32> = (0..dim).map(f).collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / norm).collect()
    };
    // Two nearly-parallel vectors: cosine ~0.995, i.e. vec0 distance ~0.005.
    // If the hook ever reads the distance as a similarity, both fall under
    // the 0.7 threshold and no link is written — which is the assert below.
    let a = unit(&|i| if i == 0 { 1.0 } else { 0.0 });
    let b = unit(&|i| {
        if i == 0 {
            1.0
        } else if i == 1 {
            0.1
        } else {
            0.0
        }
    });

    let mk = |ft: FactType, text: &str| {
        let mut n = NewNode::new("b1", ft, text);
        n.mentioned_at = Some(1_782_898_200_000);
        nodes::insert(&db, n).unwrap()
    };
    let w1 = mk(FactType::World, "world fact one");
    let w2 = mk(FactType::World, "world fact two");

    // 40 observations sitting *closer* to w1 than w2 does. They must not
    // become links (wrong fact_type), and they must not crowd w2 out of the
    // KNN window either — which is what the TOP_K * 5 over-fetch is for. Drop
    // the over-fetch to a bare TOP_K and w2 falls outside the k=20 window and
    // this test fails.
    let mut batch: Vec<(i64, String, Vec<f32>)> = Vec::new();
    for i in 0..40 {
        let id = mk(FactType::Observation, &format!("observation {i}"));
        let v = unit(&|j| {
            if j == 0 {
                1.0
            } else if j == 1 {
                0.001
            } else {
                0.0
            }
        });
        batch.push((id, "b1".to_string(), v));
    }
    batch.push((w1, "b1".to_string(), a));
    batch.push((w2, "b1".to_string(), b));

    // The first half of a real tick: commit the embeddings...
    nodes::set_embeddings_batch(&db, &batch).unwrap();
    // ...then the hook, exactly as `drain_once` calls it.
    memgardend::embed_task::on_batch_embedded(&db, batch).await;

    let semantic = count(
        &db,
        "SELECT count(*) FROM links WHERE link_type = 'semantic'",
    );
    assert!(semantic > 0, "the backlog tick must create semantic links");
    let across_types: i64 = db
        .read()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM links l
             JOIN memory_nodes f ON f.id = l.from_node_id
             JOIN memory_nodes t ON t.id = l.to_node_id
             WHERE l.link_type = 'semantic' AND f.fact_type != t.fact_type",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(across_types, 0, "semantic links are per fact_type");
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM links WHERE link_type='semantic'
               AND ((from_node_id=1 AND to_node_id=2) OR (from_node_id=2 AND to_node_id=1))"
        ),
        2,
        "the two world facts must find each other despite 40 closer observations"
    );
    // Weights are similarities, not distances: ~0.995, nowhere near 0.005.
    let w: f64 = db
        .read()
        .unwrap()
        .query_row(
            "SELECT min(weight) FROM links WHERE link_type = 'semantic'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        w > 0.7,
        "weight {w} must be a cosine similarity, not a distance"
    );
}

/// CE-7: a semantic edge must be able to reach a node embedded in an *earlier*
/// batch.
///
/// The test above hands all 42 nodes to one `on_batch_embedded` call, so it is
/// structurally unable to see this: it would pass with the fact_type oracle
/// built from the batch alone, which is how the defect survived. Everything a
/// real backlog drain does happens across batches —
/// `embedding.batch_size` defaults to 8 — so under the old code every semantic
/// edge joined two nodes from the same batch and out-degree capped at 7
/// against a `SEMANTIC_LINK_TOP_K` of 20.
///
/// Two batches, one node each, deliberately: with the old code this asserts 0
/// links where the property demands 2.
#[tokio::test]
async fn a_semantic_link_reaches_a_node_embedded_in_an_earlier_batch() {
    let db = Arc::new(Db::open_memory().unwrap());
    banks::create(&db, "b1", None, None).unwrap();

    let dim = memgarden_core::EMBEDDING_DIM;
    let unit = |second: f32| -> Vec<f32> {
        let mut v = vec![0.0f32; dim];
        v[0] = 1.0;
        v[1] = second;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / norm).collect()
    };

    let mk = |text: &str| {
        let mut n = NewNode::new("b1", FactType::World, text);
        n.mentioned_at = Some(1_782_898_200_000);
        nodes::insert(&db, n).unwrap()
    };
    let first = mk("world fact embedded first");
    let second = mk("world fact embedded second");

    // Batch one, alone. Nothing to link to yet.
    let b1 = vec![(first, "b1".to_string(), unit(0.0))];
    nodes::set_embeddings_batch(&db, &b1).unwrap();
    memgardend::embed_task::on_batch_embedded(&db, b1).await;
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM links WHERE link_type = 'semantic'"
        ),
        0,
        "a lone first batch has no same-type neighbour to reach"
    );

    // Batch two. Its KNN finds `first`, which is not in this batch.
    let b2 = vec![(second, "b1".to_string(), unit(0.1))];
    nodes::set_embeddings_batch(&db, &b2).unwrap();
    memgardend::embed_task::on_batch_embedded(&db, b2).await;

    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM links WHERE link_type='semantic'
               AND from_node_id=2 AND to_node_id=1"
        ),
        1,
        "the second batch must link to the node the first batch embedded"
    );

    // **One edge, not two, and that is the pass's shape rather than a
    // shortfall of this fix.** `on_batch_embedded` only ever writes edges
    // *out of* the nodes it was just handed, so `first` cannot acquire an edge
    // to a node that did not exist when it was embedded. The test above sees 2
    // only because both of its nodes are in one batch and each links to the
    // other. Consequence worth knowing: in a growing bank the semantic graph
    // is built in insertion order, and an early node's out-edges are fixed at
    // the moment it drains. Widening that means re-linking settled nodes on
    // every batch, which is a different decision from this one.
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM links WHERE link_type='semantic'
               AND from_node_id=1 AND to_node_id=2"
        ),
        0,
        "the earlier node keeps the out-edges it had when it drained"
    );
}

// ---------------------------------------------------------------------------
// E1: GET /v1/banks/{bank_id}/nodes/{id}
// ---------------------------------------------------------------------------

/// `/graph` truncates node text to a 160-character label and its own comment
/// promises the viewer "fetches the full text on click". Until E1 there was
/// nothing to fetch it from.
#[tokio::test]
async fn node_detail_returns_the_untruncated_text() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();

    let long = "가".repeat(400);
    let id = nodes::insert(&db, NewNode::new("b1", FactType::World, &long)).unwrap();

    let (status, graph) = send(&app, get("/v1/banks/b1/graph?limit=10")).await;
    assert_eq!(status, StatusCode::OK);
    let label = graph["nodes"][0]["text"].as_str().unwrap();
    assert!(
        label.chars().count() < 200 && label.ends_with('…'),
        "the graph endpoint still truncates"
    );

    let (status, node) = send(&app, get(&format!("/v1/banks/b1/nodes/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        node["text"].as_str().unwrap().chars().count(),
        400,
        "the detail endpoint must return the whole text"
    );
}

/// **The property this endpoint exists to get right.**
///
/// `links` is keyed `(from_node_id, to_node_id, link_type, entity_id)` and the
/// semantic pass writes edges only *out of* the batch it was handed, so a pair
/// embedded together has two rows and a pair spanning batches has one — see
/// `a_semantic_link_reaches_a_node_embedded_in_an_earlier_batch` above.
/// Reading a single direction would make a node's neighbourhood depend on when
/// it happened to be embedded, which is not a property of the memory.
#[tokio::test]
async fn neighbours_are_found_through_an_edge_pointing_either_way() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();

    let subject = nodes::insert(&db, NewNode::new("b1", FactType::World, "the subject")).unwrap();
    let outgoing =
        nodes::insert(&db, NewNode::new("b1", FactType::World, "edge points away")).unwrap();
    let incoming = nodes::insert(
        &db,
        NewNode::new("b1", FactType::World, "edge points at us"),
    )
    .unwrap();

    graph::insert_links(
        &db,
        &[
            NewLink {
                from_node_id: subject,
                to_node_id: outgoing,
                link_type: "semantic",
                weight: 0.9,
            },
            // Only the reverse row exists for this one, exactly as a
            // cross-batch semantic edge leaves it.
            NewLink {
                from_node_id: incoming,
                to_node_id: subject,
                link_type: "semantic",
                weight: 0.8,
            },
        ],
        1,
    )
    .unwrap();

    let (status, node) = send(&app, get(&format!("/v1/banks/b1/nodes/{subject}"))).await;
    assert_eq!(status, StatusCode::OK);
    let semantic = node["neighbors"]["semantic"].as_array().unwrap();
    let ids: Vec<i64> = semantic.iter().map(|n| n["id"].as_i64().unwrap()).collect();

    assert!(
        ids.contains(&outgoing) && ids.contains(&incoming),
        "both directions must appear; got {ids:?}"
    );
    // Ordered by weight, and the subject is never its own neighbour.
    assert_eq!(ids[0], outgoing, "strongest edge first");
    assert!(!ids.contains(&subject));
}

/// `proof_count` is derived from `node_sources`, and MG-2 reports 93
/// disagreements against legacy's stored value. Both directions of the join
/// are served so that number is auditable from the screen: `sources` is what
/// this observation was built from, `cited_by` is what was built from it.
#[tokio::test]
async fn provenance_is_reported_in_both_directions() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();

    let fact = nodes::insert(&db, NewNode::new("b1", FactType::World, "a source fact")).unwrap();
    // The production path, so the provenance rows are written the way
    // consolidation writes them rather than by hand.
    let obs = memgarden_store::consolidate::insert_observation(
        &db,
        "b1",
        "an observation over it",
        &vec![0.0f32; memgarden_core::EMBEDDING_DIM],
        &[fact],
    )
    .unwrap();

    let (_, o) = send(&app, get(&format!("/v1/banks/b1/nodes/{obs}"))).await;
    assert_eq!(o["sources"][0]["id"].as_i64().unwrap(), fact);
    assert!(o["cited_by"].as_array().unwrap().is_empty());

    let (_, f) = send(&app, get(&format!("/v1/banks/b1/nodes/{fact}"))).await;
    assert_eq!(f["cited_by"][0]["id"].as_i64().unwrap(), obs);
    assert!(f["sources"].as_array().unwrap().is_empty());
}

/// The bank in the path is part of the identity, not decoration. A node in
/// another bank is a 404 and not a 200 — and it is the *same* 404 as a node
/// that does not exist, so ids cannot be enumerated across banks.
#[tokio::test]
async fn a_node_in_another_bank_is_not_readable_through_this_one() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();
    banks::create(&db, "b2", None, None).unwrap();
    let theirs = nodes::insert(&db, NewNode::new("b2", FactType::World, "b2's memory")).unwrap();

    let (status, _) = send(&app, get(&format!("/v1/banks/b1/nodes/{theirs}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = send(&app, get(&format!("/v1/banks/b1/nodes/{}", theirs + 9999))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "same answer, no oracle");

    let (status, _) = send(&app, get(&format!("/v1/banks/b2/nodes/{theirs}"))).await;
    assert_eq!(status, StatusCode::OK, "its own bank still reads it");
}

// ---------------------------------------------------------------------------
// E1: the UI is served by the daemon
// ---------------------------------------------------------------------------

/// Same origin as the API is the point: `check_host` then passes on the Host
/// header a browser sends anyway, so the UI needs no token and there is no
/// CORS to configure. This asserts the assets are reachable and that the
/// route does not become an arbitrary file reader.
#[tokio::test]
async fn the_ui_is_served_from_the_daemon_and_is_not_a_file_server() {
    let (app, _db) = read_only_app();

    for (path, needle) in [
        ("/ui/", "<title>MemGarden</title>"),
        ("/ui/app.js", "/v1/banks/"),
        ("/ui/style.css", "#search-panel"),
        // E3's vendored libraries. Listed by exact path like everything else
        // here — the point of this test is that the set is exact.
        ("/ui/vendor/sigma.js", "Sigma"),
        ("/ui/vendor/graphology.js", "graphology"),
        ("/ui/vendor/d3-force.js", "d3-force"),
        ("/ui/vendor/d3-quadtree.js", "d3-quadtree"),
        ("/ui/vendor/d3-timer.js", "d3-timer"),
        ("/ui/vendor/d3-dispatch.js", "d3-dispatch"),
    ] {
        let res = app.clone().oneshot(get(path)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains(needle), "{path} did not carry {needle:?}");
    }

    // Nothing under /ui/ resolves to a path on disk, so traversal has nothing
    // to traverse — every unknown asset is a flat 404.
    for path in [
        "/ui/../Cargo.toml",
        "/ui/%2e%2e/Cargo.toml",
        "/ui/nope.js",
        // A vendored file that exists on disk but is not routed: proof the
        // vendor directory is a list of routes and not a served directory.
        "/ui/vendor/README.md",
        "/ui/vendor/LICENSE-sigma.txt",
        "/ui/vendor/sigma-3.0.3.min.js",
    ] {
        let res = app.clone().oneshot(get(path)).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "{path} must not resolve"
        );
    }
}

/// The host guard covers the UI too — it is applied to the whole router, and
/// a UI served to a rebound hostname would hand a malicious page the same
/// origin as the API.
#[tokio::test]
async fn the_ui_is_behind_the_same_host_guard_as_the_api() {
    let (app, _db) = read_only_app();
    let req = Request::builder()
        .method("GET")
        .uri("/ui/")
        .header("host", "memgarden.example.com")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

/// **E2's contract.** The ego-graph draws from this response and nothing else,
/// so what it reads is pinned here rather than left to be discovered when the
/// canvas silently renders empty.
///
/// A graph that draws nothing looks exactly like a node with no neighbours,
/// which is why this is a test and not a manual check: rename `label`, flatten
/// `neighbors` into one array, or drop `weight`, and `ui/app.js`'s `drawEgo`
/// stops finding what it walks — with no error anywhere.
///
/// The four properties it depends on:
///
/// 1. `neighbors` is an **object keyed by link type**. Each key becomes one
///    sector and supplies the sector's label, so a flat array would collapse
///    the whole legend.
/// 2. Every neighbour row carries `id`, `label` and `type` — the click target,
///    the hover title, and the fact_type that colours the dot.
/// 3. `weight` is present and inside `[0, 1]` on a link-derived neighbour.
///    `radiusFor` maps it onto a radius and would place a rogue value off the
///    canvas.
/// 4. Provenance rows (`sources` / `cited_by`) have the same shape **minus**
///    `weight`, which is why the drawing sends them to the midpoint radius
///    instead of claiming a similarity they do not have.
#[tokio::test]
async fn node_detail_carries_what_the_ego_graph_draws() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();

    let subject = nodes::insert(&db, NewNode::new("b1", FactType::World, "the centre")).unwrap();
    let near = nodes::insert(&db, NewNode::new("b1", FactType::World, "a close one")).unwrap();
    let later = nodes::insert(&db, NewNode::new("b1", FactType::World, "a same-day one")).unwrap();
    // A provenance edge, written by the production path, so `sources` is
    // populated the way consolidation populates it.
    let obs = memgarden_store::consolidate::insert_observation(
        &db,
        "b1",
        "an observation over the centre",
        &vec![0.0f32; memgarden_core::EMBEDDING_DIM],
        &[subject],
    )
    .unwrap();

    graph::insert_links(
        &db,
        &[
            NewLink {
                from_node_id: subject,
                to_node_id: near,
                link_type: "semantic",
                weight: 0.93,
            },
            NewLink {
                from_node_id: subject,
                to_node_id: later,
                link_type: "temporal",
                weight: 0.49,
            },
        ],
        1,
    )
    .unwrap();

    let (status, node) = send(&app, get(&format!("/v1/banks/b1/nodes/{subject}"))).await;
    assert_eq!(status, StatusCode::OK);

    // 1. Keyed by link type, one key per sector.
    let by_type = node["neighbors"]
        .as_object()
        .expect("neighbors must be an object keyed by link type, not a flat array");
    let mut types: Vec<&String> = by_type.keys().collect();
    types.sort();
    assert_eq!(types, vec!["semantic", "temporal"]);

    // 2 and 3. Every row is drawable, and its weight is a radius input.
    for (link_type, rows) in by_type {
        for row in rows.as_array().expect("each link type holds an array") {
            assert!(row["id"].is_i64(), "{link_type} row needs an id to click");
            assert!(
                row["label"].as_str().is_some_and(|s| !s.is_empty()),
                "{link_type} row needs a label for the hover title"
            );
            assert!(
                row["type"].as_str().is_some_and(|s| !s.is_empty()),
                "{link_type} row needs a fact_type to colour the dot"
            );
            let w = row["weight"]
                .as_f64()
                .unwrap_or_else(|| panic!("{link_type} row needs a weight"));
            assert!(
                (0.0..=1.0).contains(&w),
                "{link_type} weight {w} is outside [0,1] and would land off the canvas"
            );
        }
    }

    // 4. Provenance: same shape, and deliberately weightless.
    let source_row = &node["cited_by"][0];
    assert_eq!(source_row["id"].as_i64().unwrap(), obs);
    assert!(source_row["label"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(source_row["type"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(
        source_row["weight"].is_null(),
        "provenance carries no similarity; the drawing depends on that absence"
    );
}

/// E3's date filter, and why it is SQL rather than a `.filter()` on the way
/// out.
///
/// `graph_view` orders by `id DESC` and takes `limit`, so it returns the
/// *newest* N. A range applied after that could only ever narrow the newest
/// window — it could never reach a memory older than it, which is the entire
/// reason someone asks for a date. This pins that the bounds run inside the
/// query: the old node here is outside the newest-1 window and must still
/// come back when asked for by date.
#[tokio::test]
async fn the_graph_date_filter_reaches_past_the_newest_window() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();

    let day = 86_400_000i64;
    let mk = |text: &str, event_date: i64| {
        let mut n = NewNode::new("b1", FactType::World, text);
        n.event_date = Some(event_date);
        n.mentioned_at = Some(event_date);
        nodes::insert(&db, n).unwrap()
    };
    // Inserted oldest-first, so `id DESC` puts `newest` first.
    let oldest = mk("a memory from the first day", day);
    let middle = mk("a memory from the second day", 2 * day);
    let newest = mk("a memory from the third day", 3 * day);

    let ids = |v: &serde_json::Value| -> Vec<i64> {
        let mut out: Vec<i64> = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_i64().unwrap())
            .collect();
        out.sort_unstable();
        out
    };

    // The window alone: only the newest survives `limit=1`.
    let (status, v) = send(&app, get("/v1/banks/b1/graph?limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&v), vec![newest]);

    // The same window, asked for by date. If the bound were applied after the
    // limit this would be empty, because the newest node is not in range.
    let (_, v) = send(
        &app,
        get(&format!("/v1/banks/b1/graph?limit=1&until={}", day)),
    )
    .await;
    assert_eq!(
        ids(&v),
        vec![oldest],
        "a date range must reach past the newest {{limit}} rows"
    );

    // `since` alone, and it has to be decisive on its own: with the lower
    // bound dropped this returns all three.
    let (_, v) = send(
        &app,
        get(&format!("/v1/banks/b1/graph?limit=100&since={}", 3 * day)),
    )
    .await;
    assert_eq!(ids(&v), vec![newest], "since excludes everything older");

    // Both bounds, inclusive at each end.
    let (_, v) = send(
        &app,
        get(&format!(
            "/v1/banks/b1/graph?limit=100&since={}&until={}",
            day,
            2 * day
        )),
    )
    .await;
    assert_eq!(ids(&v), vec![oldest, middle], "bounds are inclusive");

    // Absent bounds mean unbounded, not zero.
    let (_, v) = send(&app, get("/v1/banks/b1/graph?limit=100")).await;
    assert_eq!(ids(&v), vec![oldest, middle, newest]);
}

/// `?ids=` returns the induced subgraph over a set the caller already holds —
/// the half of the picture `nodes/{id}` cannot give.
///
/// `nodes/{id}` answers "what is adjacent to *this* one" and nothing else, so
/// an ego view built from it is a star: two neighbours that are themselves
/// linked get no edge between them, and walking the graph shows the path taken
/// rather than the fabric around it. Measured on the live bank, one node's ego
/// view drew 4 edges where the same node set actually holds 10.
#[tokio::test]
async fn graph_ids_returns_the_links_between_a_set_the_caller_already_has() {
    let (app, db) = read_only_app();
    banks::create(&db, "b1", None, None).unwrap();

    let mk = |t: &str| nodes::insert(&db, NewNode::new("b1", FactType::World, t)).unwrap();
    let a = mk("a");
    let b = mk("b");
    let c = mk("c");
    let outside = mk("not in the set");

    graph::insert_links(
        &db,
        &[
            // The star `nodes/{a}` would report…
            NewLink {
                from_node_id: a,
                to_node_id: b,
                link_type: "semantic",
                weight: 0.9,
            },
            NewLink {
                from_node_id: a,
                to_node_id: c,
                link_type: "semantic",
                weight: 0.8,
            },
            // …and the edge it cannot: between two of a's neighbours.
            NewLink {
                from_node_id: b,
                to_node_id: c,
                link_type: "semantic",
                weight: 0.85,
            },
            // An edge leaving the set, which must not come back.
            NewLink {
                from_node_id: c,
                to_node_id: outside,
                link_type: "semantic",
                weight: 0.7,
            },
        ],
        1,
    )
    .unwrap();

    let (status, v) = send(
        &app,
        get(&format!("/v1/banks/b1/graph?limit=100&ids={a},{b},{c}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let mut got: Vec<i64> = v["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![a, b, c], "exactly the requested set, and no more");

    let pairs: Vec<(i64, i64)> = v["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let (x, y) = (l["from"].as_i64().unwrap(), l["to"].as_i64().unwrap());
            if x < y { (x, y) } else { (y, x) }
        })
        .collect();
    assert!(
        pairs.contains(&(b.min(c), b.max(c))),
        "the neighbour-to-neighbour edge is the whole point; got {pairs:?}"
    );
    assert!(
        !pairs.iter().any(|(x, y)| *x == outside || *y == outside),
        "an edge leaving the set must not come back"
    );

    // Without `ids` the same bank hands back the newest rows instead, so the
    // parameter is a selection and not decoration.
    let (_, v) = send(&app, get("/v1/banks/b1/graph?limit=100")).await;
    assert_eq!(v["nodes"].as_array().unwrap().len(), 4);
}
