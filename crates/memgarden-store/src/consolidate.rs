//! Consolidation storage (CE-9a): observation provenance (`node_sources`),
//! evidence counting (`memory_nodes.proof_count`), and the dedup merge.
//!
//! Legacy stores provenance as a `uuid[]` column on `memory_units`
//! (`source_memory_ids`) and keeps `proof_count` alongside it; the merge is
//! one Postgres statement using `unnest`/`array_agg`
//! (`engine/consolidation/consolidator.py:284-296`). SQLite has no arrays, so
//! the array becomes the `node_sources` join table and the merge becomes the
//! four statements in [`merge_observation`] — still one transaction.

use rusqlite::params;

use memgarden_core::error::Result;
use memgarden_core::now_ms;
use memgarden_core::types::FactType;

use crate::{Db, nodes, store_err, vecblob};

/// One existing observation, as the dedup probe needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationVector {
    pub id: i64,
    pub text: String,
    pub embedding: Vec<f32>,
}

/// Every embedded observation in `bank_id` except `exclude_id`, for the
/// dedup probe's cosine scan.
///
/// Deliberately **not** the `vec_nodes` KNN path. vec0 partitions on
/// `bank_id` only, so a `MATCH ... AND k = 5` there returns the five nearest
/// nodes of *any* fact type; observations are a small minority of a bank, so
/// the top-5 would usually be all facts and dedup would never fire. Legacy
/// gets the type filter for free because it retrieves grouped by fact type
/// (`consolidator.py:222-227`, `types=["observation"]`). Scanning the
/// observations directly reproduces that exactly.
///
/// `// ponytail: full scan of one bank's observations, decoded in Rust —
/// observations are consolidated summaries, so there are orders of magnitude
/// fewer of them than facts. Measured (dedup_probe_bench, release): p95
/// 0.39ms at 500, 1.61ms at 2000, i.e. linear at ~0.8us per observation, on a
/// background path with no latency SLO. Upgrade path if a bank ever passes
/// ~10k observations: a second vec0 table partitioned on (bank_id,
/// fact_type), which is the only way to get a type-filtered KNN out of
/// sqlite-vec.`
pub fn observation_vectors(
    db: &Db,
    bank_id: &str,
    exclude_id: i64,
) -> Result<Vec<ObservationVector>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, text, embedding FROM memory_nodes
             WHERE bank_id = ?1 AND fact_type = 'observation'
               AND embedding IS NOT NULL AND id <> ?2",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![bank_id, exclude_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    rows.into_iter()
        .map(|(id, text, blob)| {
            Ok(ObservationVector {
                id,
                text,
                embedding: vecblob::decode(&blob)?,
            })
        })
        .collect()
}

/// Creates an observation node with its embedding, its source-fact links and
/// the matching `proof_count`, in **one** `BEGIN IMMEDIATE`.
///
/// The embedding is written here rather than left to the backlog worker
/// (Critic Revision R3): dedup probes the new observation's own vector
/// immediately afterwards, so this path depends on reading its own write.
/// Taking `embedding` by value rather than embedding internally is what makes
/// that a compile-time guarantee — there is no way to call this without one.
///
/// `source_ids` are filtered against `bank_id` in SQL, so an id that is
/// unknown or belongs to another bank is silently dropped rather than
/// aborting the insert (legacy drops unresolvable `source_fact_ids` the same
/// way).
pub fn insert_observation(
    db: &Db,
    bank_id: &str,
    text: &str,
    embedding: &[f32],
    source_ids: &[i64],
) -> Result<i64> {
    let blob = vecblob::encode(embedding)?;
    let now = now_ms();
    let uuid = uuid::Uuid::now_v7().to_string();
    db.write(|tx| {
        tx.execute(
            "INSERT INTO memory_nodes
             (uuid, bank_id, fact_type, text, embedding, mentioned_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?6)",
            params![
                uuid,
                bank_id,
                FactType::Observation.as_str(),
                text,
                blob,
                now,
            ],
        )
        .map_err(store_err)?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO vec_nodes (rowid, bank_id, embedding) VALUES (?1, ?2, ?3)",
            params![id, bank_id, blob],
        )
        .map_err(store_err)?;
        link_sources_tx(tx, id, bank_id, source_ids, now)?;
        recount_proof_tx(tx, id)?;
        Ok(id)
    })
}

