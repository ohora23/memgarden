use memgarden_core::EMBEDDING_DIM;
use memgarden_core::error::Error;
use memgarden_core::types::FactType;
use memgarden_store::models::NewNode;
use memgarden_store::{Db, banks, nodes, search, vecblob};

fn store_err(e: rusqlite::Error) -> Error {
    Error::Storage(e.to_string())
}

#[test]
fn migrate_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let db1 = Db::open(&path).unwrap();
    drop(db1);
    let db2 = Db::open(&path).unwrap(); // re-opening re-runs migrate(); must be a no-op

    let conn = db2.read().unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, memgarden_store::LATEST_VERSION);
    let count: i64 = conn
        .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count,
        memgarden_store::LATEST_VERSION,
        "schema_migrations must log each migration exactly once"
    );
}

/// A database created by an older build (schema v1) must upgrade in place
/// when a newer binary opens it — `0002` is the first migration that has to
/// prove this, since `0001` only ever runs against an empty file.
#[test]
fn migrate_upgrades_a_v1_database_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.db");

    // Build a v1 database the way the previous release left it: apply
    // 0001 only, then stamp user_version = 1.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(include_str!("../migrations/0001_init.sql"))
            .unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (1, 0)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        // Pre-existing data must survive the upgrade.
        conn.execute(
            "INSERT INTO banks (bank_id, created_at, updated_at) VALUES ('legacy', 0, 0)",
            [],
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    let conn = db.read().unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, memgarden_store::LATEST_VERSION);
    // 0002's table exists...
    let jobs: i64 = conn
        .query_row("SELECT count(*) FROM retain_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(jobs, 0);
    // ...and the v1 row is still there.
    let banks: i64 = conn
        .query_row("SELECT count(*) FROM banks WHERE bank_id = 'legacy'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(banks, 1);
}

#[test]
fn fts_triggers_sync() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id = nodes::insert(
        &db,
        NewNode::new("b1", FactType::World, "original text about apples"),
    )
    .unwrap();

    assert_eq!(
        search::fts_candidates(&db, "b1", "apples*", 10).unwrap(),
        vec![id]
    );

    db.write(|tx| {
        tx.execute(
            "UPDATE memory_nodes SET text = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params!["updated text about oranges", memgarden_core::now_ms(), id],
        )
        .unwrap();
        Ok(())
    })
    .unwrap();

    assert!(
        search::fts_candidates(&db, "b1", "apples*", 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        search::fts_candidates(&db, "b1", "oranges*", 10).unwrap(),
        vec![id]
    );

    nodes::delete(&db, id).unwrap();
    assert!(
        search::fts_candidates(&db, "b1", "oranges*", 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn fts_korean_prefix() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id = nodes::insert(
        &db,
        NewNode::new("b1", FactType::World, "새 데몬에 연결되었다"),
    )
    .unwrap();

    // Bare root word doesn't match: unicode61 tokenizes "데몬에" as one token
    // (Korean particles attach without a space), not "데몬".
    assert!(
        search::fts_candidates(&db, "b1", "데몬", 10)
            .unwrap()
            .is_empty()
    );

    // fts_query_string appends '*', which matches via the prefix='2 3 4' index.
    let query = search::fts_query_string("데몬");
    assert_eq!(query, "데몬*");
    assert_eq!(
        search::fts_candidates(&db, "b1", &query, 10).unwrap(),
        vec![id]
    );
}

#[test]
fn fts_korean_compound_negative() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    nodes::insert(&db, NewNode::new("b1", FactType::World, "메모리회수 로직")).unwrap();

    // Known limit: prefix indexing only matches from the start of a token.
    // "회수" is a suffix inside the unsegmented compound "메모리회수", not a
    // prefix, so it is unreachable via '회수*'.
    assert!(
        search::fts_candidates(&db, "b1", "회수*", 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn vec_knn_partitioned() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    banks::create(&db, "b2", None, None).unwrap();

    let id1 = nodes::insert(&db, NewNode::new("b1", FactType::World, "n1")).unwrap();
    let id2 = nodes::insert(&db, NewNode::new("b2", FactType::World, "n2")).unwrap();

    let mut v1 = vec![0.0f32; EMBEDDING_DIM];
    v1[0] = 1.0;
    let mut v2 = vec![0.0f32; EMBEDDING_DIM];
    v2[1] = 1.0;

    nodes::set_embedding(&db, id1, "b1", &v1).unwrap();
    nodes::set_embedding(&db, id2, "b2", &v2).unwrap();

    let hits = search::knn(&db, "b1", &v1, 10).unwrap();
    let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        ids,
        vec![id1],
        "b2's node must not leak into b1's KNN results"
    );
}

#[test]
fn set_embedding_roundtrip() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id = nodes::insert(&db, NewNode::new("b1", FactType::World, "n")).unwrap();

    let v: Vec<f32> = (0..EMBEDDING_DIM).map(|i| i as f32 * 0.01).collect();
    nodes::set_embedding(&db, id, "b1", &v).unwrap();

    let node = nodes::get(&db, id).unwrap().unwrap();
    let decoded = vecblob::decode(&node.embedding.unwrap()).unwrap();
    assert_eq!(decoded, v);

    let wrong_dim = vec![0.0f32; 10];
    assert!(nodes::set_embedding(&db, id, "b1", &wrong_dim).is_err());
}

#[test]
fn cascade_delete() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id1 = nodes::insert(&db, NewNode::new("b1", FactType::World, "n1 keyword")).unwrap();
    let id2 = nodes::insert(&db, NewNode::new("b1", FactType::World, "n2")).unwrap();
    nodes::add_tags(&db, id1, &["tag-a"]).unwrap();

    let v = vec![0.3f32; EMBEDDING_DIM];
    nodes::set_embedding(&db, id1, "b1", &v).unwrap();

    db.write(|tx| {
        tx.execute(
            "INSERT INTO links (from_node_id, to_node_id, link_type, created_at) VALUES (?1, ?2, 'semantic', ?3)",
            rusqlite::params![id1, id2, memgarden_core::now_ms()],
        )
        .unwrap();
        Ok(())
    })
    .unwrap();

    banks::delete(&db, "b1").unwrap();

    let conn = db.read().unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT count(*) FROM memory_nodes"), 0);
    assert_eq!(count("SELECT count(*) FROM links"), 0);
    assert_eq!(count("SELECT count(*) FROM node_tags"), 0);
    assert_eq!(count("SELECT count(*) FROM memory_nodes_fts"), 0);
    assert_eq!(
        count("SELECT count(*) FROM vec_nodes"),
        0,
        "memory_nodes_vec_ad trigger must fire on FK-cascade deletes too, not just via nodes::delete"
    );
}

#[test]
fn link_types_seven() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id1 = nodes::insert(&db, NewNode::new("b1", FactType::World, "a")).unwrap();
    let id2 = nodes::insert(&db, NewNode::new("b1", FactType::World, "b")).unwrap();

    let valid = [
        "semantic",
        "temporal",
        "entity",
        "caused_by",
        "causes",
        "enables",
        "prevents",
    ];
    for lt in valid {
        db.write(|tx| {
            tx.execute(
                "INSERT INTO links (from_node_id, to_node_id, link_type, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id1, id2, lt, memgarden_core::now_ms()],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
    }

    let conn = db.read().unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM links", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 7);
    drop(conn);

    let result = db.write(|tx| {
        tx.execute(
            "INSERT INTO links (from_node_id, to_node_id, link_type, created_at) VALUES (?1, ?2, 'bogus', ?3)",
            rusqlite::params![id1, id2, memgarden_core::now_ms()],
        )
        .map_err(store_err)?;
        Ok(())
    });
    assert!(
        result.is_err(),
        "an 8th, invalid link_type must be rejected by the CHECK constraint"
    );
}

#[test]
fn entity_distinct_links() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id1 = nodes::insert(&db, NewNode::new("b1", FactType::World, "a")).unwrap();
    let id2 = nodes::insert(&db, NewNode::new("b1", FactType::World, "b")).unwrap();

    // entity_id has no FK (documented, not enforced) — these need not exist
    // in the entities table.
    for entity_id in [0i64, 42i64] {
        db.write(|tx| {
            tx.execute(
                "INSERT INTO links (from_node_id, to_node_id, link_type, entity_id, created_at)
                 VALUES (?1, ?2, 'entity', ?3, ?4)",
                rusqlite::params![id1, id2, entity_id, memgarden_core::now_ms()],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
    }

    let conn = db.read().unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM links", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "same from/to/type with different entity_id must be two distinct rows"
    );
}

#[test]
fn bank_crud() {
    let db = Db::open_memory().unwrap();
    let created = banks::create(&db, "b1", Some("mission"), None).unwrap();
    assert_eq!(created.bank_id, "b1");

    let fetched = banks::get(&db, "b1").unwrap().unwrap();
    assert_eq!(fetched.mission.as_deref(), Some("mission"));
    assert_eq!(fetched.disposition, None);

    banks::update(
        &db,
        "b1",
        Some(Some("new mission")),
        Some(Some(r#"{"k":"v"}"#)),
    )
    .unwrap();
    let updated = banks::get(&db, "b1").unwrap().unwrap();
    assert_eq!(updated.mission.as_deref(), Some("new mission"));
    assert_eq!(updated.disposition.as_deref(), Some(r#"{"k":"v"}"#));

    banks::delete(&db, "b1").unwrap();
    assert!(banks::get(&db, "b1").unwrap().is_none());

    assert!(banks::update(&db, "missing", None, None).is_err());
}

#[test]
fn pending_embeddings_only_returns_null_embedding_rows() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id1 = nodes::insert(&db, NewNode::new("b1", FactType::World, "no embedding yet")).unwrap();
    let id2 = nodes::insert(&db, NewNode::new("b1", FactType::World, "has embedding")).unwrap();
    nodes::set_embedding(&db, id2, "b1", &vec![0.1f32; EMBEDDING_DIM]).unwrap();

    let pending = nodes::pending_embeddings(&db, 10).unwrap();
    let ids: Vec<i64> = pending.iter().map(|(id, ..)| *id).collect();
    assert_eq!(ids, vec![id1]);
    assert_eq!(pending[0].1, "b1");
    assert_eq!(pending[0].2, "no embedding yet");
}

#[test]
fn pending_embeddings_respects_limit() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    for i in 0..5 {
        nodes::insert(&db, NewNode::new("b1", FactType::World, &format!("n{i}"))).unwrap();
    }
    assert_eq!(nodes::pending_embeddings(&db, 3).unwrap().len(), 3);
}

#[test]
fn set_embeddings_batch_roundtrip() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id1 = nodes::insert(&db, NewNode::new("b1", FactType::World, "a")).unwrap();
    let id2 = nodes::insert(&db, NewNode::new("b1", FactType::World, "b")).unwrap();

    let v1: Vec<f32> = (0..EMBEDDING_DIM).map(|i| i as f32 * 0.001).collect();
    let v2: Vec<f32> = (0..EMBEDDING_DIM).map(|i| i as f32 * -0.001).collect();
    nodes::set_embeddings_batch(
        &db,
        &[
            (id1, "b1".to_string(), v1.clone()),
            (id2, "b1".to_string(), v2.clone()),
        ],
    )
    .unwrap();

    let n1 = nodes::get(&db, id1).unwrap().unwrap();
    let n2 = nodes::get(&db, id2).unwrap().unwrap();
    assert_eq!(vecblob::decode(&n1.embedding.unwrap()).unwrap(), v1);
    assert_eq!(vecblob::decode(&n2.embedding.unwrap()).unwrap(), v2);
    assert!(nodes::pending_embeddings(&db, 10).unwrap().is_empty());

    let hits = search::knn(&db, "b1", &v1, 10).unwrap();
    assert_eq!(hits[0].0, id1);
}

#[test]
fn set_embeddings_batch_dimension_error() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id = nodes::insert(&db, NewNode::new("b1", FactType::World, "a")).unwrap();
    let wrong_dim = vec![0.0f32; 10];
    assert!(nodes::set_embeddings_batch(&db, &[(id, "b1".to_string(), wrong_dim)]).is_err());
}

#[test]
fn rebuild_vec_index_restores_after_truncate() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id = nodes::insert(&db, NewNode::new("b1", FactType::World, "n")).unwrap();
    let v = vec![0.2f32; EMBEDDING_DIM];
    nodes::set_embedding(&db, id, "b1", &v).unwrap();

    db.write(|tx| {
        tx.execute("DELETE FROM vec_nodes", []).unwrap();
        Ok(())
    })
    .unwrap();
    assert!(search::knn(&db, "b1", &v, 10).unwrap().is_empty());

    let rebuilt = search::rebuild_vec_index(&db, None).unwrap();
    assert_eq!(rebuilt, 1);
    let hits = search::knn(&db, "b1", &v, 10).unwrap();
    assert_eq!(hits[0].0, id);
}

#[test]
fn rebuild_vec_index_is_bank_scoped() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    banks::create(&db, "b2", None, None).unwrap();
    let id1 = nodes::insert(&db, NewNode::new("b1", FactType::World, "n1")).unwrap();
    let id2 = nodes::insert(&db, NewNode::new("b2", FactType::World, "n2")).unwrap();
    let v1 = vec![0.3f32; EMBEDDING_DIM];
    let v2 = vec![0.4f32; EMBEDDING_DIM];
    nodes::set_embedding(&db, id1, "b1", &v1).unwrap();
    nodes::set_embedding(&db, id2, "b2", &v2).unwrap();

    db.write(|tx| {
        tx.execute("DELETE FROM vec_nodes", []).unwrap();
        Ok(())
    })
    .unwrap();

    let rebuilt = search::rebuild_vec_index(&db, Some("b1")).unwrap();
    assert_eq!(rebuilt, 1);
    assert_eq!(search::knn(&db, "b1", &v1, 10).unwrap().len(), 1);
    assert!(
        search::knn(&db, "b2", &v2, 10).unwrap().is_empty(),
        "unscoped bank must not be rebuilt"
    );
}

/// Critic Revision R1: the whole reason `fts_query_string` joins with `OR`.
/// A realistic multi-token prompt must return hits — under the previous
/// whitespace (implicit AND) join both of these measured zero.
#[test]
fn fts_multi_token_queries_hit() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let en = nodes::insert(
        &db,
        NewNode::new(
            "b1",
            FactType::World,
            "the recall pipeline fuses BM25 and vector candidates with reciprocal rank fusion",
        ),
    )
    .unwrap();
    let ko = nodes::insert(
        &db,
        NewNode::new(
            "b1",
            FactType::World,
            "메모리 회수 파이프라인은 하이브리드 검색으로 후보를 융합한다",
        ),
    )
    .unwrap();

    // 13 tokens (> MAX_QUERY_TERMS, so the cap is exercised too), only some
    // of which appear in the stored text.
    let english = "how does the hybrid recall pipeline combine keyword search \
                   with vector similarity again";
    assert!(english.split_whitespace().count() >= 12);
    let hits = search::fts_candidates(&db, "b1", &search::fts_query_string(english), 10).unwrap();
    assert!(hits.contains(&en), "multi-token English query found nothing");

    // 5 Korean tokens.
    let korean = "메모리 회수 파이프라인 하이브리드 검색";
    assert_eq!(korean.split_whitespace().count(), 5);
    let hits = search::fts_candidates(&db, "b1", &search::fts_query_string(korean), 10).unwrap();
    assert!(hits.contains(&ko), "multi-token Korean query found nothing");
}

#[test]
fn fts_candidates_filtered_by_fact_type() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let w = nodes::insert(&db, NewNode::new("b1", FactType::World, "migration notes")).unwrap();
    let o = nodes::insert(
        &db,
        NewNode::new("b1", FactType::Observation, "migration notes"),
    )
    .unwrap();

    let q = search::fts_query_string("migration");
    let all = search::fts_candidates_filtered(&db, "b1", &q, &[], 10).unwrap();
    assert_eq!(all.len(), 2, "empty fact_types means no filter");
    // bm25() is negative (lower = better), so a real score came back.
    assert!(all.iter().all(|(_, score)| *score < 0.0));

    let only_world =
        search::fts_candidates_filtered(&db, "b1", &q, &[FactType::World], 10).unwrap();
    assert_eq!(only_world.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![w]);

    let two = search::fts_candidates_filtered(
        &db,
        "b1",
        &q,
        &[FactType::World, FactType::Observation],
        10,
    )
    .unwrap();
    assert_eq!(two.len(), 2);
    assert!(two.iter().any(|(id, _)| *id == o));

    let none =
        search::fts_candidates_filtered(&db, "b1", &q, &[FactType::Experience], 10).unwrap();
    assert!(none.is_empty());
}

