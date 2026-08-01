//! Connection setup: the sqlite-vec auto-extension registration (must run
//! once, before the pool is built) and per-connection pragmas.

use std::path::Path;
use std::sync::Once;
use std::time::Duration;

use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

static VEC_EXTENSION_INIT: Once = Once::new();

/// Registers sqlite-vec as a SQLite auto-extension so every connection
/// opened afterwards (including every pooled connection) gets `vec0`
/// support. Must be called before the connection pool is built. Safe to
/// call more than once — only the first call takes effect.
pub fn register_vec_extension() {
    VEC_EXTENSION_INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    });
}

pub fn manager_file(path: &Path) -> SqliteConnectionManager {
    SqliteConnectionManager::file(path).with_init(init_pragmas)
}

pub fn manager_memory() -> SqliteConnectionManager {
    SqliteConnectionManager::memory().with_init(init_pragmas)
}

/// Per-connection pragmas, applied outside any transaction via r2d2's
/// `with_init` hook. Order: busy_timeout -> WAL -> synchronous ->
/// foreign_keys -> temp_store -> mmap -> cache.
fn init_pragmas(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "mmap_size", 256i64 * 1024 * 1024)?;
    conn.pragma_update(None, "cache_size", -64 * 1024i64)?; // negative = KiB
    Ok(())
}
