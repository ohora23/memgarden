//! Mental-model storage (CE-10): CRUD over `mental_models` plus KNN over the
//! `vec_mental_models` vector space.
//!
//! Legacy: `engine/memory_engine.py:11073-11077` (list), `:11263-11300`
//! (create), `:12688-12734` (row mapping).

use rusqlite::{OptionalExtension, Row, params};
use uuid::Uuid;

use memgarden_core::EMBEDDING_MODEL_ID;
use memgarden_core::error::Result;

use crate::{Db, store_err, vecblob};

const SELECT_COLUMNS: &str = "id, bank_id, name, source_query, content, reflect_response, \
     max_tokens, trigger, last_refreshed_at, created_at";

#[derive(Debug, Clone, PartialEq)]
pub struct MentalModel {
    pub id: String,
    pub bank_id: String,
    pub name: String,
    pub source_query: Option<String>,
    pub content: String,
    /// Raw JSON text; the daemon hands it back to callers as a parsed value.
    pub reflect_response: Option<String>,
    pub max_tokens: Option<i64>,
    /// 5-field cron expression, UTC — see `memgardend::mental::cron`.
    pub trigger: Option<String>,
    pub last_refreshed_at: Option<i64>,
    pub created_at: i64,
}

fn from_row(r: &Row) -> rusqlite::Result<MentalModel> {
    Ok(MentalModel {
        id: r.get(0)?,
        bank_id: r.get(1)?,
        name: r.get(2)?,
        source_query: r.get(3)?,
        content: r.get(4)?,
        reflect_response: r.get(5)?,
        max_tokens: r.get(6)?,
        trigger: r.get(7)?,
        last_refreshed_at: r.get(8)?,
        created_at: r.get(9)?,
    })
}

/// `"mm-<uuid4hex>"` — legacy's id format verbatim (`memory_engine.py:11269`),
/// kept because MG-1 (Phase D) imports legacy rows by this id and a different
/// shape would make imported and native models distinguishable by accident.
pub fn new_id() -> String {
    format!("mm-{}", Uuid::new_v4().simple())
}

#[derive(Debug, Clone)]
pub struct NewMentalModel<'a> {
    pub id: &'a str,
    pub bank_id: &'a str,
    pub name: &'a str,
    pub source_query: Option<&'a str>,
    pub content: &'a str,
    pub max_tokens: Option<i64>,
    pub trigger: Option<&'a str>,
}

/// Fields to overwrite. `None` leaves the column untouched — the UPDATE below
/// is one COALESCE'd statement rather than a dynamically built SET list, so
/// there is exactly one statement shape whatever the caller changes.
///
/// The consequence, deliberate: a nullable column cannot be *cleared* through
/// this path, only overwritten. Nothing in CE-10 clears one, and paying for it
/// would mean either dynamic SQL or a sentinel value.
#[derive(Debug, Clone, Default)]
pub struct Patch<'a> {
    pub name: Option<&'a str>,
    pub source_query: Option<&'a str>,
    pub content: Option<&'a str>,
    pub max_tokens: Option<i64>,
    pub trigger: Option<&'a str>,
    /// JSON text; the caller has already serialized it.
    pub reflect_response: Option<&'a str>,
    pub last_refreshed_at: Option<i64>,
}

/// Inserts one mental model, optionally with its vector.
///
/// `embedding` is `None` when the embedder has not finished loading. The row
/// is then simply absent from `vec_mental_models` — invisible to KNN, fully
/// visible to every other read — and the next write with a vector fixes it.
/// There is no backlog worker for mental models (see the design note).
pub fn insert(db: &Db, new: &NewMentalModel, embedding: Option<&[f32]>) -> Result<MentalModel> {
    let now = memgarden_core::now_ms();
    let blob = embedding.map(vecblob::encode).transpose()?;
    db.write(|tx| {
        tx.execute(
            "INSERT INTO mental_models
             (id, bank_id, name, source_query, content, max_tokens, trigger,
              embedding, embedding_model, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                new.id,
                new.bank_id,
                new.name,
                new.source_query,
                new.content,
                new.max_tokens,
                new.trigger,
                blob,
                blob.as_ref().map(|_| EMBEDDING_MODEL_ID),
                now,
            ],
        )
        .map_err(store_err)?;
        if let Some(blob) = &blob {
            write_vec(tx, tx.last_insert_rowid(), new.bank_id, blob)?;
        }
        Ok(())
    })?;
    get(db, new.bank_id, new.id)?
        .ok_or_else(|| memgarden_core::Error::Storage("mental model vanished after insert".into()))
}

