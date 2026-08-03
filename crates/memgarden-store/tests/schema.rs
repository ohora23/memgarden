use memgarden_core::error::Error;
use memgarden_core::types::FactType;
use memgarden_core::{EMBEDDING_DIM, EMBEDDING_MODEL_ID};
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
        .query_row(
            "SELECT count(*) FROM banks WHERE bank_id = 'legacy'",
            [],
            |r| r.get(0),
        )
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

    // fts_query_string quotes each term and appends '*', which matches via
    // the prefix='2 3 4' index. (Quoting neutralizes FTS5's bareword
    // operators; a quoted phrase still takes a prefix suffix.)
    let query = search::fts_query_string("데몬");
    assert_eq!(query, "\"데몬\"*");
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
    assert!(
        hits.contains(&en),
        "multi-token English query found nothing"
    );

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
    assert_eq!(
        only_world.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![w]
    );

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

    let none = search::fts_candidates_filtered(&db, "b1", &q, &[FactType::Experience], 10).unwrap();
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

    assert!(search::hydrate(&db, "b1", &[]).unwrap().is_empty());

    let rows = search::hydrate(&db, "b1", &[id, bare, 999_999]).unwrap();
    assert_eq!(rows.len(), 2, "unknown ids are silently absent");

    let tagged = rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(tagged.fact_type, FactType::Experience);
    assert_eq!(tagged.text, "hydrate me");
    assert_eq!(tagged.context.as_deref(), Some("ctx"));
    assert_eq!(tagged.occurred_start, Some(1_700_000_000_000));
    assert_eq!(tagged.mentioned_at, Some(1_700_000_100_000));
    assert_eq!(
        tagged.tags,
        vec!["file:src/lib.rs", "session:abc"],
        "tag order is ORDER BY tag, not insertion order"
    );
    assert!(!tagged.uuid.is_empty());

    let untagged = rows.iter().find(|r| r.id == bare).unwrap();
    assert!(
        untagged.tags.is_empty(),
        "no tags must be an empty vec, not [\"\"]"
    );
}

#[test]
fn hydrate_is_bank_scoped() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    banks::create(&db, "b2", None, None).unwrap();
    let mine = nodes::insert(&db, NewNode::new("b1", FactType::World, "mine")).unwrap();
    let theirs = nodes::insert(&db, NewNode::new("b2", FactType::World, "theirs")).unwrap();

    let rows = search::hydrate(&db, "b1", &[mine, theirs]).unwrap();
    assert_eq!(rows.len(), 1, "another bank's id must not hydrate");
    assert_eq!(rows[0].id, mine);
}

/// Review MEDIUM: tags are user-supplied text, so no character is safe as a
/// concatenation separator. `hydrate` reads them from their own query rather
/// than splitting a `group_concat`, which is what makes this hold.
#[test]
fn hydrate_does_not_split_a_tag_on_a_separator_character() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let id = nodes::insert(&db, NewNode::new("b1", FactType::World, "n")).unwrap();
    // U+001F (unit separator) and a comma: the two obvious separator picks.
    nodes::add_tags(&db, id, &["weird\u{1f}tag", "comma,tag"]).unwrap();

    let rows = search::hydrate(&db, "b1", &[id]).unwrap();
    assert_eq!(
        rows[0].tags,
        vec!["comma,tag", "weird\u{1f}tag"],
        "each tag must round-trip whole"
    );
}

// ---------------------------------------------------------------------------
// CE-7 / PR B5: entities, co-occurrence, links, graph expansion.
// ---------------------------------------------------------------------------

use memgarden_store::graph::{self, NewLink};

