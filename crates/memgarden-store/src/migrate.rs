//! Schema migrations. `PRAGMA user_version` is the authority on what has
//! been applied; `schema_migrations` is an audit log only.

use rusqlite::{Connection, TransactionBehavior};

use memgarden_core::error::Result;
use memgarden_core::now_ms;

use crate::store_err;

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_retain_jobs.sql")),
    (3, include_str!("../migrations/0003_entities_graph.sql")),
    (4, include_str!("../migrations/0004_consolidation.sql")),
    (5, include_str!("../migrations/0005_embedding_model.sql")),
    (6, include_str!("../migrations/0006_mental_models.sql")),
    (7, include_str!("../migrations/0007_sessions.sql")),
    (
        8,
        include_str!("../migrations/0008_retain_cursor_range.sql"),
    ),
    (
        9,
        include_str!("../migrations/0009_retain_partial_status.sql"),
    ),
    (
        10,
        include_str!("../migrations/0010_mental_model_usage.sql"),
    ),
    (11, include_str!("../migrations/0011_supersession.sql")),
    (12, include_str!("../migrations/0012_task_ledger.sql")),
    (13, include_str!("../migrations/0013_drop_ledger_done.sql")),
];

/// The schema version this build expects, i.e. the highest entry in
/// `MIGRATIONS`. `/healthz` reports it; `tests/schema.rs` asserts a fresh DB
/// and a DB opened at an older `user_version` both land here.
pub const LATEST_VERSION: i64 = MIGRATIONS[MIGRATIONS.len() - 1].0;

/// Applies all pending migrations. Idempotent — migrations already
/// reflected in `PRAGMA user_version` are skipped. Each migration opens its
/// own `BEGIN IMMEDIATE` transaction (grabbing the write lock up front) and
/// re-reads `PRAGMA user_version` *inside* that transaction before deciding
/// whether to apply: two processes racing `Db::open` on the same file will
/// serialize on the write lock, and the second one to acquire it sees the
/// first one's already-committed version and skips, so the DDL is never
/// double-applied.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    for &(version, sql) in MIGRATIONS {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err)?;
        let current: i64 = tx
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(store_err)?;
        if version <= current {
            continue;
        }
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