/// Applies `patch`, and — when `embedding` is `Some` — rewrites the vector in
/// the **same transaction** as the text it was computed from. The two can
/// therefore never disagree, which is the whole point: a `vec_mental_models`
/// row left over from the previous content is a KNN hit that returns text the
/// query never matched.
///
/// Returns the number of rows changed (0 = no such model in this bank).
pub fn update(
    db: &Db,
    bank_id: &str,
    id: &str,
    patch: &Patch,
    embedding: Option<&[f32]>,
) -> Result<usize> {
    let blob = embedding.map(vecblob::encode).transpose()?;
    db.write(|tx| {
        let changed = tx
            .execute(
                "UPDATE mental_models SET
                   name              = COALESCE(?3, name),
                   source_query      = COALESCE(?4, source_query),
                   content           = COALESCE(?5, content),
                   max_tokens        = COALESCE(?6, max_tokens),
                   trigger           = COALESCE(?7, trigger),
                   reflect_response  = COALESCE(?8, reflect_response),
                   last_refreshed_at = COALESCE(?9, last_refreshed_at),
                   embedding         = COALESCE(?10, embedding),
                   embedding_model   = COALESCE(?11, embedding_model)
                 WHERE bank_id = ?1 AND id = ?2",
                params![
                    bank_id,
                    id,
                    patch.name,
                    patch.source_query,
                    patch.content,
                    patch.max_tokens,
                    patch.trigger,
                    patch.reflect_response,
                    patch.last_refreshed_at,
                    blob,
                    blob.as_ref().map(|_| EMBEDDING_MODEL_ID),
                ],
            )
            .map_err(store_err)?;
        if changed > 0
            && let Some(blob) = &blob
        {
            let rowid: i64 = tx
                .query_row(
                    "SELECT rowid FROM mental_models WHERE bank_id = ?1 AND id = ?2",
                    params![bank_id, id],
                    |r| r.get(0),
                )
                .map_err(store_err)?;
            write_vec(tx, rowid, bank_id, blob)?;
        }
        Ok(changed)
    })
}

/// vec0 has no upsert, so a rewrite is a delete plus an insert — the same
/// shape `nodes::set_embedding` uses for `vec_nodes`.
fn write_vec(tx: &rusqlite::Transaction, rowid: i64, bank_id: &str, blob: &[u8]) -> Result<()> {
    tx.execute(
        "DELETE FROM vec_mental_models WHERE rowid = ?1",
        params![rowid],
    )
    .map_err(store_err)?;
    tx.execute(
        "INSERT INTO vec_mental_models (rowid, bank_id, embedding) VALUES (?1, ?2, ?3)",
        params![rowid, bank_id, blob],
    )
    .map_err(store_err)?;
    Ok(())
}

pub fn get(db: &Db, bank_id: &str, id: &str) -> Result<Option<MentalModel>> {
    let conn = db.read()?;
    conn.query_row(
        &format!("SELECT {SELECT_COLUMNS} FROM mental_models WHERE bank_id = ?1 AND id = ?2"),
        params![bank_id, id],
        from_row,
    )
    .optional()
    .map_err(store_err)
}

/// Newest refresh first (`memory_engine.py:11077`). A never-refreshed model
/// has a NULL watermark, which SQLite's DESC sorts *last* — so the freshly
/// created ones sit at the bottom until their first refresh, exactly as
/// Postgres orders them for legacy.
pub fn list(db: &Db, bank_id: &str, limit: usize, offset: usize) -> Result<Vec<MentalModel>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {SELECT_COLUMNS} FROM mental_models
             WHERE bank_id = ?1
             ORDER BY last_refreshed_at DESC, created_at DESC
             LIMIT ?2 OFFSET ?3"
        ))
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![bank_id, limit as i64, offset as i64], from_row)
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// The `vec_mental_models` row goes with it via `mental_models_vec_ad`.
pub fn delete(db: &Db, bank_id: &str, id: &str) -> Result<usize> {
    db.write(|tx| {
        tx.execute(
            "DELETE FROM mental_models WHERE bank_id = ?1 AND id = ?2",
            params![bank_id, id],
        )
        .map_err(store_err)
    })
}