/// The v2-upgrade mirror of `migrate_upgrades_a_v1_database_in_place`: 0003
/// is the first migration that rewrites an existing table (`links`), so a
/// database left at v2 with data in it must come out at v3 intact.
#[test]
fn migrate_upgrades_a_v2_database_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.db");

    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(include_str!("../migrations/0001_init.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/0002_retain_jobs.sql"))
            .unwrap();
        for v in [1, 2] {
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, 0)",
                rusqlite::params![v],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute(
            "INSERT INTO banks (bank_id, created_at, updated_at) VALUES ('legacy', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_nodes (uuid, bank_id, fact_type, text, created_at, updated_at)
             VALUES ('u1', 'legacy', 'world', 'a', 0, 0), ('u2', 'legacy', 'world', 'b', 0, 0)",
            [],
        )
        .unwrap();
        // A pre-0003 link with an out-of-range weight: the rebuild clamps it
        // into the new CHECK rather than failing the migration.
        conn.execute(
            "INSERT INTO links (from_node_id, to_node_id, link_type, weight, created_at)
             VALUES (1, 2, 'semantic', 7.5, 0)",
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

    // 0003's table exists, the new entity columns exist...
    let cooc: i64 = conn
        .query_row("SELECT count(*) FROM entity_cooccurrences", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(cooc, 0);
    conn.query_row(
        "SELECT count(mention_count) + count(first_seen) + count(last_seen) FROM entities",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap();

    // ...and the pre-existing link survived, clamped.
    let (count, weight): (i64, f64) = conn
        .query_row("SELECT count(*), max(weight) FROM links", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        weight, 1.0,
        "an out-of-range weight is clamped, not dropped"
    );
}

#[test]
fn link_weight_check_rejects_out_of_range() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    let a = nodes::insert(&db, NewNode::new("b1", FactType::World, "a")).unwrap();
    let b = nodes::insert(&db, NewNode::new("b1", FactType::World, "b")).unwrap();

    for bad in [-0.1f64, 1.1] {
        let result = db.write(|tx| {
            tx.execute(
                "INSERT INTO links (from_node_id, to_node_id, link_type, weight, created_at)
                 VALUES (?1, ?2, 'semantic', ?3, 0)",
                rusqlite::params![a, b, bad],
            )
            .map_err(store_err)?;
            Ok(())
        });
        assert!(result.is_err(), "weight {bad} must be rejected (NIT 18)");
    }
    // insert_links clamps in Rust, so it never trips the CHECK.
    graph::insert_links(
        &db,
        &[NewLink {
            from_node_id: a,
            to_node_id: b,
            link_type: "semantic",
            weight: 42.0,
        }],
        0,
    )
    .unwrap();
    let w: f64 = db
        .read()
        .unwrap()
        .query_row("SELECT weight FROM links", [], |r| r.get(0))
        .unwrap();
    assert_eq!(w, 1.0);
}

#[test]
fn cooccurrence_check_rejects_non_canonical_order() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    db.write(|tx| {
        tx.execute(
            "INSERT INTO entities (bank_id, canonical_name, created_at) VALUES ('b1','a',0),('b1','b',0)",
            [],
        )
        .unwrap();
        Ok(())
    })
    .unwrap();

    let insert = |a: i64, b: i64| {
        db.write(|tx| {
            tx.execute(
                "INSERT INTO entity_cooccurrences
                   (entity_id_1, entity_id_2, cooccurrence_count, last_cooccurred)
                 VALUES (?1, ?2, 1, 0)",
                rusqlite::params![a, b],
            )
            .map_err(store_err)?;
            Ok(())
        })
    };
    assert!(insert(1, 2).is_ok());
    assert!(insert(2, 1).is_err(), "a > b must be rejected");
    assert!(insert(1, 1).is_err(), "a = b must be rejected");
}

fn seed_nodes(db: &Db, n: usize) -> Vec<i64> {
    banks::create(db, "b1", None, None).ok();
    (0..n)
        .map(|i| nodes::insert(db, NewNode::new("b1", FactType::World, &format!("n{i}"))).unwrap())
        .collect()
}