#[test]
fn hydrate_returns_rows_and_tags() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let mut node = NewNode::new("b1", FactType::Experience, "hydrate me");
    node.occurred_start = Some(1_700_000_000_000);
    node.mentioned_at = Some(1_700_000_100_000);
    node.context = Some("ctx");
    let id = nodes::insert(&db, node).unwrap();
    nodes::add_tags(&db, id, &["file:src/lib.rs", "session:abc"]).unwrap();
    let bare = nodes::insert(&db, NewNode::new("b1", FactType::World, "no tags")).unwrap();

    assert!(search::hydrate(&db, &[]).unwrap().is_empty());

    let rows = search::hydrate(&db, &[id, bare, 999_999]).unwrap();
    assert_eq!(rows.len(), 2, "unknown ids are silently absent");

    let tagged = rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(tagged.fact_type, FactType::Experience);
    assert_eq!(tagged.text, "hydrate me");
    assert_eq!(tagged.context.as_deref(), Some("ctx"));
    assert_eq!(tagged.occurred_start, Some(1_700_000_000_000));
    assert_eq!(tagged.mentioned_at, Some(1_700_000_100_000));
    let mut tags = tagged.tags.clone();
    tags.sort();
    assert_eq!(tags, vec!["file:src/lib.rs", "session:abc"]);
    assert!(!tagged.uuid.is_empty());

    let untagged = rows.iter().find(|r| r.id == bare).unwrap();
    assert!(untagged.tags.is_empty(), "no tags must be an empty vec, not [\"\"]");
}
