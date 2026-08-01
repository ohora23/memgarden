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
        .map_err(store_err)?;
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

/// Overwrites `mission`/`disposition` (each `None` sets the column to
/// `NULL`) and bumps `updated_at`.
pub fn update(
    db: &Db,
    bank_id: &str,
    mission: Option<&str>,
    disposition: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    db.write(|tx| {
        let changed = tx
            .execute(
                "UPDATE banks SET mission = ?1, disposition = ?2, updated_at = ?3 WHERE bank_id = ?4",
                params![mission, disposition, now, bank_id],
            )
            .map_err(store_err)?;
        if changed == 0 {
            return Err(Error::NotFound(format!("bank {bank_id}")));
        }
        Ok(())
    })
}

pub fn delete(db: &Db, bank_id: &str) -> Result<()> {
    db.write(|tx| {
        tx.execute("DELETE FROM banks WHERE bank_id = ?1", params![bank_id])
            .map_err(store_err)?;
        Ok(())
    })
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
