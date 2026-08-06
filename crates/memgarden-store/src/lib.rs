pub mod banks;
mod conn;
pub mod consolidate;
pub mod documents;
pub mod graph;
pub mod mental_models;
pub mod metrics_store;
mod migrate;
pub mod models;
pub mod nodes;
pub mod retain_jobs;
pub mod search;
pub mod sessions;
pub mod vecblob;

pub use migrate::LATEST_VERSION;

use std::path::Path;
use std::time::Duration;

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
            .connection_timeout(Duration::from_secs(3))
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

#[cfg(test)]
mod heap_corruption_repro {
    //! The minimal reproducer for the intermittent SIGSEGV documented in
    //! `book/src/roadmap.md` — **committed rather than described**, because
    //! the conclusion it supports is "the corruption is the store's, not the
    //! migration's", and a measurement nobody can re-run is not evidence.
    //!
    //! It is `#[ignore]`d because it does not fail on its own: the corruption
    //! needs concurrent load, so the harness is *outside* the test —
    //!
    //! ```text
    //! cargo test -p memgarden-store --lib --no-run
    //! BIN=$(ls -t target/debug/deps/memgarden_store-* | grep -v '\.d$' | head -1)
    //! for r in 1 2 3 4; do
    //!   for i in $(seq 8); do $BIN --ignored --test-threads=32 heap_corruption_repro & done
    //!   wait
    //! done
    //! ```
    //!
    //! Measured on a Ryzen 7 9800X3D (16 threads): **6 of 32 processes died**,
    //! with SIGSEGV inside FTS5's index merge or SQLite's allocator. There is
    //! no `migrate` code here, no links, and no reopen — a file-backed `Db`
    //! and one large `insert_batch` of FTS5-bearing rows is the whole shape.
    //!
    //! This is the input an ASAN build needs. Delete it when the defect is
    //! closed, not before.
    use memgarden_core::types::FactType;

    fn one(seed: usize) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::Db::open(dir.path().join("p.db")).unwrap();
        crate::banks::create(&db, "b", None, None).unwrap();
        let texts: Vec<String> = (0..150)
            .map(|r| format!("db {seed} row {r} lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua ut enim ad minim veniam quis nostrud"))
            .collect();
        let items: Vec<crate::nodes::NewNodeWithTags> = texts
            .iter()
            .map(|t| crate::nodes::NewNodeWithTags {
                node: crate::models::NewNode::new("b", FactType::World, t),
                tags: &[],
            })
            .collect();
        assert_eq!(crate::nodes::insert_batch(&db, &items).unwrap().len(), 150);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn a1() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn a2() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn a3() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn a4() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn a5() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn a6() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn a7() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn a8() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn a9() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn b1() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn b2() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn b3() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn b4() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn b5() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn b6() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn b7() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn b8() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn b9() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn c1() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn c2() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn c3() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn c4() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn c5() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn c6() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn c7() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn c8() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn c9() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn d1() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn d2() {
        one(2);
    }
    #[test]
    #[ignore = "needs the concurrent-process harness in this module's docs"]
    fn d3() {
        one(2);
    }
}
