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

pub use conn::register_vec_extension;
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
    /// Set only by `open_memory`: the throwaway file to remove on drop.
    temp_path: Option<std::path::PathBuf>,
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Some(p) = self.temp_path.take() {
            let side = |suffix: &str| std::path::PathBuf::from(format!("{}{suffix}", p.display()));
            let _ = std::fs::remove_file(side("-wal"));
            let _ = std::fs::remove_file(side("-shm"));
            let _ = std::fs::remove_file(&p);
        }
    }
}

impl Db {
    /// Opens (creating if missing) a SQLite database file and applies all
    /// pending migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        conn::register_vec_extension();
        Self::from_manager(conn::manager_file(path.as_ref()))
    }

    /// Opens a private throwaway database and applies all pending migrations.
    /// Each call gets its own, removed when the `Db` drops.
    ///
    /// **On disk, not in memory, and the difference is the point.**
    /// `r2d2_sqlite::memory()` opens `file:{uuid}?mode=memory&cache=shared`,
    /// and SQLite silently refuses WAL for an in-memory database — measured:
    /// `PRAGMA journal_mode=WAL` returns `memory`, so `init_pragmas`' request
    /// is discarded without an error. Every test therefore ran on
    /// shared-cache table-level locking, where a read concurrent with a write
    /// fails `SQLITE_LOCKED` **immediately** (`busy_timeout` does not cover
    /// that lock class) while the same pair under the file+WAL production
    /// path passes untouched. A test suite must exercise the locking model it
    /// ships with.
    ///
    /// // ponytail: costs the workspace suite 28.0s -> 37.9s, because
    /// // `std::env::temp_dir()` is ext4 here and 828 tests each create and
    /// // fsync a database. Putting these on `/dev/shm` recovers it and is
    /// // five lines; not taken, because it buys ten seconds with a
    /// // Linux-only branch. Do it when ten seconds is the thing that hurts.
    pub fn open_memory() -> Result<Self> {
        conn::register_vec_extension();
        let path = std::env::temp_dir().join(format!("memgarden-tmp-{}.db", uuid::Uuid::new_v4()));
        Self::build(conn::manager_file(&path), Some(path))
    }

    fn from_manager(manager: SqliteConnectionManager) -> Result<Self> {
        Self::build(manager, None)
    }

    fn build(
        manager: SqliteConnectionManager,
        temp_path: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        let pool = Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .connection_timeout(Duration::from_secs(3))
            .build(manager)
            .map_err(store_err)?;
        let mut conn = pool.get().map_err(store_err)?;
        migrate::migrate(&mut conn)?;
        drop(conn);
        Ok(Db { pool, temp_path })
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
    //! Measured 2026-08-07 on a Ryzen 7 9800X3D (16 threads): **6 of 32
    //! processes died**, with SIGSEGV inside FTS5's index merge or SQLite's
    //! allocator. There is no `migrate` code here, no links, and no reopen — a
    //! file-backed `Db` and one large `insert_batch` of FTS5-bearing rows is
    //! the whole shape.
    //!
    //! # It no longer reproduces, and that is a finding rather than a fix
    //!
    //! Re-measured 2026-08-09 on the same machine, same harness: **0 of 32**.
    //! So did the pre-reduction variant this was cut down from — the one that
    //! also wrote ~3,000 links in a second transaction and reopened the
    //! database — at **0 of 32**.
    //!
    //! The defect itself is not gone. `cargo test --workspace` died **2 of 8**
    //! the same afternoon. What changed is where the reproducing boundary sits:
    //! it is the whole workspace run, whose test binaries cargo schedules
    //! concurrently, and not any single binary. `memgardend`'s own lib tests at
    //! 4 processes x 16 threads are 0 of 16.
    //!
    //! **Do not read this module as evidence that the corruption is the
    //! store's.** That conclusion rested on this probe reproducing on its own,
    //! and today it does not. `book/src/roadmap.md` carries the current state.
    //!
    //! Kept, rather than deleted, because a probe that stopped reproducing is
    //! itself a measurement — and because the shape it isolates is still the
    //! cheapest thing to re-run when the conditions are next understood.
    //!
    //! # Why it went quiet, and what that separated out
    //!
    //! This probe opens a **file** database, and until now every integration
    //! test opened `Db::open_memory` — which was a shared-cache in-memory
    //! database with no WAL. So the probe going quiet while the workspace run
    //! kept dying was consistent all along: the two were not running the same
    //! storage model. `open_memory` is now file+WAL like this probe and like
    //! production.
    //!
    //! That change was measured against the shape that reproduces, `retain_api`
    //! at 4 concurrent processes x 32 threads, interleaved so both arms take
    //! the same machine load, 160 runs each:
    //!
    //! | | `SQLITE_LOCKED` | anything else |
    //! |---|---|---|
    //! | shared-cache in-memory | **5** | 1 (SIGSEGV) |
    //! | file + WAL | **0** | 1 (a corrupt schema text) |
    //!
    //! **Two defects, not one.** The `SQLITE_LOCKED` arm is closed, by
    //! mechanism as much as by count: `await_job`'s poll died on
    //! `database table is locked: retain_jobs`, and a read cannot take that
    //! error under WAL. The other arm is not. One death on each side was a
    //! value read back wrong — a SIGSEGV on one, `error in table sessions
    //! after add column: near "\n  ": syntax error` on the other, and the
    //! `Store { message: "malformed JSON" }` recorded on 2026-08-09 is the
    //! same shape. Whatever that is, it survives the storage change, and this
    //! probe is still the cheapest place to look for it.
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

#[cfg(test)]
mod test_database_matches_production {
    //! `Db::open_memory` is what 73 call sites test against. These two pin the
    //! properties that made it *not* a test of what ships.

    use super::*;

    /// The guard. Revert `open_memory` to `r2d2_sqlite::memory()` and this
    /// reads `memory`, because SQLite refuses WAL for an in-memory database
    /// and `pragma_update` never looks at the answer.
    ///
    /// The cost of that divergence was not theoretical. Under shared-cache
    /// locking a read concurrent with a write fails `SQLITE_LOCKED`
    /// immediately — `busy_timeout` does not cover that lock class — which is
    /// how `retain_api`'s `await_job` poll died with `database table is
    /// locked: retain_jobs` under load, on a pair the file+WAL production path
    /// passes untouched. Measured over 80 concurrent runs of each: 2 deaths on
    /// shared cache, 0 on this.
    #[test]
    fn open_memory_runs_the_production_journal_mode() {
        let db = Db::open_memory().unwrap();
        let mode: String = db
            .read()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            mode, "wal",
            "the test database must run the locking model that ships"
        );
    }

    /// A file-backed test database is only acceptable if it cleans up after
    /// itself — 828 tests leaking three files each would fill the temp dir.
    #[test]
    fn the_throwaway_database_and_its_sidecars_are_removed_on_drop() {
        let db = Db::open_memory().unwrap();
        let path = db.temp_path.clone().expect("open_memory sets a temp path");
        db.write(|tx| {
            tx.execute("CREATE TABLE probe (x)", [])
                .map_err(store_err)?;
            Ok(())
        })
        .unwrap();
        assert!(
            path.exists(),
            "the database file exists while the Db is live"
        );
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let side = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
            assert!(!side.exists(), "{} outlived the Db", side.display());
        }
    }
}
