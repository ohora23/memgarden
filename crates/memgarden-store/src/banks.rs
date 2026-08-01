use rusqlite::{OptionalExtension, params};

use memgarden_core::error::{Error, Result};
use memgarden_core::now_ms;

use crate::models::Bank;
use crate::{Db, store_err};

pub fn create(
    db: &Db,
    bank_id: &str,
    mission: Option<&str>,
    disposition: Option<&str>,
) -> Result<Bank> {
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "INSERT INTO banks (bank_id, mission, disposition, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![bank_id, mission, disposition, now],
        )
        .map_err(|e| map_bank_err(e, bank_id))?;
        Ok(())
    })?;
    Ok(Bank {
        bank_id: bank_id.to_string(),
        mission: mission.map(str::to_string),
        disposition: disposition.map(str::to_string),
        created_at: now,
        updated_at: now,
    })
}

/// All banks, ordered by `bank_id`.
pub fn list(db: &Db) -> Result<Vec<Bank>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare("SELECT bank_id, mission, disposition, created_at, updated_at FROM banks ORDER BY bank_id")
        .map_err(store_err)?;
    let rows = stmt.query_map([], row_to_bank).map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

pub fn get(db: &Db, bank_id: &str) -> Result<Option<Bank>> {
    let conn = db.read()?;
    conn.query_row(
        "SELECT bank_id, mission, disposition, created_at, updated_at FROM banks WHERE bank_id = ?1",
        params![bank_id],
        row_to_bank,
    )
    .optional()
    .map_err(store_err)
}

/// Partial update: `None` leaves the column untouched, `Some(None)` sets it
/// to `NULL`, `Some(Some(v))` sets it to `v`. Builds a dynamic (but still
/// fully parameter-bound) `SET` clause covering only the columns actually
/// present in the request, and always bumps `updated_at`.
pub fn update(
    db: &Db,
    bank_id: &str,
    mission: Option<Option<&str>>,
    disposition: Option<Option<&str>>,
) -> Result<()> {
    let now = now_ms();
    db.write(|tx| {
        let mut sets = vec!["updated_at = ?1".to_string()];
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&now];
        if let Some(m) = &mission {
            sets.push(format!("mission = ?{}", values.len() + 1));
            values.push(m);
        }
        if let Some(d) = &disposition {
            sets.push(format!("disposition = ?{}", values.len() + 1));
            values.push(d);
        }
        let bank_id_idx = values.len() + 1;
        values.push(&bank_id);
        let sql = format!(
            "UPDATE banks SET {} WHERE bank_id = ?{bank_id_idx}",
            sets.join(", ")
        );

        let changed = tx
            .execute(&sql, values.as_slice())
            .map_err(|e| map_bank_err(e, bank_id))?;
        if changed == 0 {
            return Err(Error::NotFound(format!("bank {bank_id}")));
        }
        Ok(())
    })
}

pub fn delete(db: &Db, bank_id: &str) -> Result<usize> {
    db.write(|tx| {
        tx.execute("DELETE FROM banks WHERE bank_id = ?1", params![bank_id])
            .map_err(store_err)
    })
}

/// Maps a rusqlite constraint violation to the right API-level error:
/// `UNIQUE`/`PRIMARY KEY` (duplicate `bank_id`) -> `Error::Conflict`;
/// `CHECK` (e.g. invalid `disposition` JSON) -> `Error::Invalid`. Any other
/// error falls through to the generic storage mapping.
fn map_bank_err(e: rusqlite::Error, bank_id: &str) -> Error {
    if let rusqlite::Error::SqliteFailure(ref ffi_err, _) = e {
        match ffi_err.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
            | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => {
                return Error::Conflict(format!("bank {bank_id} already exists"));
            }
            rusqlite::ffi::SQLITE_CONSTRAINT_CHECK => {
                return Error::Invalid(format!("bank {bank_id}: json_valid or enum check failed"));
            }
            _ => {}
        }
    }
    store_err(e)
}

fn row_to_bank(row: &rusqlite::Row) -> rusqlite::Result<Bank> {
    Ok(Bank {
        bank_id: row.get(0)?,
        mission: row.get(1)?,
        disposition: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}