/// Folds `drop_id` into `keep_id`: the merged text replaces `keep_id`'s,
/// `drop_id`'s sources are unioned in, `proof_count` is recomputed from the
/// union, and `drop_id` is deleted. One transaction, since a half-applied
/// merge either loses provenance or duplicates the observation.
///
/// Returns `keep_id`'s new `proof_count`.
///
/// **Only the redundant observation dies.** The source *facts* are never
/// touched — `node_sources` rows are moved, not the nodes they point at
/// (plan PR B7: "Merged-away facts are never deleted").
///
/// Diverges from legacy on one point: legacy keeps the twin's existing
/// embedding (`consolidator.py:283-285` — "the merged text is >= threshold
/// similar, so it stays representative and avoids a re-embed"). Here
/// `nodes::update_text_tx` nulls it so the backlog worker re-embeds, per
/// Critic Revision R4. See the design note.
pub fn merge_observation(db: &Db, keep_id: i64, drop_id: i64, merged_text: &str) -> Result<i64> {
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "INSERT OR IGNORE INTO node_sources (observation_id, source_id, created_at)
             SELECT ?1, source_id, created_at FROM node_sources WHERE observation_id = ?2",
            params![keep_id, drop_id],
        )
        .map_err(store_err)?;
        nodes::update_text_tx(tx, keep_id, merged_text, now)?;
        // `drop_id`'s own node_sources rows go with it (ON DELETE CASCADE);
        // the source facts they referenced do not.
        tx.execute("DELETE FROM memory_nodes WHERE id = ?1", params![drop_id])
            .map_err(store_err)?;
        recount_proof_tx(tx, keep_id)
    })
}

/// Source-fact ids backing an observation, ascending.
pub fn sources_of(db: &Db, observation_id: i64) -> Result<Vec<i64>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare("SELECT source_id FROM node_sources WHERE observation_id = ?1 ORDER BY source_id")
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![observation_id], |r| r.get::<_, i64>(0))
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// `memory_nodes.proof_count` for one node (0 for anything that is not a
/// sourced observation).
pub fn proof_count(db: &Db, node_id: i64) -> Result<i64> {
    let conn = db.read()?;
    conn.query_row(
        "SELECT proof_count FROM memory_nodes WHERE id = ?1",
        params![node_id],
        |r| r.get(0),
    )
    .map_err(store_err)
}

fn link_sources_tx(
    tx: &rusqlite::Transaction,
    observation_id: i64,
    bank_id: &str,
    source_ids: &[i64],
    now: i64,
) -> Result<()> {
    for &source_id in source_ids {
        tx.execute(
            "INSERT OR IGNORE INTO node_sources (observation_id, source_id, created_at)
             SELECT ?1, id, ?3 FROM memory_nodes WHERE id = ?2 AND bank_id = ?4",
            params![observation_id, source_id, now, bank_id],
        )
        .map_err(store_err)?;
    }
    Ok(())
}

