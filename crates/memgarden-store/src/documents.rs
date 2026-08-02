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

    fn meta(hash: &str) -> String {
        format!(r#"{{"content_sha256":"{hash}"}}"#)
    }

    #[test]
    fn upsert_inserts_then_reports_unchanged_for_same_hash() {
        let db = Db::open_memory().unwrap();
        banks::create(&db, "b1", None, None).unwrap();

        let first = upsert(&db, "b1", "sess-1", Some("t"), &meta("aaa"), "aaa").unwrap();
        assert!(!first.unchanged);

        let second = upsert(&db, "b1", "sess-1", Some("t"), &meta("aaa"), "aaa").unwrap();
        assert_eq!(second.id, first.id, "same (bank_id, doc_key) -> same row");
        assert!(second.unchanged, "identical content hash must be a no-op");

        let third = upsert(&db, "b1", "sess-1", Some("t2"), &meta("bbb"), "bbb").unwrap();
        assert_eq!(third.id, first.id);
        assert!(!third.unchanged);
        assert_eq!(get_metadata(&db, first.id).unwrap().unwrap(), meta("bbb"));
    }

    #[test]
    fn upsert_is_scoped_per_bank() {
        let db = Db::open_memory().unwrap();
        banks::create(&db, "b1", None, None).unwrap();
        banks::create(&db, "b2", None, None).unwrap();

        let a = upsert(&db, "b1", "sess", None, &meta("aaa"), "aaa").unwrap();
        let b = upsert(&db, "b2", "sess", None, &meta("aaa"), "aaa").unwrap();
        assert_ne!(a.id, b.id);
        assert!(!b.unchanged, "same doc_key in another bank is a new doc");
    }
}
