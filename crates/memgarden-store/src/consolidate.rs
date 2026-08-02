//! Consolidation storage (CE-9a): observation provenance (`node_sources`),
//! evidence counting (`memory_nodes.proof_count`), and the dedup merge.
//!
//! Legacy stores provenance as a `uuid[]` column on `memory_units`
//! (`source_memory_ids`) and keeps `proof_count` alongside it; the merge is
//! one Postgres statement using `unnest`/`array_agg`
//! (`engine/consolidation/consolidator.py:284-296`). SQLite has no arrays, so
//! the array becomes the `node_sources` join table and the merge becomes the
//! four statements in [`merge_observation`] — still one transaction.

use rusqlite::{OptionalExtension, params};

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
/// fewer of them than facts. Measured (dedup_probe_bench, release,
/// IN-MEMORY — an on-disk WAL database will be slower): p95 0.39ms at 500,
/// 1.61ms at 2000, i.e. linear at ~0.8us per observation, on a background
/// path with no latency SLO. Read the ceiling PER ROUND, not per call: this
/// runs once per created observation, so B8's batch_size = 50 means 50 scans
/// per round — ~0.4s at the 10k-observation ceiling. Memory matters as much
/// as time: every call materialises n x (1536-byte vector + text) at once,
/// ~3MB at 2k and ~15MB at 10k, churned 50 times a round. Upgrade path at
/// ~10k observations: a second vec0 table partitioned on (bank_id,
/// fact_type), which is the only way to get a type-filtered KNN out of
/// sqlite-vec, and which also removes the allocation.`
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
/// (Critic Revision R3). The reason is **not** that the dedup probe reads
/// this row back — [`observation_vectors`] excludes the new id, and the
/// caller's cosine runs against the slice it already holds. It is that this
/// observation must be embedded before the **next** one can dedup against
/// it, and in a batch round the next one is milliseconds away, far inside
/// `embedding.backlog_poll_secs`. Taking `embedding` by value rather than
/// embedding internally is what makes that a compile-time guarantee — there
/// is no way to call this without one.
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
/// Critic Revision R4 — unless the merged text is byte-identical to what the
/// twin already says, which is what the LLM's empty-`text` fallback produces
/// and which would queue a re-embed of an unchanged string. See the design
/// note.
///
/// The twin's continued existence is checked **first**, not inferred. `keep_id`
/// was chosen before an LLM call that takes seconds; if it was deleted in the
/// meantime and `drop_id` happened to carry no sources, every statement below
/// would silently no-op and the DELETE would still destroy the new
/// observation. Failing up front rolls the whole transaction back and the
/// caller keeps its row.
pub fn merge_observation(db: &Db, keep_id: i64, drop_id: i64, merged_text: &str) -> Result<i64> {
    let now = now_ms();
    db.write(|tx| {
        let current: Option<String> = tx
            .query_row(
                "SELECT text FROM memory_nodes WHERE id = ?1",
                params![keep_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)?;
        let Some(current) = current else {
            return Err(memgarden_core::error::Error::NotFound(format!(
                "observation {keep_id} vanished before the merge could be applied"
            )));
        };

        tx.execute(
            "INSERT OR IGNORE INTO node_sources (observation_id, source_id, created_at)
             SELECT ?1, source_id, created_at FROM node_sources WHERE observation_id = ?2",
            params![keep_id, drop_id],
        )
        .map_err(store_err)?;
        if current != merged_text {
            nodes::update_text_tx(tx, keep_id, merged_text, now)?;
        }
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
        // R4: the stale embedding is nulled and the node is back on the
        // backlog...
        let merged = nodes::get(&db, keep).unwrap().unwrap();
        assert!(
            merged.embedding.is_none(),
            "merge nulls the stale embedding"
        );
        assert!(
            nodes::pending_embeddings(&db, 10)
                .unwrap()
                .iter()
                .any(|(id, ..)| *id == keep),
            "re-embed is queued"
        );
        // ...but the vec row STAYS, so the twin is stale to the vector arm
        // for the backlog window rather than invisible to it (review MEDIUM).
        assert!(
            crate::search::knn(&db, "b1", &vec_at(0.0), 5)
                .unwrap()
                .iter()
                .any(|(id, _)| *id == keep),
            "the merged twin must stay reachable by KNN until it is re-embedded"
        );
    }

    /// The LLM's empty-`text` fallback resolves the merged text to the twin's
    /// current wording. Re-embedding an unchanged string is pure loss — a
    /// backlog tick and a window where the vector is stale for no reason.
    #[test]
    fn a_merge_that_does_not_change_the_text_keeps_the_embedding() {
        let (db, facts) = seeded();
        let keep = insert_observation(&db, "b1", "unchanged", &vec_at(0.0), &facts[..1]).unwrap();
        let drop = insert_observation(&db, "b1", "twin", &vec_at(0.01), &facts[1..]).unwrap();

        assert_eq!(merge_observation(&db, keep, drop, "unchanged").unwrap(), 3);

        assert!(
            nodes::get(&db, keep).unwrap().unwrap().embedding.is_some(),
            "an unchanged text must not queue a re-embed"
        );
        assert!(
            nodes::pending_embeddings(&db, 10)
                .unwrap()
                .iter()
                .all(|(id, ..)| *id != keep)
        );
    }

    /// Security review LOW 6: `keep_id` is chosen before an LLM call that
    /// takes seconds. If it is deleted meanwhile AND the candidate carries no
    /// sources, every statement in the merge no-ops except the DELETE — which
    /// would destroy the new observation and merge nothing. The guard must
    /// fail the whole transaction instead.
    #[test]
    fn a_merge_into_a_vanished_twin_fails_without_touching_the_candidate() {
        let (db, _facts) = seeded();
        let keep = insert_observation(&db, "b1", "twin", &vec_at(0.0), &[]).unwrap();
        let drop = insert_observation(&db, "b1", "candidate", &vec_at(0.01), &[]).unwrap();
        nodes::delete(&db, keep).unwrap();

        let err = merge_observation(&db, keep, drop, "merged").unwrap_err();

        assert!(
            matches!(err, memgarden_core::error::Error::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        assert!(
            nodes::get(&db, drop).unwrap().is_some(),
            "the candidate must survive a merge that could not happen"
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
    /// dangling provenance row, and `proof_count` must follow it down — the
    /// value is derived, so a cascade no Rust code sees still has to recount
    /// (security review LOW 5; the `node_sources_ad` trigger).
    #[test]
    fn deleting_a_source_fact_cascades_and_recounts_proof() {
        let (db, facts) = seeded();
        let id = insert_observation(&db, "b1", "obs", &vec_at(0.0), &facts).unwrap();
        assert_eq!(proof_count(&db, id).unwrap(), 3);

        nodes::delete(&db, facts[0]).unwrap();

        assert_eq!(sources_of(&db, id).unwrap(), facts[1..]);
        assert_eq!(
            proof_count(&db, id).unwrap(),
            2,
            "proof_count must not drift above the surviving evidence"
        );
        assert!(nodes::get(&db, id).unwrap().is_some());
    }

    /// R4's first half, on the public wrapper B8/B9 will call: text replaced,
    /// embedding nulled, node re-queued, vec row retained.
    #[test]
    fn update_text_nulls_the_embedding_and_requeues_without_unindexing() {
        let (db, _facts) = seeded();
        let id = insert_observation(&db, "b1", "before", &vec_at(0.0), &[]).unwrap();

        nodes::update_text(&db, id, "after").unwrap();

        let node = nodes::get(&db, id).unwrap().unwrap();
        assert_eq!(node.text, "after");
        assert!(node.embedding.is_none());
        assert!(
            nodes::pending_embeddings(&db, 10)
                .unwrap()
                .iter()
                .any(|(pending, ..)| *pending == id)
        );
        assert!(
            crate::search::knn(&db, "b1", &vec_at(0.0), 5)
                .unwrap()
                .iter()
                .any(|(hit, _)| *hit == id),
            "stale-but-present beats invisible for the backlog window"
        );
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