#[test]
fn write_entities_upserts_counts_attaches_and_pairs() {
    let db = Db::open_memory().unwrap();
    let ids = seed_nodes(&db, 2);

    let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // Per-fact dates (review MEDIUM 3): fact 0 is a day older than fact 1.
    let batch = vec![
        (ids[0], names(&["ollama", "qwen3"]), 1_000i64),
        (ids[1], names(&["ollama"]), 2_000i64),
    ];
    let map = graph::write_entities(&db, "b1", &batch, 500).unwrap();
    assert_eq!(map.len(), 2);

    let conn = db.read().unwrap();
    let (count, first, last): (i64, i64, i64) = conn
        .query_row(
            "SELECT mention_count, first_seen, last_seen FROM entities WHERE canonical_name='ollama'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(count, 2, "two mentions in one batch count twice");
    assert_eq!(
        (first, last),
        (1_000, 2_000),
        "first_seen/last_seen come from each fact's own date, not one chunk stamp"
    );
    // entity_type is deliberately never persisted (legacy hardcodes CONCEPT).
    let typed: i64 = conn
        .query_row("SELECT count(entity_type) FROM entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(typed, 0);

    let attached: i64 = conn
        .query_row("SELECT count(*) FROM node_entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(attached, 3);

    // One pair, canonically ordered, from the fact naming both.
    let (e1, e2, cooc): (i64, i64, i64) = conn
        .query_row(
            "SELECT entity_id_1, entity_id_2, cooccurrence_count FROM entity_cooccurrences",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(e1 < e2);
    assert_eq!(cooc, 1);
    drop(conn);

    // Second batch: counts accumulate rather than conflicting.
    let later: Vec<_> = batch
        .iter()
        .map(|(id, names, seen)| (*id, names.clone(), seen + 5_000))
        .collect();
    graph::write_entities(&db, "b1", &later, 600).unwrap();
    let conn = db.read().unwrap();
    let (count, first, last): (i64, i64, i64) = conn
        .query_row(
            "SELECT mention_count, first_seen, last_seen FROM entities WHERE canonical_name='ollama'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(count, 4);
    assert_eq!(
        (first, last),
        (1_000, 7_000),
        "first_seen sticks, last_seen advances"
    );
    let cooc: i64 = conn
        .query_row(
            "SELECT cooccurrence_count FROM entity_cooccurrences",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cooc, 2, "ON CONFLICT adds the batch count");
}

/// Korean names go through `normalize` (trim + lowercase) unchanged, and must
/// survive the UNIQUE key, the co-occurrence join and the read-back.
#[test]
fn korean_entity_names_round_trip() {
    let db = Db::open_memory().unwrap();
    let ids = seed_nodes(&db, 1);
    let batch = vec![(
        ids[0],
        vec!["메모리 시스템".to_string(), "제트슨 자비에".to_string()],
        1_000i64,
    )];
    graph::write_entities(&db, "b1", &batch, 500).unwrap();

    let ctx = graph::load_resolution_context(&db, "b1").unwrap();
    let mut got: Vec<&str> = ctx
        .candidates
        .iter()
        .map(|c| c.canonical_name.as_str())
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec!["메모리 시스템", "제트슨 자비에"]);
    // Each is recorded as the other's co-occurrent, both directions.
    let by_name = |n: &str| {
        ctx.candidates
            .iter()
            .find(|c| c.canonical_name == n)
            .unwrap()
            .id
    };
    assert!(ctx.cooccurring[&by_name("메모리 시스템")].contains("제트슨 자비에"));
    assert!(ctx.cooccurring[&by_name("제트슨 자비에")].contains("메모리 시스템"));
}

#[test]
fn expand_walks_one_hop_in_both_directions_and_excludes_seeds() {
    let db = Db::open_memory().unwrap();
    let ids = seed_nodes(&db, 4);
    let (seed, out, incoming, far) = (ids[0], ids[1], ids[2], ids[3]);

    graph::insert_links(
        &db,
        &[
            NewLink {
                from_node_id: seed,
                to_node_id: out,
                link_type: "semantic",
                weight: 0.9,
            },
            NewLink {
                from_node_id: incoming,
                to_node_id: seed,
                link_type: "caused_by",
                weight: 1.0,
            },
            // Two hops away: reachable from `out`, not from `seed`.
            NewLink {
                from_node_id: out,
                to_node_id: far,
                link_type: "semantic",
                weight: 0.8,
            },
        ],
        0,
    )
    .unwrap();
    // Shared entity between the seed and `far` — the node_entities path.
    graph::write_entities(
        &db,
        "b1",
        &[
            (seed, vec!["ollama".to_string()], 0),
            (far, vec!["ollama".to_string()], 0),
        ],
        0,
    )
    .unwrap();

    let (links, shared) = graph::expand(&db, "b1", &[seed], 100).unwrap();
    let mut reached: Vec<i64> = links.iter().map(|n| n.node_id).collect();
    reached.sort_unstable();
    assert_eq!(
        reached,
        vec![out, incoming],
        "both directions, one hop, no seed"
    );
    assert!(
        !reached.contains(&far),
        "two hops away must not appear via links"
    );
    assert_eq!(shared, vec![(far, 1)], "entity co-membership reaches `far`");

    // Another bank's node must never be expanded into.
    banks::create(&db, "b2", None, None).unwrap();
    let (links, shared) = graph::expand(&db, "b2", &[seed], 100).unwrap();
    assert!(links.is_empty() && shared.is_empty());
}

#[test]
fn graph_view_returns_nodes_entities_and_only_internal_edges() {
    let db = Db::open_memory().unwrap();
    let ids = seed_nodes(&db, 3);
    graph::insert_links(
        &db,
        &[NewLink {
            from_node_id: ids[0],
            to_node_id: ids[1],
            link_type: "temporal",
            weight: 0.5,
        }],
        0,
    )
    .unwrap();
    graph::write_entities(&db, "b1", &[(ids[0], vec!["ollama".to_string()], 0)], 0).unwrap();

    let (nodes_out, edges) = graph::graph_view(&db, "b1", 100, &[], None).unwrap();
    assert_eq!(nodes_out.len(), 3);
    let first = nodes_out.iter().find(|n| n.id == ids[0]).unwrap();
    assert_eq!(first.entities, vec!["ollama".to_string()]);
    assert_eq!(edges.len(), 1);

    // `limit` is newest-first, so a limit of 1 keeps the last node — and the
    // edge, whose other endpoint fell out of the set, must not come back.
    let (nodes_out, edges) = graph::graph_view(&db, "b1", 1, &[], None).unwrap();
    assert_eq!(nodes_out.len(), 1);
    assert_eq!(nodes_out[0].id, ids[2]);
    assert!(edges.is_empty(), "no dangling edges");
}

/// Review HIGH-2: without a per-entity cap, one hub entity makes the
/// co-membership self-join `seeds x |entity|` — the outer LIMIT lands after
/// the GROUP BY, so it bounds the output, not the work. `normalize()` merges
/// name variants into exactly such hot buckets.
#[test]
fn expand_ignores_a_hub_entity_past_the_fanout_cap() {
    let db = Db::open_memory().unwrap();
    let ids = seed_nodes(&db, 4);
    let (seed, rare_partner) = (ids[0], ids[1]);

    // "hub" is on every node; "rare" is on two. Both reach `rare_partner`,
    // only "rare" should count.
    let batch: Vec<memgarden_store::graph::EntityMentions> = ids
        .iter()
        .map(|id| {
            let mut names = vec!["hub".to_string()];
            if *id == seed || *id == rare_partner {
                names.push("rare".to_string());
            }
            (*id, names, 0i64)
        })
        .collect();
    graph::write_entities(&db, "b1", &batch, 0).unwrap();

    // Under the cap: the hub still expands, so every other node is reached.
    let (_, shared) = graph::expand(&db, "b1", &[seed], 100).unwrap();
    assert_eq!(shared.len(), 3, "a small entity is a real edge");

    // Push "hub" past MAX_ENTITY_FANOUT and it stops being an edge; "rare"
    // is untouched, so exactly its one partner survives.
    db.write(|tx| {
        tx.execute(
            "UPDATE entities SET mention_count = ?1 WHERE canonical_name = 'hub'",
            rusqlite::params![memgarden_store::graph::MAX_ENTITY_FANOUT + 1],
        )
        .map_err(store_err)?;
        Ok(())
    })
    .unwrap();
    let (_, shared) = graph::expand(&db, "b1", &[seed], 100).unwrap();
    assert_eq!(
        shared,
        vec![(rare_partner, 1)],
        "a hub entity past the cap connects nothing"
    );

    // Exactly at the cap it still counts — the gate is `<=`.
    db.write(|tx| {
        tx.execute(
            "UPDATE entities SET mention_count = ?1 WHERE canonical_name = 'hub'",
            rusqlite::params![memgarden_store::graph::MAX_ENTITY_FANOUT],
        )
        .map_err(store_err)?;
        Ok(())
    })
    .unwrap();
    let (_, shared) = graph::expand(&db, "b1", &[seed], 100).unwrap();
    assert_eq!(shared.len(), 3);
}

/// Security MED-6: the co-occurrence load keeps only each entity's strongest
/// partners, so a bank whose entity pairs grow quadratically does not make
/// every retain chunk load the whole table.
#[test]
fn resolution_context_bounds_cooccurrence_partners() {
    let db = Db::open_memory().unwrap();
    let ids = seed_nodes(&db, 1);
    let cap = memgarden_store::graph::MAX_COOCCURRENCE_PARTNERS;

    // One fact naming the hub plus `cap + 20` partners: every pair is
    // recorded, but the loaded view keeps `cap` of them per entity.
    let mut names = vec!["hub".to_string()];
    names.extend((0..cap + 20).map(|i| format!("partner {i:03}")));
    graph::write_entities(&db, "b1", &[(ids[0], names, 0)], 0).unwrap();

    let ctx = graph::load_resolution_context(&db, "b1").unwrap();
    let hub = ctx
        .candidates
        .iter()
        .find(|c| c.canonical_name == "hub")
        .unwrap();
    assert_eq!(
        ctx.cooccurring[&hub.id].len(),
        cap,
        "the hub's partner set is capped"
    );
    // Nothing is lost from the table itself — only from the loaded view.
    let stored: i64 = db
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM entity_cooccurrences", [], |r| {
            r.get(0)
        })
        .unwrap();
    let n = cap + 21;
    assert_eq!(stored as usize, n * (n - 1) / 2);
}

/// Security MED-5: candidates are bounded and ordered newest-first, so an old
/// bank cannot make resolution unboundedly expensive.
#[test]
fn resolution_context_keeps_the_most_recently_seen_candidates() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();
    db.write(|tx| {
        for i in 0..40i64 {
            tx.execute(
                "INSERT INTO entities (bank_id, canonical_name, created_at, last_seen)
                 VALUES ('b1', ?1, 0, ?2)",
                rusqlite::params![format!("entity {i:03}"), i],
            )
            .map_err(store_err)?;
        }
        Ok(())
    })
    .unwrap();

    let ctx = graph::load_resolution_context(&db, "b1").unwrap();
    assert_eq!(ctx.candidates.len(), 40, "under the cap, everything loads");
    assert_eq!(
        ctx.candidates[0].canonical_name, "entity 039",
        "ordered last_seen DESC, so the cap drops the stalest"
    );
}

// --- 0004 (CE-9a): consolidation storage --------------------------------

/// A fresh database lands on v4 with every 0004 object in place.
#[test]
fn fresh_database_has_the_0004_consolidation_schema() {
    let db = Db::open_memory().unwrap();
    let conn = db.read().unwrap();

    // The `LATEST_VERSION` pin lives with the newest migration's test — see
    // `fresh_database_has_the_0007_sessions_schema`.
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, memgarden_store::LATEST_VERSION);

    for table in ["node_sources", "consolidation_runs"] {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table} exists and is empty");
    }
    // The new column defaults to 0 for every node, which `proof_norm` reads
    // as neutral.
    banks::create(&db, "b1", None, None).unwrap();
    let id = nodes::insert(&db, NewNode::new("b1", FactType::World, "a")).unwrap();
    assert_eq!(
        memgarden_store::consolidate::proof_count(&db, id).unwrap(),
        0
    );
    // The run ledger's status CHECK is real.
    let bad = db.write(|tx| {
        tx.execute(
            "INSERT INTO consolidation_runs (bank_id, status, started_at) VALUES ('b1', 'nope', 0)",
            [],
        )
        .map_err(store_err)?;
        Ok(())
    });
    assert!(bad.is_err(), "status CHECK rejects an unknown state");
}

