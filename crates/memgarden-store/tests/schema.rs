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
    assert_eq!(version, 1);
    let count: i64 = conn
        .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 1,
        "schema_migrations must log each migration exactly once"
    );
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

    banks::update(&db, "b1", Some("new mission"), Some(r#"{"k":"v"}"#)).unwrap();
    let updated = banks::get(&db, "b1").unwrap().unwrap();
    assert_eq!(updated.mission.as_deref(), Some("new mission"));
    assert_eq!(updated.disposition.as_deref(), Some(r#"{"k":"v"}"#));

    banks::delete(&db, "b1").unwrap();
    assert!(banks::get(&db, "b1").unwrap().is_none());

    assert!(banks::update(&db, "missing", None, None).is_err());
}
