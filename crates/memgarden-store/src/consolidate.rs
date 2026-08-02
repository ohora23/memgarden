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
///
/// Carries the `uuid` as well as the rowid because the probe's result is used
/// **after** an LLM call that can run for minutes: see [`merge_observation`].
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationVector {
    pub id: i64,
    pub uuid: String,
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
            "SELECT id, uuid, text, embedding FROM memory_nodes
             WHERE bank_id = ?1 AND fact_type = 'observation'
               AND embedding IS NOT NULL AND id <> ?2",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![bank_id, exclude_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    rows.into_iter()
        .map(|(id, uuid, text, blob)| {
            Ok(ObservationVector {
                id,
                uuid,
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
    db.write(|tx| insert_observation_tx(tx, bank_id, text, embedding, source_ids, now_ms()))
}

/// [`insert_observation`]'s body, for callers already inside a write
/// transaction. CE-9b's batch round applies a whole LLM plan — several
/// creates, updates and deletes — in one `BEGIN IMMEDIATE`, so it cannot
/// call the `db.write` wrapper per observation.
pub(crate) fn insert_observation_tx(
    tx: &rusqlite::Transaction,
    bank_id: &str,
    text: &str,
    embedding: &[f32],
    source_ids: &[i64],
    now: i64,
) -> Result<i64> {
    let blob = vecblob::encode(embedding)?;
    let uuid = uuid::Uuid::now_v7().to_string();
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
/// **Keyed by uuid, not rowid, and this is load-bearing.** Both nodes are
/// named seconds — up to the Ollama client's whole deadline — before this runs:
/// `select_twin` picks the survivor and the caller inserted the candidate,
/// then an adjudication happens, and only then do we mutate. `memory_nodes.id`
/// is `INTEGER PRIMARY KEY` with no `AUTOINCREMENT`, so SQLite recycles the
/// rowid of a deleted max row; an existence check on a recycled rowid passes
/// while pointing at a *different* observation, which would rewrite one
/// stranger's text and delete another's. The uuid is `NOT NULL UNIQUE` and is
/// never reused. Both are resolved to rowids **inside** this transaction, so
/// the identity that is checked is the identity that is mutated.
///
/// The twin's continued existence is checked **first**, not inferred. If it was
/// deleted in the meantime and the candidate happened to carry no sources,
/// every statement below would silently no-op and the DELETE would still
/// destroy the new observation. Failing up front rolls the whole transaction
/// back and the caller keeps its row.
///
/// Returns `(keep_id, proof_count)` — the survivor's rowid resolved under the
/// transaction, which is the only place it is safe to read.
pub fn merge_observation(
    db: &Db,
    bank_id: &str,
    keep_uuid: &str,
    drop_uuid: &str,
    merged_text: &str,
) -> Result<(i64, i64)> {
    let now = now_ms();
    db.write(|tx| {
        let keep: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, text FROM memory_nodes
                 WHERE uuid = ?1 AND bank_id = ?2 AND fact_type = 'observation'",
                params![keep_uuid, bank_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(store_err)?;
        let Some((keep_id, current)) = keep else {
            return Err(memgarden_core::error::Error::NotFound(format!(
                "observation {keep_uuid} vanished before the merge could be applied"
            )));
        };
        // The candidate is resolved the same way. A merge that cannot find
        // what it was told to drop must not proceed: the union would attach
        // the wrong provenance and the DELETE would hit nothing (or, on a
        // recycled rowid, the wrong row).
        let drop_id: Option<i64> = tx
            .query_row(
                "SELECT id FROM memory_nodes
                 WHERE uuid = ?1 AND bank_id = ?2 AND fact_type = 'observation'",
                params![drop_uuid, bank_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_err)?;
        let Some(drop_id) = drop_id else {
            return Err(memgarden_core::error::Error::NotFound(format!(
                "observation {drop_uuid} vanished before the merge could be applied"
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
        Ok((keep_id, recount_proof_tx(tx, keep_id)?))
    })
}

/// An observation's current text, scoped to its bank — what
/// `consolidate::round` reads back to re-embed a twin a merge just rewrote.
///
/// The bank and fact-type predicates are the point: without them a rowid
/// recycled between the merge committing and the re-embed would hand back
/// another bank's node, and the caller would stamp its vector into *this*
/// bank's `vec_nodes` partition.
pub fn observation_text_in_bank(db: &Db, bank_id: &str, id: i64) -> Result<Option<String>> {
    db.read()?
        .query_row(
            "SELECT text FROM memory_nodes
             WHERE id = ?1 AND bank_id = ?2 AND fact_type = 'observation'",
            params![id, bank_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(store_err)
}

// ---------------------------------------------------------------------------
// CE-9b: the batch round — fact selection, plan application, the run ledger
// ---------------------------------------------------------------------------

/// One not-yet-consolidated fact, as the batch prompt needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct FactRow {
    pub id: i64,
    pub uuid: String,
    pub text: String,
    pub occurred_start: Option<i64>,
    pub occurred_end: Option<i64>,
    pub mentioned_at: Option<i64>,
}

/// Facts in `bank_id` with `id > after_id`, oldest first, at most `limit`.
///
/// "Fact" here means *not* an observation: consolidation reads `world` and
/// `experience` nodes and writes `observation` ones, so including observations
/// would feed the round its own output. Legacy scopes the same way with
/// `types=[...]` on its unconsolidated query (`consolidator.py:890-933`),
/// where the equivalent guard is a `consolidated_at IS NULL` column.
///
/// **`id > after_id` is the whole watermark mechanism.** MemGarden has no
/// per-fact `consolidated_at`: `memory_nodes.id` is a monotone SQLite rowid,
/// so "everything newer than the last run's high-water mark" is one indexed
/// range scan and needs no second write per fact. The cost of that choice is
/// recorded in the design note (a fact inserted *below* a committed watermark
/// — only possible via an explicit id — is never seen).
pub fn unconsolidated(db: &Db, bank_id: &str, after_id: i64, limit: usize) -> Result<Vec<FactRow>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, uuid, text, occurred_start, occurred_end, mentioned_at
             FROM memory_nodes
             WHERE bank_id = ?1 AND fact_type <> 'observation' AND id > ?2
             ORDER BY id LIMIT ?3",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![bank_id, after_id, limit as i64], |r| {
            Ok(FactRow {
                id: r.get(0)?,
                uuid: r.get(1)?,
                text: r.get(2)?,
                occurred_start: r.get(3)?,
                occurred_end: r.get(4)?,
                mentioned_at: r.get(5)?,
            })
        })
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// How many facts are waiting above `after_id` — the background task's gate,
/// so a bank with nothing new costs one indexed count and no run row.
pub fn count_unconsolidated(db: &Db, bank_id: &str, after_id: i64) -> Result<i64> {
    db.read()?
        .query_row(
            "SELECT count(*) FROM memory_nodes
             WHERE bank_id = ?1 AND fact_type <> 'observation' AND id > ?2",
            params![bank_id, after_id],
            |r| r.get(0),
        )
        .map_err(store_err)
}

/// An observation the LLM asked to create, with its already-computed vector.
#[derive(Debug, Clone, Copy)]
pub struct NewObservation<'a> {
    pub text: &'a str,
    pub embedding: &'a [f32],
    pub source_ids: &'a [i64],
}

/// An observation the LLM asked to update: new text, plus the facts to add to
/// its provenance.
///
/// Keyed by **uuid, not rowid**. The LLM names a uuid, and the rowid it maps
/// to was read seconds earlier: SQLite reuses the rowid of a deleted max row,
/// so an update aimed at an observation deleted in the meantime could land on
/// a brand-new, unrelated observation and silently rewrite its text. (Not
/// hypothetical — `apply_plan_skips_an_update_whose_target_vanished` hit
/// exactly this reuse while being written.) The uuid is unique for the life
/// of the database.
#[derive(Debug, Clone, Copy)]
pub struct ObservationUpdate<'a> {
    pub uuid: &'a str,
    pub text: &'a str,
    pub source_ids: &'a [i64],
}

/// What [`apply_plan`] actually did — never what it was asked to do. An
/// entry whose target vanished between the LLM call and the write is skipped,
/// not counted, and not an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub created: Vec<i64>,
    /// `(rowid, uuid)` of each update that actually landed — resolved under
    /// the transaction, so these rowids are safe to use afterwards. The
    /// caller needs them to re-embed and dedup the rewritten observations.
    pub updated: Vec<(i64, String)>,
    pub deleted: usize,
}

/// Applies one LLM batch's whole plan in **one** `BEGIN IMMEDIATE`.
///
/// One transaction per batch, and the LLM call is long over by the time we
/// get here — the write lock is never held across it (CE-9a's handoff #5).
/// Creates, updates and deletes of one batch belong together: a half-applied
/// plan can delete an observation whose replacement create was rolled back.
///
/// * `creates` → [`insert_observation_tx`] (node + vector + provenance +
///   `proof_count`).
/// * `updates` → `nodes::update_text_tx` (which nulls the embedding so the
///   backlog re-embeds — R4's one text-update rule) + source union + recount.
///   An update whose text is unchanged keeps its vector, same exception the
///   merge makes.
/// * `deletes` → only `observation` rows, only in `bank_id`. Rule 7 says be
///   conservative; the caller already restricts deletes to the pooled set,
///   and this is the storage-layer half of the same guard — the LLM cannot
///   name a source fact and have it deleted. Keyed by uuid for the same
///   reason [`ObservationUpdate`] is.
pub fn apply_plan(
    db: &Db,
    bank_id: &str,
    creates: &[NewObservation],
    updates: &[ObservationUpdate],
    deletes: &[&str],
) -> Result<Applied> {
    let now = now_ms();
    db.write(|tx| {
        let mut applied = Applied::default();
        for c in creates {
            applied.created.push(insert_observation_tx(
                tx,
                bank_id,
                c.text,
                c.embedding,
                c.source_ids,
                now,
            )?);
        }
        for u in updates {
            let target: Option<(i64, String)> = tx
                .query_row(
                    "SELECT id, text FROM memory_nodes
                     WHERE uuid = ?1 AND bank_id = ?2 AND fact_type = 'observation'",
                    params![u.uuid, bank_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(store_err)?;
            // Gone (or never an observation in this bank) — skip it. The LLM
            // named a target that no longer exists; that is a race, not a
            // failure of the batch.
            let Some((id, current)) = target else {
                continue;
            };
            if current != u.text {
                nodes::update_text_tx(tx, id, u.text, now)?;
            }
            link_sources_tx(tx, id, bank_id, u.source_ids, now)?;
            recount_proof_tx(tx, id)?;
            applied.updated.push((id, u.uuid.to_string()));
        }
        for uuid in deletes {
            applied.deleted += tx
                .execute(
                    "DELETE FROM memory_nodes
                     WHERE uuid = ?1 AND bank_id = ?2 AND fact_type = 'observation'",
                    params![uuid, bank_id],
                )
                .map_err(store_err)?;
        }
        Ok(applied)
    })
}

/// A row of `consolidation_runs` (`0004_consolidation.sql`).
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub id: i64,
    pub bank_id: String,
    pub status: String,
    pub facts_seen: i64,
    pub created_n: i64,
    pub updated_n: i64,
    pub deleted_n: i64,
    pub merged_n: i64,
    pub watermark: Option<i64>,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// What a finished round produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunCounts {
    pub facts_seen: i64,
    pub created_n: i64,
    pub updated_n: i64,
    pub deleted_n: i64,
    pub merged_n: i64,
}

/// Opens a `running` row. The caller must close it with [`finish_run`].
pub fn start_run(db: &Db, bank_id: &str) -> Result<i64> {
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "INSERT INTO consolidation_runs (bank_id, status, started_at)
             VALUES (?1, 'running', ?2)",
            params![bank_id, now],
        )
        .map_err(store_err)?;
        Ok(tx.last_insert_rowid())
    })
}

/// Closes a run as `done` or `failed`.
///
/// `watermark` is the highest fact id that reached a terminal decision, and it
/// is written on a **failed** run too when the round got partway: the facts
/// before the failure were consolidated, and replaying them would create
/// duplicate observations. `None` leaves the column NULL, so
/// [`watermark`](self::watermark) ignores the run entirely.
pub fn finish_run(
    db: &Db,
    run_id: i64,
    status: &str,
    counts: RunCounts,
    watermark: Option<i64>,
    error: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "UPDATE consolidation_runs
             SET status = ?2, facts_seen = ?3, created_n = ?4, updated_n = ?5,
                 deleted_n = ?6, merged_n = ?7, watermark = ?8, error = ?9,
                 finished_at = ?10
             WHERE id = ?1",
            params![
                run_id,
                status,
                counts.facts_seen,
                counts.created_n,
                counts.updated_n,
                counts.deleted_n,
                counts.merged_n,
                watermark,
                error,
                now,
            ],
        )
        .map_err(store_err)?;
        Ok(())
    })
}

/// Closes out `running` rows left behind by a process that died mid-round.
///
/// Same job as `retain_jobs::fail_stale` at startup (`main.rs`), and it became
/// load-bearing the moment CE-9b added a per-bank in-flight guard: the guard
/// itself is in-memory and dies with the process, but a `running` ledger row
/// survives and would otherwise sit there forever claiming a round is
/// underway. Their watermark is NULL, so
/// [`watermark`](self::watermark) already ignores them and the facts are
/// simply re-selected — this only stops the ledger from lying.
pub fn fail_stale_runs(db: &Db, reason: &str) -> Result<usize> {
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "UPDATE consolidation_runs
             SET status = 'failed', error = ?1, finished_at = ?2
             WHERE status = 'running'",
            params![reason, now],
        )
        .map_err(store_err)
    })
}

