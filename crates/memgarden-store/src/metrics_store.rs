//! Persistence for the two metrics-adjacent tables: `metric_snapshots`
//! (periodic METRICS.snapshot() dumps, written by memgardend's
//! metrics_task) and `benefit_ledger` (manual + future automatic
//! recall/retain-savings cases, the `/v1/ledger` API).

use rusqlite::params;

use memgarden_core::error::{Error, Result};
use memgarden_core::now_ms;

use crate::models::LedgerEntry;
use crate::{Db, store_err};

/// Inserts one `metric_snapshots` row. `payload` must be a JSON string
/// (the table has `CHECK (json_valid(payload))`).
pub fn insert_snapshot(db: &Db, payload: &str) -> Result<()> {
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "INSERT INTO metric_snapshots (created_at, payload) VALUES (?1, ?2)",
            params![now, payload],
        )
        .map_err(store_err)?;
        Ok(())
    })
}

/// Newest-first `(id, created_at, payload)` rows, capped at `limit`
/// (clamped to `1..=1000` so a caller-supplied 0/negative/huge value can't
/// turn into an unbounded scan).
pub fn recent_snapshots(db: &Db, limit: i64) -> Result<Vec<(i64, i64, String)>> {
    let limit = limit.clamp(1, 1000);
    let conn = db.read()?;
    let mut stmt = conn
        .prepare("SELECT id, created_at, payload FROM metric_snapshots ORDER BY id DESC LIMIT ?1")
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

pub fn insert_ledger(
    db: &Db,
    kind: &str,
    bank_id: Option<&str>,
    detail: Option<&str>,
) -> Result<LedgerEntry> {
    let now = now_ms();
    let id = db.write(|tx| {
        tx.execute(
            "INSERT INTO benefit_ledger (kind, bank_id, detail, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![kind, bank_id, detail, now],
        )
        .map_err(map_ledger_err)?;
        Ok(tx.last_insert_rowid())
    })?;
    Ok(LedgerEntry {
        id,
        kind: kind.to_string(),
        bank_id: bank_id.map(str::to_string),
        detail: detail.map(str::to_string),
        created_at: now,
    })
}

/// Newest-first ledger entries, capped at `limit` (clamped to `1..=1000`,
/// same rationale as `recent_snapshots`).
pub fn list_ledger(db: &Db, limit: i64) -> Result<Vec<LedgerEntry>> {
    let limit = limit.clamp(1, 1000);
    let conn = db.read()?;
    let mut stmt = conn
        .prepare("SELECT id, kind, bank_id, detail, created_at FROM benefit_ledger ORDER BY id DESC LIMIT ?1")
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![limit], row_to_ledger)
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// Maps a `kind` CHECK-constraint violation (or any other constraint
/// violation on this table, e.g. an unknown `bank_id`) to `Error::Invalid`
/// so the API surfaces it as 400, not 500.
fn map_ledger_err(e: rusqlite::Error) -> Error {
    if e.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        return Error::Invalid(format!("invalid ledger entry: {e}"));
    }
    store_err(e)
}

fn row_to_ledger(row: &rusqlite::Row) -> rusqlite::Result<LedgerEntry> {
    Ok(LedgerEntry {
        id: row.get(0)?,
        kind: row.get(1)?,
        bank_id: row.get(2)?,
        detail: row.get(3)?,
        created_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrip() {
        let db = Db::open_memory().unwrap();
        insert_snapshot(&db, r#"{"http_requests":5}"#).unwrap();
        insert_snapshot(&db, r#"{"http_requests":6}"#).unwrap();
        let rows = recent_snapshots(&db, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].2, r#"{"http_requests":6}"#, "newest first");
    }

    #[test]
    fn ledger_roundtrip() {
        let db = Db::open_memory().unwrap();
        let entry = insert_ledger(&db, "manual", None, Some(r#"{"case_text":"x"}"#)).unwrap();
        assert_eq!(entry.kind, "manual");
        let all = list_ledger(&db, 50).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, entry.id);
    }

    #[test]
    fn ledger_invalid_kind_is_invalid_error() {
        let db = Db::open_memory().unwrap();
        let err = insert_ledger(&db, "not_a_real_kind", None, None).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }
}
