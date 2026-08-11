//! The bisect for the corruption `heap_corruption_repro` reproduces: **is
//! `sqlite-vec` in it, or is it SQLite and FTS5 alone?**
//!
//! `heap_corruption_repro` goes through `Db`, so it registers `vec0` as a
//! process-wide auto-extension and its migrations create a `vec_nodes` virtual
//! table. This probe deliberately does neither: raw `rusqlite`, the same pool
//! shape, the same WAL pragmas, the same FTS5-bearing insert load, and no
//! `sqlite-vec` anywhere in the process.
//!
//! **Read any number this produces against `roadmap.md`'s warning first.** The
//! reproduction rate for this failure was measured moving between 0% and 30%
//! across one day for an unchanged binary, so two runs taken an hour apart
//! compare nothing. Only same-session interleaved arms — both conditions in the
//! same rounds, under the same machine load — are worth reporting, and even
//! those only while the failure is firing at all.
//!
//! Read it against its sibling, at the same run count and the same
//! concurrency:
//!
//! * both reproduce → SQLite core / FTS5 or how we drive them;
//! * only `heap_corruption_repro` does → `sqlite-vec` is implicated, which
//!   would matter because it is a third-party C extension pinned at `=0.1.9`
//!   and is by far the least battle-tested C in the process.
//!
//! Ignored for the same reason its sibling is — the corruption needs
//! concurrent load, so the harness is outside the test:
//!
//! ```text
//! cargo test -p memgarden-store --test no_vec_probe --no-run
//! BIN=$(ls -t target/debug/deps/no_vec_probe-* | grep -v '\.d$' | head -1)
//! for r in $(seq 100); do
//!   for i in $(seq 8); do $BIN --ignored --test-threads=32 & done; wait
//! done
//! ```

use std::time::Duration;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

/// The same pragmas `conn::init_pragmas` applies, in the same order. Kept as a
/// copy rather than reached for through the crate, because the point of this
/// probe is to build a database **without** going through `Db` at all.
///
/// Two of them are env-tunable so a bisect can move one pragma at a time from
/// the *same binary* — a second build would vary the code layout as well as
/// the setting, and this failure is sensitive enough to timing that the
/// comparison has to be clean:
///
/// * `PROBE_MMAP_MB` (default 256, production's value; 0 disables mmap)
/// * `PROBE_CACHE_MB` (default 64, production's value)
fn init_pragmas(conn: &mut rusqlite::Connection) -> rusqlite::Result<()> {
    let mb = |k: &str, d: i64| -> i64 {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "mmap_size", mb("PROBE_MMAP_MB", 256) * 1024 * 1024)?;
    conn.pragma_update(None, "cache_size", -mb("PROBE_CACHE_MB", 64) * 1024)?;
    Ok(())
}

/// One database's worth of load: open a 4-connection pool on a fresh file,
/// create an FTS5 index over a text column, and commit 150 long rows in one
/// transaction. This is `heap_corruption_repro::one` with `vec0` removed.
fn one(seed: usize) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.db");
    let pool = Pool::builder()
        .max_size(4)
        .connection_timeout(Duration::from_secs(3))
        .build(SqliteConnectionManager::file(&path).with_init(init_pragmas))
        .unwrap();

    // `PROBE_FTS=0` drops the FTS5 index and its trigger, leaving an ordinary
    // table under the same pool, pragmas and insert count — the bisect that
    // separates SQLite's core from FTS5. Note it also removes roughly half the
    // write work, so a *lower* rate is expected either way; only a rate of
    // zero says FTS5 is required.
    let fts = std::env::var("PROBE_FTS").map(|v| v != "0").unwrap_or(true);
    {
        let conn = pool.get().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, text TEXT NOT NULL);")
            .unwrap();
        if fts {
            conn.execute_batch(
                "CREATE VIRTUAL TABLE t_fts USING fts5(
                   text, content='t', content_rowid='id',
                   tokenize='unicode61', prefix='2 3 4');
                 CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN
                   INSERT INTO t_fts(rowid, text) VALUES (new.id, new.text);
                 END;",
            )
            .unwrap();
        }
    }

    let mut conn = pool.get().unwrap();
    let tx = conn.transaction().unwrap();
    for r in 0..150 {
        tx.execute(
            "INSERT INTO t (text) VALUES (?1)",
            rusqlite::params![format!(
                "db {seed} row {r} lorem ipsum dolor sit amet consectetur adipiscing elit sed do \
                 eiusmod tempor incididunt ut labore et dolore magna aliqua ut enim ad minim \
                 veniam quis nostrud"
            )],
        )
        .unwrap();
    }
    tx.commit().unwrap();

    let n: i64 = pool
        .get()
        .unwrap()
        .query_row(
            if fts {
                "SELECT count(*) FROM t_fts"
            } else {
                "SELECT count(*) FROM t"
            },
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 150);
}

macro_rules! probes {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            #[ignore = "needs the concurrent-process harness in this file's docs"]
            fn $name() {
                one(2);
            }
        )*
    };
}

// The sibling probe fans out over this many test functions so that
// `--test-threads=32` produces many concurrent short-lived databases in one
// process, which is the shape that reproduces. Matched here on purpose.
probes!(
    a1, a2, a3, a4, a5, a6, a7, a8, a9, b1, b2, b3, b4, b5, b6, b7, b8, b9, c1, c2, c3, c4, c5, c6,
    c7, c8, c9, d1, d2, d3,
);