/// The bank's high-water mark: the highest fact id any run has committed.
///
/// `MAX` over every run that recorded one, not just the last or only the
/// successful ones — the value is monotone by construction and a failed run
/// that made partial progress still must not have its work replayed.
pub fn watermark(db: &Db, bank_id: &str) -> Result<i64> {
    db.read()?
        .query_row(
            "SELECT COALESCE(MAX(watermark), 0) FROM consolidation_runs WHERE bank_id = ?1",
            params![bank_id],
            |r| r.get(0),
        )
        .map_err(store_err)
}

/// The most recently started run for a bank, for `GET /v1/banks/{id}/consolidation`.
pub fn latest_run(db: &Db, bank_id: &str) -> Result<Option<RunRow>> {
    db.read()?
        .query_row(
            "SELECT id, bank_id, status, facts_seen, created_n, updated_n, deleted_n,
                    merged_n, watermark, error, started_at, finished_at
             FROM consolidation_runs WHERE bank_id = ?1 ORDER BY id DESC LIMIT 1",
            params![bank_id],
            |r| {
                Ok(RunRow {
                    id: r.get(0)?,
                    bank_id: r.get(1)?,
                    status: r.get(2)?,
                    facts_seen: r.get(3)?,
                    created_n: r.get(4)?,
                    updated_n: r.get(5)?,
                    deleted_n: r.get(6)?,
                    merged_n: r.get(7)?,
                    watermark: r.get(8)?,
                    error: r.get(9)?,
                    started_at: r.get(10)?,
                    finished_at: r.get(11)?,
                })
            },
        )
        .optional()
        .map_err(store_err)
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

    fn uuid_of(db: &Db, id: i64) -> String {
        nodes::get(db, id).unwrap().unwrap().uuid
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

        let (into, count) = merge_observation(
            &db,
            "b1",
            &uuid_of(&db, keep),
            &uuid_of(&db, drop),
            "merged text",
        )
        .unwrap();

        assert_eq!(into, keep);
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

        assert_eq!(
            merge_observation(
                &db,
                "b1",
                &uuid_of(&db, keep),
                &uuid_of(&db, drop),
                "unchanged"
            )
            .unwrap(),
            (keep, 3)
        );

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
        let (keep_uuid, drop_uuid) = (uuid_of(&db, keep), uuid_of(&db, drop));
        nodes::delete(&db, keep).unwrap();

        let err = merge_observation(&db, "b1", &keep_uuid, &drop_uuid, "merged").unwrap_err();

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

    // --- CE-9b: fact selection, the plan, the ledger ----------------------

    #[test]
    fn unconsolidated_reads_facts_above_the_watermark_oldest_first() {
        let (db, facts) = seeded();
        crate::banks::create(&db, "b2", None, None).unwrap();
        let elsewhere = nodes::insert(&db, NewNode::new("b2", FactType::World, "other")).unwrap();
        // An observation is output, never input — feeding it back would let a
        // round consolidate its own results.
        insert_observation(&db, "b1", "obs", &vec_at(0.0), &[]).unwrap();
        let experience =
            nodes::insert(&db, NewNode::new("b1", FactType::Experience, "felt slow")).unwrap();

        let all = unconsolidated(&db, "b1", 0, 100).unwrap();
        assert_eq!(
            all.iter().map(|f| f.id).collect::<Vec<_>>(),
            [facts.clone(), vec![experience]].concat(),
            "world + experience, ascending; no observation, no other bank"
        );
        assert!(!all.iter().any(|f| f.id == elsewhere));
        assert!(all.iter().all(|f| !f.uuid.is_empty()));

        // The watermark is the whole mechanism.
        assert_eq!(
            unconsolidated(&db, "b1", facts[1], 100)
                .unwrap()
                .iter()
                .map(|f| f.id)
                .collect::<Vec<_>>(),
            vec![facts[2], experience]
        );
        assert_eq!(unconsolidated(&db, "b1", 0, 2).unwrap().len(), 2, "limit");
        assert_eq!(count_unconsolidated(&db, "b1", 0).unwrap(), 4);
        assert_eq!(count_unconsolidated(&db, "b1", facts[2]).unwrap(), 1);
        assert_eq!(count_unconsolidated(&db, "b1", experience).unwrap(), 0);
    }

    #[test]
    fn apply_plan_creates_updates_and_deletes_in_one_transaction() {
        let (db, facts) = seeded();
        let target = insert_observation(&db, "b1", "before", &vec_at(0.0), &facts[..1]).unwrap();
        let doomed = insert_observation(&db, "b1", "superseded", &vec_at(0.5), &[]).unwrap();
        let (target_uuid, doomed_uuid) = (uuid_of(&db, target), uuid_of(&db, doomed));
        let embedding = vec_at(0.25);

        let applied = apply_plan(
            &db,
            "b1",
            &[NewObservation {
                text: "a brand new observation",
                embedding: &embedding,
                source_ids: &facts[1..],
            }],
            &[ObservationUpdate {
                uuid: &target_uuid,
                text: "after",
                source_ids: &facts[1..2],
            }],
            &[doomed_uuid.as_str()],
        )
        .unwrap();

        assert_eq!(applied.created.len(), 1);
        assert_eq!((applied.updated.len(), applied.deleted), (1, 1));

        let created = nodes::get(&db, applied.created[0]).unwrap().unwrap();
        assert_eq!(created.text, "a brand new observation");
        assert_eq!(created.fact_type, FactType::Observation);
        assert!(
            created.embedding.is_some(),
            "created observations are embedded"
        );
        assert_eq!(sources_of(&db, created.id).unwrap(), facts[1..].to_vec());
        assert_eq!(proof_count(&db, created.id).unwrap(), 2);

        let updated = nodes::get(&db, target).unwrap().unwrap();
        assert_eq!(updated.text, "after");
        assert_eq!(sources_of(&db, target).unwrap(), facts[..2].to_vec());
        assert_eq!(proof_count(&db, target).unwrap(), 2, "union, then recount");
        // R4: rewriting the text invalidates the vector and re-queues it.
        assert!(updated.embedding.is_none());
        assert!(
            nodes::pending_embeddings(&db, 10)
                .unwrap()
                .iter()
                .any(|(id, ..)| *id == target)
        );

        assert!(nodes::get(&db, doomed).unwrap().is_none());
        // Only observations died. Every source fact is intact.
        for &f in &facts {
            assert!(nodes::get(&db, f).unwrap().is_some());
        }
    }

    /// The storage half of the delete guard: the LLM can only ever name an
    /// observation, and only one in its own bank. A source fact id reaching
    /// `deletes` must be a no-op, not a deleted fact.
    #[test]
    fn apply_plan_refuses_to_delete_facts_or_other_banks_rows() {
        let (db, facts) = seeded();
        crate::banks::create(&db, "b2", None, None).unwrap();
        let foreign = insert_observation(&db, "b2", "elsewhere", &vec_at(0.0), &[]).unwrap();

        let (fact_uuid, foreign_uuid) = (uuid_of(&db, facts[0]), uuid_of(&db, foreign));
        let applied = apply_plan(
            &db,
            "b1",
            &[],
            &[],
            &[&fact_uuid, &foreign_uuid, "not-a-uuid-at-all"],
        )
        .unwrap();

        assert_eq!(applied.deleted, 0);
        assert!(nodes::get(&db, facts[0]).unwrap().is_some());
        assert!(nodes::get(&db, foreign).unwrap().is_some());
    }

    /// The LLM chose its target seconds ago. If the observation is gone by the
    /// time the write lands, the entry is skipped — not counted, and not an
    /// error that would roll back the rest of the batch.
    ///
    /// This test is also the reason updates are keyed by uuid: written against
    /// rowids it failed, because SQLite handed `alive` the rowid it had just
    /// freed by deleting `gone`, and the update meant for a dead observation
    /// landed on a live unrelated one.
    #[test]
    fn apply_plan_skips_an_update_whose_target_vanished() {
        let (db, facts) = seeded();
        let alive = insert_observation(&db, "b1", "here too", &vec_at(0.5), &[]).unwrap();
        let gone = insert_observation(&db, "b1", "here", &vec_at(0.0), &[]).unwrap();
        let (alive_uuid, gone_uuid) = (uuid_of(&db, alive), uuid_of(&db, gone));
        nodes::delete(&db, gone).unwrap();
        // The rowid `gone` freed is now the table's next one; a create in the
        // same batch takes it, and the stale update must NOT follow it.
        let recycled_vec = vec_at(0.9);

        let applied = apply_plan(
            &db,
            "b1",
            &[NewObservation {
                text: "a fresh observation on a recycled rowid",
                embedding: &recycled_vec,
                source_ids: &[],
            }],
            &[
                ObservationUpdate {
                    uuid: &gone_uuid,
                    text: "x",
                    source_ids: &facts[..1],
                },
                ObservationUpdate {
                    uuid: &alive_uuid,
                    text: "y",
                    source_ids: &facts[..1],
                },
            ],
            &[],
        )
        .unwrap();

        assert_eq!(
            applied.updated.len(),
            1,
            "only the surviving target counted"
        );
        assert_eq!(nodes::get(&db, alive).unwrap().unwrap().text, "y");
        assert_eq!(
            nodes::get(&db, applied.created[0]).unwrap().unwrap().text,
            "a fresh observation on a recycled rowid",
            "the vanished target's update must not land on whatever took its rowid"
        );
    }

    /// An update whose text is unchanged keeps its vector — the same
    /// exception the merge makes, for the same reason (re-embedding an
    /// identical string is pure loss).
    #[test]
    fn apply_plan_leaves_an_unchanged_text_embedded() {
        let (db, facts) = seeded();
        let id = insert_observation(&db, "b1", "same", &vec_at(0.0), &[]).unwrap();

        apply_plan(
            &db,
            "b1",
            &[],
            &[ObservationUpdate {
                uuid: &uuid_of(&db, id),
                text: "same",
                source_ids: &facts[..1],
            }],
            &[],
        )
        .unwrap();

        assert!(nodes::get(&db, id).unwrap().unwrap().embedding.is_some());
        assert_eq!(proof_count(&db, id).unwrap(), 1, "provenance still grew");
    }

    #[test]
    fn the_run_ledger_records_a_round_and_carries_the_watermark() {
        let (db, _facts) = seeded();
        assert_eq!(watermark(&db, "b1").unwrap(), 0, "no runs yet");
        assert!(latest_run(&db, "b1").unwrap().is_none());

        let run = start_run(&db, "b1").unwrap();
        let open = latest_run(&db, "b1").unwrap().unwrap();
        assert_eq!(open.status, "running");
        assert!(open.finished_at.is_none() && open.watermark.is_none());
        assert_eq!(
            watermark(&db, "b1").unwrap(),
            0,
            "an open run contributes nothing"
        );

        finish_run(
            &db,
            run,
            "done",
            RunCounts {
                facts_seen: 4,
                created_n: 2,
                updated_n: 1,
                deleted_n: 0,
                merged_n: 1,
            },
            Some(42),
            None,
        )
        .unwrap();

        let done = latest_run(&db, "b1").unwrap().unwrap();
        assert_eq!(done.status, "done");
        assert_eq!((done.facts_seen, done.created_n, done.merged_n), (4, 2, 1));
        assert_eq!(done.watermark, Some(42));
        assert!(done.finished_at.is_some() && done.error.is_none());
        assert_eq!(watermark(&db, "b1").unwrap(), 42);

        // A later failed run that got partway still moves the mark: its
        // earlier batches were applied, and replaying them would duplicate.
        let failed = start_run(&db, "b1").unwrap();
        finish_run(
            &db,
            failed,
            "failed",
            RunCounts::default(),
            Some(50),
            Some("ollama unreachable"),
        )
        .unwrap();
        assert_eq!(watermark(&db, "b1").unwrap(), 50);
        assert_eq!(
            latest_run(&db, "b1").unwrap().unwrap().error.as_deref(),
            Some("ollama unreachable")
        );

        // ...and a failure with no progress leaves it exactly where it was.
        let nothing = start_run(&db, "b1").unwrap();
        finish_run(
            &db,
            nothing,
            "failed",
            RunCounts::default(),
            None,
            Some("x"),
        )
        .unwrap();
        assert_eq!(watermark(&db, "b1").unwrap(), 50);
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
