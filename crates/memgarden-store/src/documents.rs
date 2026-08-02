//! `documents` upsert keyed on `(bank_id, doc_key)`, with exact-content
//! dedup.
//!
//! Legacy retain dedups a re-sent document on an exact SHA-256 of its
//! content and nothing else — never on cosine similarity (port brief gotcha
//! #5). The hash lives inside the existing `documents.metadata` JSON under
//! `content_sha256` rather than in a dedicated column: `0002` is scoped to
//! `retain_jobs` by the plan, and SQLite's own `json_extract` reads the key
//! without this crate needing a JSON dependency.

use rusqlite::{OptionalExtension, params};

use memgarden_core::error::Result;
use memgarden_core::now_ms;

use crate::{Db, store_err};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocUpsert {
    pub id: i64,
    /// `true` when a row already existed with the same `content_sha256`, i.e.
    /// the caller re-sent byte-identical content and must skip extraction.
    pub unchanged: bool,
}

/// Inserts or updates the `(bank_id, doc_key)` document.
///
/// `metadata` must be a JSON object string that already carries
/// `"content_sha256": <content_hash>` — the caller builds it (it owns the
/// JSON library), this function only compares. Returns `unchanged: true`
/// without writing anything when the stored hash already matches.
pub fn upsert(
    db: &Db,
    bank_id: &str,
    doc_key: &str,
    title: Option<&str>,
    metadata: &str,
    content_hash: &str,
) -> Result<DocUpsert> {
    let now = now_ms();
    db.write(|tx| {
        // Read + compare + write in ONE `BEGIN IMMEDIATE` transaction: two
        // concurrent retains of the same session would otherwise both see
        // "changed" and both extract.
        let existing: Option<(i64, Option<String>)> = tx
            .query_row(
                "SELECT id, json_extract(metadata, '$.content_sha256')
                 FROM documents WHERE bank_id = ?1 AND doc_key = ?2",
                params![bank_id, doc_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(store_err)?;

        if let Some((id, stored_hash)) = existing {
            if stored_hash.as_deref() == Some(content_hash) {
                return Ok(DocUpsert {
                    id,
                    unchanged: true,
                });
            }
            tx.execute(
                "UPDATE documents SET title = ?1, metadata = ?2, updated_at = ?3 WHERE id = ?4",
                params![title, metadata, now, id],
            )
            .map_err(store_err)?;
            return Ok(DocUpsert {
                id,
                unchanged: false,
            });
        }

        tx.execute(
            "INSERT INTO documents (bank_id, doc_key, title, metadata, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![bank_id, doc_key, title, metadata, now],
        )
        .map_err(store_err)?;
        Ok(DocUpsert {
            id: tx.last_insert_rowid(),
            unchanged: false,
        })
    })
}

/// Stamps `metadata.content_sha256` on an existing document.
///
/// Split out from `upsert` on purpose (review HIGH 1): the hash means "this
/// exact content is fully ingested", which is only true once the retain job
/// has finished cleanly. Writing it at upsert time turned any partially
/// failed job into a permanent "duplicate" for that transcript.
pub fn set_content_hash(db: &Db, id: i64, content_hash: &str) -> Result<()> {
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "UPDATE documents
             SET metadata = json_set(coalesce(metadata, '{}'), '$.content_sha256', ?1),
                 updated_at = ?2
             WHERE id = ?3",
            params![content_hash, now, id],
        )
        .map_err(store_err)?;
        Ok(())
    })
}

pub fn get_metadata(db: &Db, id: i64) -> Result<Option<String>> {
    let conn = db.read()?;
    conn.query_row(
        "SELECT metadata FROM documents WHERE id = ?1",
        params![id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .map_err(store_err)
    .map(Option::flatten)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::banks;

    const META: &str = r#"{"kind":"transcript"}"#;

    #[test]
    fn unchanged_only_after_the_hash_is_stamped() {
        let db = Db::open_memory().unwrap();
        banks::create(&db, "b1", None, None).unwrap();

        let first = upsert(&db, "b1", "sess-1", Some("t"), META, "aaa").unwrap();
        assert!(!first.unchanged);

        // The job has not finished, so no hash is stored yet: re-sending the
        // same content must NOT be dismissed as a duplicate (review HIGH 1).
        let retry = upsert(&db, "b1", "sess-1", Some("t"), META, "aaa").unwrap();
        assert_eq!(retry.id, first.id);
        assert!(!retry.unchanged, "an un-ingested document is not a duplicate");

        set_content_hash(&db, first.id, "aaa").unwrap();
        let third = upsert(&db, "b1", "sess-1", Some("t"), META, "aaa").unwrap();
        assert!(third.unchanged, "a fully ingested document IS a duplicate");

        // Different content -> not a duplicate, and the stale hash is gone.
        let fourth = upsert(&db, "b1", "sess-1", Some("t2"), META, "bbb").unwrap();
        assert!(!fourth.unchanged);
        assert!(
            !get_metadata(&db, first.id).unwrap().unwrap().contains("aaa"),
            "a content change must clear the previous hash"
        );
    }

    #[test]
    fn set_content_hash_preserves_other_metadata() {
        let db = Db::open_memory().unwrap();
        banks::create(&db, "b1", None, None).unwrap();
        let doc = upsert(&db, "b1", "sess", None, r#"{"files_modified":"a.rs"}"#, "h").unwrap();
        set_content_hash(&db, doc.id, "h").unwrap();
        let meta = get_metadata(&db, doc.id).unwrap().unwrap();
        assert!(meta.contains("\"files_modified\":\"a.rs\""));
        assert!(meta.contains("\"content_sha256\":\"h\""));
    }

    #[test]
    fn upsert_is_scoped_per_bank() {
        let db = Db::open_memory().unwrap();
        banks::create(&db, "b1", None, None).unwrap();
        banks::create(&db, "b2", None, None).unwrap();

        let a = upsert(&db, "b1", "sess", None, META, "aaa").unwrap();
        set_content_hash(&db, a.id, "aaa").unwrap();
        let b = upsert(&db, "b2", "sess", None, META, "aaa").unwrap();
        assert_ne!(a.id, b.id);
        assert!(!b.unchanged, "same doc_key in another bank is a new doc");
    }
}