/// `proof_count = count(node_sources)` — the count is always derived, never
/// incremented, so it cannot drift from the join table
/// (`consolidator.py:290`: `count(DISTINCT e)` over the unioned array).
fn recount_proof_tx(tx: &rusqlite::Transaction, observation_id: i64) -> Result<i64> {
    tx.execute(
        "UPDATE memory_nodes
         SET proof_count = (SELECT count(*) FROM node_sources WHERE observation_id = ?1)
         WHERE id = ?1",
        params![observation_id],
    )
    .map_err(store_err)?;
    tx.query_row(
        "SELECT proof_count FROM memory_nodes WHERE id = ?1",
        params![observation_id],
        |r| r.get(0),
    )
    .map_err(store_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::NewNode;

    const DIM: usize = memgarden_core::EMBEDDING_DIM;

    fn seeded() -> (Db, Vec<i64>) {
        let db = Db::open_memory().unwrap();
        crate::banks::create(&db, "b1", None, None).unwrap();
        let facts = (0..3)
            .map(|i| {
                nodes::insert(
                    &db,
                    NewNode::new("b1", FactType::World, &format!("source fact {i}")),
                )
                .unwrap()
            })
            .collect();
        (db, facts)
    }

    fn vec_at(angle: f32) -> Vec<f32> {
        let mut v = vec![0.0; DIM];
        v[0] = angle.cos();
        v[1] = angle.sin();
        v
    }

    #[test]
    fn insert_observation_writes_sources_embedding_and_proof_count() {
        let (db, facts) = seeded();
        let id = insert_observation(&db, "b1", "obs", &vec_at(0.0), &facts).unwrap();

        assert_eq!(sources_of(&db, id).unwrap(), facts);
        assert_eq!(proof_count(&db, id).unwrap(), 3);
        // Critic Revision R3: the embedding must already be there — the dedup
        // probe that runs next reads its own write.
        let node = nodes::get(&db, id).unwrap().unwrap();
        assert!(
            node.embedding.is_some(),
            "observation embedded synchronously"
        );
        assert_eq!(node.fact_type, FactType::Observation);
        // And it is in the vector index, not just the column.
        let hits = crate::search::knn(&db, "b1", &vec_at(0.0), 5).unwrap();
        assert!(hits.iter().any(|(hit, _)| *hit == id));
    }

    #[test]
    fn insert_observation_drops_unknown_and_foreign_sources() {
        let (db, facts) = seeded();
        crate::banks::create(&db, "b2", None, None).unwrap();
        let foreign = nodes::insert(&db, NewNode::new("b2", FactType::World, "elsewhere")).unwrap();

        let id = insert_observation(
            &db,
            "b1",
            "obs",
            &vec_at(0.0),
            &[facts[0], 999_999, foreign],
        )
        .unwrap();
        assert_eq!(sources_of(&db, id).unwrap(), vec![facts[0]]);
        assert_eq!(proof_count(&db, id).unwrap(), 1);
    }

    #[test]
    fn merge_unions_sources_recounts_proof_and_spares_the_facts() {
        let (db, facts) = seeded();
        let keep = insert_observation(&db, "b1", "keep", &vec_at(0.0), &facts[..2]).unwrap();
        // Overlapping source (facts[1]) proves the union is a set, not a concat.
        let drop = insert_observation(&db, "b1", "drop", &vec_at(0.01), &facts[1..]).unwrap();

        let count = merge_observation(&db, keep, drop, "merged text").unwrap();

        assert_eq!(count, 3, "union of {{0,1}} and {{1,2}}");
        assert_eq!(proof_count(&db, keep).unwrap(), 3);
        assert_eq!(sources_of(&db, keep).unwrap(), facts);
        assert_eq!(nodes::get(&db, keep).unwrap().unwrap().text, "merged text");
        assert!(
            nodes::get(&db, drop).unwrap().is_none(),
            "candidate deleted"
        );
        // The merged-away observation's facts are untouched.
        for &f in &facts {
            assert!(nodes::get(&db, f).unwrap().is_some(), "fact {f} survived");
        }
        // R4: the stale embedding is gone and the node is back on the backlog.
        let merged = nodes::get(&db, keep).unwrap().unwrap();
        assert!(
            merged.embedding.is_none(),
            "merge nulls the stale embedding"
        );
        assert!(
            crate::search::knn(&db, "b1", &vec_at(0.0), 5)
                .unwrap()
                .iter()
                .all(|(id, _)| *id != keep),
            "stale vec_nodes row deleted too"
        );
        assert!(
            nodes::pending_embeddings(&db, 10)
                .unwrap()
                .iter()
                .any(|(id, ..)| *id == keep),
            "re-embed is queued"
        );
    }

    #[test]
    fn deleting_an_observation_cascades_sources_but_leaves_the_facts() {
        let (db, facts) = seeded();
        let id = insert_observation(&db, "b1", "obs", &vec_at(0.0), &facts).unwrap();

        nodes::delete(&db, id).unwrap();

        assert!(sources_of(&db, id).unwrap().is_empty());
        let orphans: i64 = db
            .read()
            .unwrap()
            .query_row("SELECT count(*) FROM node_sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0);
        for &f in &facts {
            assert!(nodes::get(&db, f).unwrap().is_some());
        }
    }

    /// The other cascade direction: deleting a *source fact* must not leave a
    /// dangling provenance row, and must drop the observation's proof by one
    /// on the next recount.
    #[test]
    fn deleting_a_source_fact_cascades_its_provenance_row() {
        let (db, facts) = seeded();
        let id = insert_observation(&db, "b1", "obs", &vec_at(0.0), &facts).unwrap();

        nodes::delete(&db, facts[0]).unwrap();

        assert_eq!(sources_of(&db, id).unwrap(), facts[1..]);
        assert!(nodes::get(&db, id).unwrap().is_some());
    }

    #[test]
    fn observation_vectors_excludes_self_facts_and_other_banks() {
        let (db, facts) = seeded();
        crate::banks::create(&db, "b2", None, None).unwrap();
        let other_bank = insert_observation(&db, "b2", "elsewhere", &vec_at(0.0), &[]).unwrap();
        let me = insert_observation(&db, "b1", "me", &vec_at(0.0), &[]).unwrap();
        let peer = insert_observation(&db, "b1", "peer", &vec_at(0.5), &[]).unwrap();
        // An observation with no embedding yet is not a probe candidate.
        let unembedded = nodes::insert(
            &db,
            NewNode::new("b1", FactType::Observation, "not embedded yet"),
        )
        .unwrap();

        let got = observation_vectors(&db, "b1", me).unwrap();

        let ids: Vec<i64> = got.iter().map(|o| o.id).collect();
        assert_eq!(ids, vec![peer]);
        for excluded in [me, other_bank, unembedded, facts[0]] {
            assert!(!ids.contains(&excluded));
        }
    }
}
