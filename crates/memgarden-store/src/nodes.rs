use std::str::FromStr;

use rusqlite::{OptionalExtension, Row, params};
use uuid::Uuid;

use memgarden_core::error::Result;
use memgarden_core::now_ms;
use memgarden_core::types::FactType;

use crate::models::{MemoryNode, NewNode};
use crate::{Db, store_err, vecblob};

const SELECT_COLUMNS: &str = "id, uuid, bank_id, document_id, fact_type, text, context, embedding, \
     event_date, occurred_start, occurred_end, mentioned_at, metadata, created_at, updated_at";

pub fn insert(db: &Db, new: NewNode) -> Result<i64> {
    let now = now_ms();
    let uuid = Uuid::now_v7().to_string();
    db.write(|tx| {
        tx.execute(
            "INSERT INTO memory_nodes
             (uuid, bank_id, document_id, fact_type, text, context, event_date,
              occurred_start, occurred_end, mentioned_at, metadata, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            params![
                uuid,
                new.bank_id,
                new.document_id,
                new.fact_type.as_str(),
                new.text,
                new.context,
                new.event_date,
                new.occurred_start,
                new.occurred_end,
                new.mentioned_at,
                new.metadata,
                now,
            ],
        )
        .map_err(store_err)?;
        Ok(tx.last_insert_rowid())
    })
}

pub fn get(db: &Db, id: i64) -> Result<Option<MemoryNode>> {
    let conn = db.read()?;
    let raw: Option<NodeRow> = conn
        .query_row(
            &format!("SELECT {SELECT_COLUMNS} FROM memory_nodes WHERE id = ?1"),
            params![id],
            row_to_node_row,
        )
        .optional()
        .map_err(store_err)?;
    raw.map(NodeRow::into_node).transpose()
}

pub fn count(db: &Db, bank_id: &str) -> Result<i64> {
    let conn = db.read()?;
    conn.query_row(
        "SELECT count(*) FROM memory_nodes WHERE bank_id = ?1",
        params![bank_id],
        |r| r.get(0),
    )
    .map_err(store_err)
}

pub fn delete(db: &Db, id: i64) -> Result<()> {
    db.write(|tx| {
        tx.execute("DELETE FROM memory_nodes WHERE id = ?1", params![id])
            .map_err(store_err)?;
        Ok(())
    })
}

/// Writes the embedding BLOB on `memory_nodes` and upserts the matching
/// `vec_nodes` row (vec0 has no native upsert, so this deletes then inserts).
pub fn set_embedding(db: &Db, node_id: i64, bank_id: &str, embedding: &[f32]) -> Result<()> {
    let blob = vecblob::encode(embedding)?;
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "UPDATE memory_nodes SET embedding = ?1, updated_at = ?2 WHERE id = ?3",
            params![blob, now, node_id],
        )
        .map_err(store_err)?;
        tx.execute("DELETE FROM vec_nodes WHERE rowid = ?1", params![node_id])
            .map_err(store_err)?;
        tx.execute(
            "INSERT INTO vec_nodes (rowid, bank_id, embedding) VALUES (?1, ?2, ?3)",
            params![node_id, bank_id, blob],
        )
        .map_err(store_err)?;
        Ok(())
    })
}

/// `(id, bank_id, text, occurred_start, occurred_end, mentioned_at)` — the
/// temporal fields the backlog worker needs for `embed::augment_for_embedding`
/// (decision #2's `date = occurred_start ?? mentioned_at` and range case).
pub type PendingEmbeddingRow = (i64, String, String, Option<i64>, Option<i64>, Option<i64>);

/// Nodes with no embedding yet (`embedding IS NULL`), driving
/// `idx_memory_nodes_embed_backlog` (`0001_init.sql:52`).
pub fn pending_embeddings(db: &Db, limit: usize) -> Result<Vec<PendingEmbeddingRow>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, bank_id, text, occurred_start, occurred_end, mentioned_at
             FROM memory_nodes WHERE embedding IS NULL LIMIT ?1",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ))
        })
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// Batch variant of `set_embedding`: one `BEGIN IMMEDIATE` for the whole
/// batch (called once per backlog-worker tick, not per node), same
/// delete-then-insert shape since vec0 has no native upsert.
pub fn set_embeddings_batch(db: &Db, batch: &[(i64, String, Vec<f32>)]) -> Result<()> {
    let now = now_ms();
    db.write(|tx| {
        for (node_id, bank_id, embedding) in batch {
            let blob = vecblob::encode(embedding)?;
            tx.execute(
                "UPDATE memory_nodes SET embedding = ?1, updated_at = ?2 WHERE id = ?3",
                params![blob, now, node_id],
            )
            .map_err(store_err)?;
            tx.execute("DELETE FROM vec_nodes WHERE rowid = ?1", params![node_id])
                .map_err(store_err)?;
            tx.execute(
                "INSERT INTO vec_nodes (rowid, bank_id, embedding) VALUES (?1, ?2, ?3)",
                params![node_id, bank_id, blob],
            )
            .map_err(store_err)?;
        }
        Ok(())
    })
}

pub fn add_tags(db: &Db, node_id: i64, tags: &[&str]) -> Result<()> {
    db.write(|tx| {
        for tag in tags {
            tx.execute(
                "INSERT OR IGNORE INTO node_tags (node_id, tag) VALUES (?1, ?2)",
                params![node_id, tag],
            )
            .map_err(store_err)?;
        }
        Ok(())
    })
}

pub fn tags_of(db: &Db, node_id: i64) -> Result<Vec<String>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare("SELECT tag FROM node_tags WHERE node_id = ?1 ORDER BY tag")
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![node_id], |r| r.get::<_, String>(0))
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// Raw row shape before `fact_type` is parsed into `FactType` — row-mapping
/// closures must return `rusqlite::Result`, so parsing happens after.
struct NodeRow {
    id: i64,
    uuid: String,
    bank_id: String,
    document_id: Option<i64>,
    fact_type: String,
    text: String,
    context: Option<String>,
    embedding: Option<Vec<u8>>,
    event_date: Option<i64>,
    occurred_start: Option<i64>,
    occurred_end: Option<i64>,
    mentioned_at: Option<i64>,
    metadata: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl NodeRow {
    fn into_node(self) -> Result<MemoryNode> {
        Ok(MemoryNode {
            id: self.id,
            uuid: self.uuid,
            bank_id: self.bank_id,
            document_id: self.document_id,
            fact_type: FactType::from_str(&self.fact_type)?,
            text: self.text,
            context: self.context,
            embedding: self.embedding,
            event_date: self.event_date,
            occurred_start: self.occurred_start,
            occurred_end: self.occurred_end,
            mentioned_at: self.mentioned_at,
            metadata: self.metadata,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

fn row_to_node_row(row: &Row) -> rusqlite::Result<NodeRow> {
    Ok(NodeRow {
        id: row.get(0)?,
        uuid: row.get(1)?,
        bank_id: row.get(2)?,
        document_id: row.get(3)?,
        fact_type: row.get(4)?,
        text: row.get(5)?,
        context: row.get(6)?,
        embedding: row.get(7)?,
        event_date: row.get(8)?,
        occurred_start: row.get(9)?,
        occurred_end: row.get(10)?,
        mentioned_at: row.get(11)?,
        metadata: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}
