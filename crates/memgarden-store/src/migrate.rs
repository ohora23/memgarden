//! Schema migrations. `PRAGMA user_version` is the authority on what has
//! been applied; `schema_migrations` is an audit log only.

use rusqlite::Connection;

use memgarden_core::error::Result;
use memgarden_core::now_ms;

use crate::store_err;

const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/0001_init.sql"))];

/// Applies all pending migrations. Idempotent — migrations already
/// reflected in `PRAGMA user_version` are skipped. Each migration runs in a
/// single transaction (DDL + `schema_migrations` log + `user_version` bump)
/// so a failure never leaves the schema partially applied.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(store_err)?;

    for &(version, sql) in MIGRATIONS {
        if version <= current {
            continue;
        }
        let tx = conn.transaction().map_err(store_err)?;
        tx.execute_batch(sql).map_err(store_err)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, now_ms()],
        )
        .map_err(store_err)?;
        tx.pragma_update(None, "user_version", version)
            .map_err(store_err)?;
        tx.commit().map_err(store_err)?;
    }
    Ok(())
}