/// The v3-upgrade mirror of the v1/v2 tests: a populated v3 database must
/// come out at v4 with its rows intact and `proof_count` backfilled to the
/// DDL default.
#[test]
fn migrate_upgrades_a_v3_database_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.db");

    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in [
            include_str!("../migrations/0001_init.sql"),
            include_str!("../migrations/0002_retain_jobs.sql"),
            include_str!("../migrations/0003_entities_graph.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        for v in [1, 2, 3] {
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, 0)",
                rusqlite::params![v],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute(
            "INSERT INTO banks (bank_id, created_at, updated_at) VALUES ('legacy', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_nodes (uuid, bank_id, fact_type, text, created_at, updated_at)
             VALUES ('u1', 'legacy', 'world', 'a fact', 0, 0),
                    ('u2', 'legacy', 'observation', 'an observation', 0, 0)",
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

    // Pre-existing rows survive and carry the new column at its default.
    let (nodes_n, proof_sum): (i64, i64) = conn
        .query_row(
            "SELECT count(*), sum(proof_count) FROM memory_nodes",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(nodes_n, 2);
    assert_eq!(proof_sum, 0, "ADD COLUMN backfills the DDL default");
    // And the new tables are usable against the pre-existing rows: the
    // observation (id 2) can be given the fact (id 1) as provenance.
    drop(conn);
    db.write(|tx| {
        tx.execute(
            "INSERT INTO node_sources (observation_id, source_id, created_at) VALUES (2, 1, 0)",
            [],
        )
        .map_err(store_err)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        memgarden_store::consolidate::sources_of(&db, 2).unwrap(),
        vec![1]
    );
}

// --- 0005 (AX-1): embedding_model, vector-space versioning ---------------

/// A fresh database has 0005's column, and the write paths stamp the
/// producer. (The `LATEST_VERSION` pin moved to 0007's test — the convention
/// AX-1 set when it took it off 0004's: the single absolute lives with the
/// newest migration and every other test asserts the derived constant.)
#[test]
fn fresh_database_has_the_0005_embedding_model_column() {
    let db = Db::open_memory().unwrap();

    {
        let conn = db.read().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, memgarden_store::LATEST_VERSION);
    }

    banks::create(&db, "b1", None, None).unwrap();
    let id = nodes::insert(&db, NewNode::new("b1", FactType::World, "a fact")).unwrap();
    assert_eq!(
        model_of(&db, id),
        None,
        "an unembedded node has no producer to name"
    );

    nodes::set_embedding(&db, id, "b1", &vec![0.1f32; EMBEDDING_DIM]).unwrap();
    assert_eq!(model_of(&db, id).as_deref(), Some(EMBEDDING_MODEL_ID));

    // The batch path and the consolidation path stamp it too — neither goes
    // through `set_embedding`.
    let id2 = nodes::insert(&db, NewNode::new("b1", FactType::World, "b fact")).unwrap();
    nodes::set_embeddings_batch(&db, &[(id2, "b1".to_string(), vec![0.2f32; EMBEDDING_DIM])])
        .unwrap();
    assert_eq!(model_of(&db, id2).as_deref(), Some(EMBEDDING_MODEL_ID));

    let obs = memgarden_store::consolidate::insert_observation(
        &db,
        "b1",
        "an observation",
        &vec![0.3f32; EMBEDDING_DIM],
        &[],
    )
    .unwrap();
    assert_eq!(model_of(&db, obs).as_deref(), Some(EMBEDDING_MODEL_ID));
}

/// SQL cannot reference a Rust const, so 0005's backfill hard-codes the id.
/// Pinned against a test-local copy, NOT against the live const: 0005
/// backfills the producer that was active when 0005 was written, and that is
/// historical. Editing the literal to follow a bumped `EMBEDDING_MODEL_ID`
/// would tag a v4 database's old-model vectors with the new id — the silent
/// mislabeling AX-1 exists to prevent, arriving through its own guard.
const ID_AT_0005: &str = "fastembed:BAAI/bge-small-en-v1.5";

#[test]
fn backfill_literal_matches_the_active_model_id() {
    let sql = include_str!("../migrations/0005_embedding_model.sql");
    assert!(
        sql.contains(&format!("SET embedding_model = '{ID_AT_0005}'")),
        "0005 backfills the producer active when it was written — freeze this \
         literal. If EMBEDDING_MODEL_ID changed, add a new migration for the \
         transition and pin that one."
    );
    // Today they are the same string; the day they diverge, the line above
    // must not move and this one must.
    assert_eq!(ID_AT_0005, EMBEDDING_MODEL_ID);
}

/// The v4-upgrade mirror of the v1/v2/v3 tests. A populated v4 database must
/// come out at v5 with rows intact and **every already-embedded row
/// backfilled** — see 0005's comment: leaving them NULL would drop them out
/// of the dense arm on upgrade.
#[test]
fn migrate_upgrades_a_v4_database_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v4.db");

    {
        // sqlite-vec is registered as a process-global auto-extension by
        // `Db::open`, and 0001's `CREATE VIRTUAL TABLE vec_nodes USING vec0`
        // needs it. The raw connection below predates any `Db` in this test,
        // so force the registration rather than depending on another test in
        // the binary having happened to run first.
        drop(Db::open_memory().unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in [
            include_str!("../migrations/0001_init.sql"),
            include_str!("../migrations/0002_retain_jobs.sql"),
            include_str!("../migrations/0003_entities_graph.sql"),
            include_str!("../migrations/0004_consolidation.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        for v in [1, 2, 3, 4] {
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, 0)",
                rusqlite::params![v],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 4).unwrap();
        conn.execute(
            "INSERT INTO banks (bank_id, created_at, updated_at) VALUES ('legacy', 0, 0)",
            [],
        )
        .unwrap();
        // One embedded row (the backfill's target) and one still on the
        // backlog (which must stay NULL — it has no producer yet).
        let blob = vecblob::encode(&vec![0.5f32; EMBEDDING_DIM]).unwrap();
        conn.execute(
            "INSERT INTO memory_nodes (uuid, bank_id, fact_type, text, embedding, created_at, updated_at)
             VALUES ('u1', 'legacy', 'world', 'an embedded fact', ?1, 0, 0)",
            rusqlite::params![blob],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_nodes (uuid, bank_id, fact_type, text, created_at, updated_at)
             VALUES ('u2', 'legacy', 'world', 'a pending fact', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vec_nodes (rowid, bank_id, embedding) VALUES (1, 'legacy', ?1)",
            rusqlite::params![blob],
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    {
        let conn = db.read().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, memgarden_store::LATEST_VERSION);
        let n: i64 = conn
            .query_row("SELECT count(*) FROM memory_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "pre-existing rows survive");
    }

    assert_eq!(
        model_of(&db, 1).as_deref(),
        Some(EMBEDDING_MODEL_ID),
        "an existing vector was produced by this code path, so it is tagged as such"
    );
    assert_eq!(model_of(&db, 2), None, "no vector, no producer");

    // The backfill is what keeps the upgraded row in the dense arm.
    let hits = search::knn(&db, "legacy", &vec![0.5f32; EMBEDDING_DIM], 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, 1);
}

/// **The AX-1 promise, end to end**: a vector from another producer is
/// invisible to dense comparison and fully visible to BM25. That asymmetry is
/// the migration strategy — a mixed bank degrades to keyword recall for the
/// foreign rows instead of returning meaningless cosine distances, and it
/// costs nothing because recall is already hybrid.
#[test]
fn a_foreign_model_vector_is_absent_from_knn_but_present_in_fts() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();

    let ours = nodes::insert(
        &db,
        NewNode::new("b1", FactType::World, "the retain worker transaction"),
    )
    .unwrap();
    let foreign = nodes::insert(
        &db,
        NewNode::new("b1", FactType::World, "the retain worker deadline"),
    )
    .unwrap();
    let untagged = nodes::insert(
        &db,
        NewNode::new("b1", FactType::World, "the retain worker chunker"),
    )
    .unwrap();

    // All three get a real vector through the normal path, so the only thing
    // that differs is the tag.
    let v = vec![0.5f32; EMBEDDING_DIM];
    for id in [ours, foreign, untagged] {
        nodes::set_embedding(&db, id, "b1", &v).unwrap();
    }
    db.write(|tx| {
        // A legacy import's two shapes: a *named* other producer, and the
        // untagged NULL that jcode's LEGACY_EMBEDDING_MODEL convention leaves
        // behind. Neither is comparable with ours.
        tx.execute(
            "UPDATE memory_nodes SET embedding_model = 'sentence-transformers:BAAI/bge-small-en-v1.5'
             WHERE id = ?1",
            rusqlite::params![foreign],
        )
        .unwrap();
        tx.execute(
            "UPDATE memory_nodes SET embedding_model = NULL WHERE id = ?1",
            rusqlite::params![untagged],
        )
        .unwrap();
        Ok(())
    })
    .unwrap();

    // Dense: only ours, even though all three sit in `vec_nodes` at distance 0.
    let hits: Vec<i64> = search::knn(&db, "b1", &v, 10)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(hits, vec![ours], "dense compares within one space only");

    // BM25: all three, unfiltered. This is the reachability guarantee.
    let q = search::fts_query_string("retain worker");
    let mut fts = search::fts_candidates(&db, "b1", &q, 10).unwrap();
    fts.sort_unstable();
    assert_eq!(fts, vec![ours, foreign, untagged]);

    // And hydration does not filter either — a BM25 hit must be returnable.
    let mut hydrated: Vec<i64> = search::hydrate(&db, "b1", &fts)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    hydrated.sort_unstable();
    assert_eq!(hydrated, vec![ours, foreign, untagged]);
}

/// The dedup probe is the other cosine consumer (CE-9a's 0.97 threshold), and
/// it reads `memory_nodes.embedding` directly rather than through `vec_nodes`.
#[test]
fn the_dedup_probe_skips_foreign_model_observations() {
    let db = Db::open_memory().unwrap();
    banks::create(&db, "b1", None, None).unwrap();

    let v = vec![0.25f32; EMBEDDING_DIM];
    let ours =
        memgarden_store::consolidate::insert_observation(&db, "b1", "ours", &v, &[]).unwrap();
    let foreign =
        memgarden_store::consolidate::insert_observation(&db, "b1", "foreign", &v, &[]).unwrap();
    db.write(|tx| {
        tx.execute(
            "UPDATE memory_nodes SET embedding_model = 'legacy:whatever' WHERE id = ?1",
            rusqlite::params![foreign],
        )
        .unwrap();
        Ok(())
    })
    .unwrap();

    let probe = memgarden_store::consolidate::observation_vectors(&db, "b1", -1).unwrap();
    let ids: Vec<i64> = probe.iter().map(|o| o.id).collect();
    assert_eq!(ids, vec![ours]);
}

/// Reads `embedding_model` off a node — the column is intentionally not on
/// `MemoryNode` (nothing in the API surface needs it; it is a storage-layer
/// invariant, and adding it to the struct would put it in every response).
fn model_of(db: &Db, id: i64) -> Option<String> {
    db.read()
        .unwrap()
        .query_row(
            "SELECT embedding_model FROM memory_nodes WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap()
}

// --- 0006 (CE-10): mental models ----------------------------------------

/// A fresh database lands on v6 with every 0006 object in place and its
/// CHECKs real.
#[test]
fn fresh_database_has_the_0006_mental_model_schema() {
    let db = Db::open_memory().unwrap();
    let conn = db.read().unwrap();

    // The `LATEST_VERSION` pin moved on to 0007's test — see
    // `fresh_database_has_the_0007_sessions_schema`.
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, memgarden_store::LATEST_VERSION);

    for table in ["mental_models", "vec_mental_models"] {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table} exists and is empty");
    }
    drop(conn);

    banks::create(&db, "b1", None, None).unwrap();
    // The JSON CHECK on the audit column is real...
    let bad_json = db.write(|tx| {
        tx.execute(
            "INSERT INTO mental_models (id, bank_id, name, reflect_response, created_at)
             VALUES ('mm-1', 'b1', 'n', 'not json', 0)",
            [],
        )
        .map_err(store_err)?;
        Ok(())
    });
    assert!(bad_json.is_err(), "reflect_response must be valid JSON");

    // ...as is the embedding length CHECK (1536 bytes = 384 f32s).
    let bad_vec = db.write(|tx| {
        tx.execute(
            "INSERT INTO mental_models (id, bank_id, name, embedding, created_at)
             VALUES ('mm-2', 'b1', 'n', X'0000', 0)",
            [],
        )
        .map_err(store_err)?;
        Ok(())
    });
    assert!(bad_vec.is_err(), "embedding must be 1536 bytes");

    // ...and so is the FK to banks.
    let bad_bank = db.write(|tx| {
        tx.execute(
            "INSERT INTO mental_models (id, bank_id, name, created_at)
             VALUES ('mm-3', 'nope', 'n', 0)",
            [],
        )
        .map_err(store_err)?;
        Ok(())
    });
    assert!(bad_bank.is_err(), "bank_id must reference a real bank");
}

/// The v5-upgrade mirror of the v1/v2/v3/v4 tests: a **populated** v5 database
/// must come out at v6 with its rows intact and the new objects usable.
#[test]
fn migrate_upgrades_a_v5_database_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v5.db");

    {
        // 0001's `CREATE VIRTUAL TABLE vec_nodes USING vec0` needs the
        // process-global auto-extension `Db::open` registers; the raw
        // connection below may be the first in this binary.
        drop(Db::open_memory().unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in [
            include_str!("../migrations/0001_init.sql"),
            include_str!("../migrations/0002_retain_jobs.sql"),
            include_str!("../migrations/0003_entities_graph.sql"),
            include_str!("../migrations/0004_consolidation.sql"),
            include_str!("../migrations/0005_embedding_model.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        for v in [1, 2, 3, 4, 5] {
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, 0)",
                rusqlite::params![v],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 5).unwrap();
        conn.execute(
            "INSERT INTO banks (bank_id, created_at, updated_at) VALUES ('legacy', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_nodes (uuid, bank_id, fact_type, text, created_at, updated_at)
             VALUES ('u1', 'legacy', 'world', 'a fact', 0, 0)",
            [],
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    {
        let conn = db.read().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, memgarden_store::LATEST_VERSION);
        let nodes_n: i64 = conn
            .query_row("SELECT count(*) FROM memory_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(nodes_n, 1, "the pre-existing row survives the upgrade");
    }

    // And the new objects work against the pre-existing bank.
    use memgarden_store::mental_models::{self as mm, NewMentalModel};
    let vector = vec![0.5f32; EMBEDDING_DIM];
    mm::insert(
        &db,
        &NewMentalModel {
            id: "mm-upgraded",
            bank_id: "legacy",
            name: "after the upgrade",
            source_query: None,
            content: "content",
            max_tokens: None,
            trigger: None,
        },
        Some(&vector),
    )
    .unwrap();
    assert_eq!(mm::knn(&db, "legacy", &vector, 5).unwrap().len(), 1);
}

// --- 0007 (HK-1a): Claude Code session and turn state --------------------

/// A fresh database lands on v7 with the `sessions` table in place, and the
/// three DDL choices that are easy to lose in a later edit — `STRICT`,
/// `WITHOUT ROWID`, and the `last_seen_at DESC` index — are each pinned by
/// something that fails if the clause is deleted.
///
/// This test carries the single **absolute** `LATEST_VERSION` pin; every
/// other migration test asserts the derived constant.
#[test]
fn fresh_database_has_the_0007_sessions_schema() {
    let db = Db::open_memory().unwrap();
    let conn = db.read().unwrap();

    assert_eq!(memgarden_store::LATEST_VERSION, 7);
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 7);

    let n: i64 = conn
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "sessions exists and is empty");

    // WITHOUT ROWID: the table has no rowid to select. Asserting the DDL
    // text would pass on a table that merely mentions the words.
    assert!(
        conn.query_row("SELECT rowid FROM sessions LIMIT 1", [], |_| Ok(()))
            .is_err(),
        "sessions must be WITHOUT ROWID"
    );

    // The index the dashboard list order and `sessions::gc` both ride on.
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_sessions_last_seen'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1);
    drop(conn);

    banks::create(&db, "b1", None, None).unwrap();

    // STRICT: an INTEGER column refuses text SQLite would otherwise coerce.
    let loose = db.write(|tx| {
        tx.execute(
            "INSERT INTO sessions (bank_id, session_id, turns, started_at, last_seen_at)
             VALUES ('b1', 's1', 'not a number', 0, 0)",
            [],
        )
        .map_err(store_err)?;
        Ok(())
    });
    assert!(loose.is_err(), "sessions must be STRICT");

    // The FK to banks is real.
    let bad_bank = db.write(|tx| {
        tx.execute(
            "INSERT INTO sessions (bank_id, session_id, started_at, last_seen_at)
             VALUES ('nope', 's1', 0, 0)",
            [],
        )
        .map_err(store_err)?;
        Ok(())
    });
    assert!(bad_bank.is_err(), "bank_id must reference a real bank");
}

/// The v6-upgrade mirror of the v1/v2/v3/v4/v5 tests: a **populated** v6
/// database comes out at v7 with its rows intact and `sessions` usable.
#[test]
fn migrate_upgrades_a_v6_database_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.db");

    {
        // 0001's `CREATE VIRTUAL TABLE vec_nodes USING vec0` needs the
        // process-global auto-extension `Db::open` registers.
        drop(Db::open_memory().unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        for sql in [
            include_str!("../migrations/0001_init.sql"),
            include_str!("../migrations/0002_retain_jobs.sql"),
            include_str!("../migrations/0003_entities_graph.sql"),
            include_str!("../migrations/0004_consolidation.sql"),
            include_str!("../migrations/0005_embedding_model.sql"),
            include_str!("../migrations/0006_mental_models.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        for v in [1, 2, 3, 4, 5, 6] {
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, 0)",
                rusqlite::params![v],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 6).unwrap();
        conn.execute(
            "INSERT INTO banks (bank_id, created_at, updated_at) VALUES ('legacy', 0, 0)",
            [],
        )
        .unwrap();
    }

    let db = Db::open(&path).unwrap();
    {
        let conn = db.read().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, memgarden_store::LATEST_VERSION);
    }

    // And the new table works against the pre-existing bank.
    use memgarden_store::sessions::{self, SessionUpdate};
    let row = sessions::upsert(
        &db,
        "legacy",
        &SessionUpdate {
            session_id: "after-the-upgrade",
            byte_offset: Some(42),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(row.byte_offset, 42);
    assert_eq!(sessions::list(&db, "legacy", 10, false).unwrap().len(), 1);
}
