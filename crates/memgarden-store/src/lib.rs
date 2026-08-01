pub mod banks;
mod conn;
mod migrate;
pub mod models;
pub mod nodes;
pub mod search;
pub mod vecblob;

use std::path::Path;

use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Transaction, TransactionBehavior};

use memgarden_core::error::{Error, Result};

/// Fixed pool size: memgardend serves recall/retain requests via
/// `spawn_blocking`, not one connection per request, so a small pool is
/// plenty (see 핵심 결정 in the implementation plan).
const POOL_MAX_SIZE: u32 = 4;

pub struct Db {
    pool: Pool<SqliteConnectionManager>,
}

impl Db {
    /// Opens (creating if missing) a SQLite database file and applies all
    /// pending migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        conn::register_vec_extension();
        Self::from_manager(conn::manager_file(path.as_ref()))
    }

    /// Opens a private shared-cache in-memory database and applies all
    /// pending migrations. Each call gets its own isolated database.
    pub fn open_memory() -> Result<Self> {
        conn::register_vec_extension();
        Self::from_manager(conn::manager_memory())
    }

    fn from_manager(manager: SqliteConnectionManager) -> Result<Self> {
        let pool = Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .build(manager)
            .map_err(store_err)?;
        let mut conn = pool.get().map_err(store_err)?;
        migrate::migrate(&mut conn)?;
        drop(conn);
        Ok(Db { pool })
    }

    /// Read accessor: a pooled connection for SELECT-only work.
    pub fn read(&self) -> Result<PooledConnection<SqliteConnectionManager>> {
        self.pool.get().map_err(store_err)
    }

    /// Write accessor: runs `f` inside a `BEGIN IMMEDIATE` transaction,
    /// committing on `Ok` and rolling back (via `Transaction`'s `Drop`) on
    /// `Err`. IMMEDIATE grabs the write lock up front instead of letting a
    /// deferred transaction try to upgrade mid-flight, which under WAL can
    /// fail with `SQLITE_BUSY_SNAPSHOT` — a case `busy_timeout` does not
    /// retry.
    pub fn write<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.pool.get().map_err(store_err)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err)?;
        let result = f(&tx)?;
        tx.commit().map_err(store_err)?;
        Ok(result)
    }
}

pub(crate) fn store_err(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}