/// K-nearest mental models in `bank_id`, nearest first, as `(id, distance)`.
///
/// Partitioned on `bank_id` by vec0 itself, and filtered to the active
/// producer through the source row's `embedding_model` (AX-1) — the same two
/// guards `search::knn` applies to `vec_nodes`, for the same reasons: a cosine
/// distance across two embedding spaces is a number with no meaning, and a
/// cross-bank hit would leak another bank's text.
pub fn knn(
    db: &Db,
    bank_id: &str,
    query_embedding: &[f32],
    k: usize,
) -> Result<Vec<(String, f64)>> {
    let blob = vecblob::encode(query_embedding)?;
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT m.id, v.distance FROM vec_mental_models v
             JOIN mental_models m ON m.rowid = v.rowid
             WHERE v.bank_id = ?1 AND v.embedding MATCH ?2 AND v.k = ?3
               AND m.embedding_model = ?4
             ORDER BY v.distance",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![bank_id, blob, k as i64, EMBEDDING_MODEL_ID], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use memgarden_core::EMBEDDING_DIM;

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[i] = 1.0;
        v
    }

    fn db_with_banks() -> Db {
        let db = Db::open_memory().unwrap();
        crate::banks::create(&db, "b1", None, None).unwrap();
        crate::banks::create(&db, "b2", None, None).unwrap();
        db
    }

    fn new<'a>(id: &'a str, bank: &'a str, name: &'a str, content: &'a str) -> NewMentalModel<'a> {
        NewMentalModel {
            id,
            bank_id: bank,
            name,
            source_query: Some("latency work"),
            content,
            max_tokens: Some(2048),
            trigger: None,
        }
    }

    #[test]
    fn id_format_is_legacy_mm_uuid4hex() {
        let id = new_id();
        assert!(id.starts_with("mm-"), "{id}");
        // uuid4 hex with no dashes: 32 lowercase hex chars.
        let hex = &id[3..];
        assert_eq!(hex.len(), 32, "{id}");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(new_id(), new_id());
    }

    /// The composite PK is `(bank_id, id)`, so the *same* mental-model id may
    /// exist in two banks and every read is bank-scoped.
    #[test]
    fn composite_primary_key_scopes_ids_to_a_bank() {
        let db = db_with_banks();
        insert(&db, &new("mm-shared", "b1", "one", "first"), None).unwrap();
        insert(&db, &new("mm-shared", "b2", "two", "second"), None).unwrap();

        assert_eq!(get(&db, "b1", "mm-shared").unwrap().unwrap().name, "one");
        assert_eq!(get(&db, "b2", "mm-shared").unwrap().unwrap().name, "two");

        // A duplicate within one bank is still a PK violation.
        assert!(insert(&db, &new("mm-shared", "b1", "again", "x"), None).is_err());

        // And a delete in one bank leaves the other bank's row alone.
        assert_eq!(delete(&db, "b1", "mm-shared").unwrap(), 1);
        assert!(get(&db, "b1", "mm-shared").unwrap().is_none());
        assert!(get(&db, "b2", "mm-shared").unwrap().is_some());
    }

    #[test]
    fn update_only_touches_the_fields_it_is_given() {
        let db = db_with_banks();
        insert(&db, &new("mm-1", "b1", "name", "content"), None).unwrap();
        let changed = update(
            &db,
            "b1",
            "mm-1",
            &Patch {
                content: Some("new content"),
                last_refreshed_at: Some(1_700_000_000_000),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(changed, 1);

        let got = get(&db, "b1", "mm-1").unwrap().unwrap();
        assert_eq!(got.content, "new content");
        assert_eq!(got.name, "name", "an absent patch field is untouched");
        assert_eq!(got.source_query.as_deref(), Some("latency work"));
        assert_eq!(got.last_refreshed_at, Some(1_700_000_000_000));

        // Wrong bank changes nothing.
        assert_eq!(
            update(
                &db,
                "b2",
                "mm-1",
                &Patch {
                    content: Some("nope"),
                    ..Default::default()
                },
                None
            )
            .unwrap(),
            0
        );
    }

    /// KNN is partitioned by bank and ordered by distance, and an update
    /// replaces the vector rather than adding a second one.
    #[test]
    fn knn_is_bank_partitioned_and_upserts_on_update() {
        let db = db_with_banks();
        insert(&db, &new("mm-a", "b1", "a", "a"), Some(&unit(0))).unwrap();
        insert(&db, &new("mm-b", "b1", "b", "b"), Some(&unit(1))).unwrap();
        insert(&db, &new("mm-other", "b2", "o", "o"), Some(&unit(0))).unwrap();

        let hits = knn(&db, "b1", &unit(0), 10).unwrap();
        assert_eq!(
            hits.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["mm-a", "mm-b"],
            "nearest first, and b2's identical vector must not appear"
        );
        assert!(hits[0].1 < hits[1].1);

        // b2 sees only its own.
        assert_eq!(knn(&db, "b2", &unit(0), 10).unwrap().len(), 1);

        // Re-embedding leaves exactly one vec row for the model.
        update(&db, "b1", "mm-a", &Patch::default(), Some(&unit(2))).unwrap();
        let hits = knn(&db, "b1", &unit(0), 10).unwrap();
        assert_eq!(hits.len(), 2, "the stale vector must be gone: {hits:?}");
        assert_eq!(hits[0].0, "mm-b", "mm-a moved away from the query vector");
    }

    /// Mental-model KNN latency against a bank far larger than any real one
    /// (a mental model is created by a human decision; tens is realistic).
    /// Numbers go in the PR body. Run:
    ///   cargo test --release -p memgarden-store --lib -- --ignored --nocapture knn_bench
    #[test]
    #[ignore = "measurement, not a correctness check"]
    fn mental_model_knn_bench() {
        let db = db_with_banks();
        let n = 1000;
        for i in 0..n {
            let id = format!("mm-{i}");
            insert(&db, &new(&id, "b1", &id, "content"), Some(&unit(i % 384))).unwrap();
        }
        let query = unit(7);
        // Warm the statement cache and the page cache first.
        knn(&db, "b1", &query, 3).unwrap();

        let runs = 200;
        let mut samples: Vec<u128> = Vec::with_capacity(runs);
        for _ in 0..runs {
            let started = std::time::Instant::now();
            let hits = knn(&db, "b1", &query, 3).unwrap();
            samples.push(started.elapsed().as_micros());
            assert_eq!(hits.len(), 3);
        }
        samples.sort_unstable();
        println!(
            "mental_model_knn_bench: n={n} k=3 p50={}us p95={}us max={}us",
            samples[runs / 2],
            samples[runs * 95 / 100],
            samples[runs - 1]
        );
    }

    /// A model stored while the embedder was still loading has no vector and
    /// is simply not a KNN candidate — every other read still returns it.
    #[test]
    fn a_model_without_an_embedding_is_absent_from_knn_only() {
        let db = db_with_banks();
        insert(&db, &new("mm-cold", "b1", "cold", "text"), None).unwrap();
        assert!(knn(&db, "b1", &unit(0), 10).unwrap().is_empty());
        assert_eq!(list(&db, "b1", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn list_orders_by_last_refreshed_desc_and_pages() {
        let db = db_with_banks();
        for (id, refreshed) in [("mm-1", Some(10i64)), ("mm-2", Some(30)), ("mm-3", None)] {
            insert(&db, &new(id, "b1", id, "c"), None).unwrap();
            if let Some(at) = refreshed {
                update(
                    &db,
                    "b1",
                    id,
                    &Patch {
                        last_refreshed_at: Some(at),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }
        }
        let ids: Vec<String> = list(&db, "b1", 10, 0)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["mm-2", "mm-1", "mm-3"]);
        assert_eq!(list(&db, "b1", 1, 1).unwrap()[0].id, "mm-1");
    }

    /// Deleting the bank cascades into `mental_models`, and the AFTER DELETE
    /// trigger clears `vec_mental_models` on that cascade path too (Critic
    /// Revision R5) — Rust never sees those row deletions.
    #[test]
    fn bank_delete_cascades_into_the_vector_table() {
        let db = db_with_banks();
        insert(&db, &new("mm-a", "b1", "a", "a"), Some(&unit(0))).unwrap();
        insert(&db, &new("mm-other", "b2", "o", "o"), Some(&unit(0))).unwrap();

        crate::banks::delete(&db, "b1").unwrap();
        assert!(list(&db, "b1", 10, 0).unwrap().is_empty());
        assert!(knn(&db, "b1", &unit(0), 10).unwrap().is_empty());

        let remaining: i64 = db
            .read()
            .unwrap()
            .query_row("SELECT count(*) FROM vec_mental_models", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "only b2's vector should survive");
    }
}
