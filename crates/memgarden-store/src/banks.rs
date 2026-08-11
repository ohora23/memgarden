use rusqlite::{OptionalExtension, params};

use memgarden_core::error::{Error, Result};
use memgarden_core::now_ms;

use crate::models::{Bank, BankStats};
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

/// Per-bank inventory for the E5 dashboard: what each bank actually holds.
///
/// One statement rather than four, because the dashboard polls this every
/// 10 s and four round trips would each re-walk `banks`. The three aggregate
/// subqueries are `LEFT JOIN`ed onto `banks` so a bank with nothing in it
/// still appears, as zeros — an empty bank is a fact worth seeing, and it is
/// how a mis-typed `bank_id` in a hook config becomes visible.
///
/// `links` has no `bank_id` of its own (see migrations/0001_init.sql), so it
/// is counted through `from_node_id`. That means an edge is attributed to the
/// bank of its *source* node; links never cross banks, so the two readings
/// agree.
///
/// Measured on the live database (5,528 nodes, 205,961 links): 46 ms, of
/// which the link join is all but 0.4 ms. Fine for a 10 s poll, and the
/// reason `/v1/stats` skips the timing middleware — see routes::router.
pub fn stats(db: &Db) -> Result<Vec<BankStats>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT b.bank_id,
                    COALESCE(n.nodes, 0), COALESCE(n.world, 0),
                    COALESCE(n.observation, 0), COALESCE(n.experience, 0),
                    COALESCE(n.unembedded, 0),
                    COALESCE(d.documents, 0), COALESCE(l.links, 0)
             FROM banks b
             LEFT JOIN (SELECT bank_id,
                               COUNT(*) AS nodes,
                               SUM(fact_type = 'world') AS world,
                               SUM(fact_type = 'observation') AS observation,
                               SUM(fact_type = 'experience') AS experience,
                               SUM(embedding IS NULL) AS unembedded
                        FROM memory_nodes GROUP BY bank_id) n
                    ON n.bank_id = b.bank_id
             LEFT JOIN (SELECT bank_id, COUNT(*) AS documents
                        FROM documents GROUP BY bank_id) d
                    ON d.bank_id = b.bank_id
             LEFT JOIN (SELECT src.bank_id, COUNT(*) AS links
                        FROM links l
                        JOIN memory_nodes src ON src.id = l.from_node_id
                        GROUP BY src.bank_id) l
                    ON l.bank_id = b.bank_id
             ORDER BY b.bank_id",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(BankStats {
                bank_id: row.get(0)?,
                nodes: row.get(1)?,
                world: row.get(2)?,
                observation: row.get(3)?,
                experience: row.get(4)?,
                unembedded: row.get(5)?,
                documents: row.get(6)?,
                links: row.get(7)?,
            })
        })
        .map_err(store_err)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{self, NewLink};
    use crate::models::NewNode;
    use crate::nodes;
    use memgarden_core::types::FactType;

    /// Counts land in the right bank and the right type column, an empty
    /// bank still appears, and a link is attributed once — not once per
    /// endpoint.
    #[test]
    fn stats_counts_per_bank_and_keeps_empty_banks() {
        let db = Db::open_memory().unwrap();
        create(&db, "full", None, None).unwrap();
        create(&db, "empty", None, None).unwrap();

        let a = nodes::insert(&db, NewNode::new("full", FactType::World, "a")).unwrap();
        let b = nodes::insert(&db, NewNode::new("full", FactType::Observation, "b")).unwrap();
        nodes::insert(&db, NewNode::new("full", FactType::Experience, "c")).unwrap();
        nodes::insert(&db, NewNode::new("empty", FactType::World, "elsewhere")).unwrap();
        // Deleted again, so `empty` is a bank that has held nodes and holds
        // none now — the LEFT JOIN case, not merely a never-used bank.
        db.write(|tx| {
            tx.execute("DELETE FROM memory_nodes WHERE bank_id = 'empty'", [])
                .unwrap();
            Ok(())
        })
        .unwrap();

        graph::insert_links(
            &db,
            &[NewLink {
                from_node_id: a,
                to_node_id: b,
                link_type: "semantic",
                weight: 0.5,
            }],
            memgarden_core::now_ms(),
        )
        .unwrap();

        let stats = stats(&db).unwrap();
        assert_eq!(stats.len(), 2, "every bank appears");

        let empty = &stats[0];
        assert_eq!(empty.bank_id, "empty");
        assert_eq!((empty.nodes, empty.documents, empty.links), (0, 0, 0));

        let full = &stats[1];
        assert_eq!(full.bank_id, "full");
        assert_eq!(full.nodes, 3);
        assert_eq!((full.world, full.observation, full.experience), (1, 1, 1));
        assert_eq!(full.links, 1, "one row, counted at its source only");
        assert_eq!(full.unembedded, 3, "nothing embedded yet");
    }
}
