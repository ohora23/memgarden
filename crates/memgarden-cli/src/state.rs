//! Per-session hook state: `<state_dir>/<session_id>.json`.
//!
//! **This file is a cache.** The authoritative copy of a session's cursors is
//! the daemon's `sessions` row (C1); this one exists so the fast path never
//! makes a network call to find out where it is. Losing it costs one recovery
//! round trip (C2b), not a session's memory — which is why nothing here is
//! defensive beyond "a corrupt file reads as absent".
//!
//! One file per session, replacing legacy's three global files
//! (`turns.json`, `retention_tracking.json`, `bank_missions.json`) and their
//! global flocks (`state.py:95-210`). That is the whole reason there is no
//! read-modify-write contention here and no 10,000-entry truncation hack.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bumped when the on-disk shape changes incompatibly. A file whose `schema`
/// is not this value reads as absent, which costs a recovery round trip and
/// nothing else — much better than a parse error on a field that moved.
pub const SCHEMA: u32 = 1;

/// legacy: `state.py:51` caps a sanitized state filename at 200 characters.
/// Applied here in **bytes** — see `path_for`.
const MAX_FILENAME_BYTES: usize = 200;

/// A retain POST that was accepted (202) but whose job has not been confirmed.
///
/// The cursor is committed optimistically on the 202 and reconciled on the
/// next invocation via `GET /v1/retain/{job_id}` (plan §Binding decisions #8).
/// `offset_from`/`chunk_before` are what a `failed` job rolls back to.
///
/// // ponytail: one in-flight job per session. A queue if retain ever fires
/// // per-turn instead of every `retain_every_n_turns`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub job_id: String,
    pub offset_from: u64,
    pub offset_to: u64,
    pub chunk_before: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub schema: u32,
    pub session_id: String,
    pub bank_id: String,

    /// Byte position in the transcript file that has been POSTed.
    ///
    /// **Recovery seeds this from the mirror's `confirmed_offset`, never from
    /// its `byte_offset`** — see [`SessionState::recovered`], which is the
    /// only way to build one of these from the daemon. `byte_offset` is what
    /// some hook *sent*, so it is already ahead of reality after a failed job
    /// or a byte-budget 429; seeding from it skips exactly the bytes the dual
    /// cursor exists to protect (`docs/design/c1-session-state.md`).
    pub offset: u64,
    /// Increments once per accepted delta. `0` -> the bare `session_id` as
    /// `document_id`; `N > 0` -> `session_id-cN` (`retain.py:154`).
    pub chunk: u64,
    /// `Stop` invocations seen this session.
    pub turns: u64,
    /// Reset to 0 on every accepted retain; the turn gate compares it against
    /// `[hooks] retain_every_n_turns`.
    pub turns_since_retain: u64,
    /// `compact_boundary` lines seen. Counted and reported, never acted on
    /// (plan §Binding decisions #6). A lower bound, not an exact count: a
    /// rollback-and-resend re-counts the boundaries in the re-sent delta.
    pub compactions: u64,
    pub pending: Option<Pending>,

    /// Connection refused, timeout, unparseable body. Drives the circuit
    /// breaker. A merely-down daemon lives here and can never poison a
    /// session.
    pub transport_failures: u32,
    /// The daemon answered and the answer was a durable client-side
    /// rejection. Drives poisoning. Kept separate from `transport_failures`
    /// precisely so an outage cannot poison anything.
    pub reject_failures: u32,
    /// While `now < breaker_open_until_ms` the request is skipped entirely.
    pub breaker_open_until_ms: i64,
    /// Set at `max_reject_failures`. A slow-retry state, not a latch: the
    /// hook still retries once per `poison_retry_secs`, and any success
    /// clears it.
    pub poisoned_at: Option<i64>,
}

impl SessionState {
    /// A session seen for the first time.
    pub fn new(session_id: &str, bank_id: &str) -> SessionState {
        SessionState {
            schema: SCHEMA,
            session_id: session_id.to_string(),
            bank_id: bank_id.to_string(),
            offset: 0,
            chunk: 0,
            turns: 0,
            turns_since_retain: 0,
            compactions: 0,
            pending: None,
            transport_failures: 0,
            reject_failures: 0,
            breaker_open_until_ms: 0,
            poisoned_at: None,
        }
    }

    /// Rebuilds state from the daemon's mirror after the local file was lost
    /// (C2b's wiped-state-dir recovery).
    ///
    /// The parameter is named `confirmed_offset` and takes only that column
    /// for a reason: `byte_offset` is the obvious reading and the unsafe one.
    /// Re-sending from the durable cursor is at-least-once and the daemon's
    /// content-hash dedup answers `duplicate`; seeding from the optimistic
    /// cursor is at-most-once, and the bytes it skips are the ones nothing
    /// ingested. There is no constructor that takes `byte_offset`, so the
    /// wrong one cannot be reached for.
    pub fn recovered(
        session_id: &str,
        bank_id: &str,
        confirmed_offset: u64,
        chunk: u64,
    ) -> SessionState {
        SessionState {
            offset: confirmed_offset,
            chunk,
            ..SessionState::new(session_id, bank_id)
        }
    }
}

/// Maps a session id onto a filename inside `dir`.
///
/// Port of `state.py:40-64`: replace path separators, control characters and
/// the Windows-reserved set with `_`, collapse `..`, cap the length, then
/// re-check that the resolved path is still inside `dir`. The sanitizer alone
/// would be enough today; the containment re-check is what survives someone
/// later "improving" the sanitizer.
pub fn path_for(dir: &Path, session_id: &str) -> Option<PathBuf> {
    let mut safe: String = session_id
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while let Some(i) = safe.find("..") {
        safe.replace_range(i..i + 2, "_");
    }
    // Bytes, on a char boundary. legacy slices *characters* (`state.py:51`),
    // which is the same thing for the ASCII uuids Claude Code actually sends
    // and blows up on anything else: 200 Korean characters is 600 bytes and
    // ext4's limit is 255, so the "capped" name is rejected by `open`.
    if safe.len() > MAX_FILENAME_BYTES {
        let end = (0..=MAX_FILENAME_BYTES)
            .rev()
            .find(|&i| safe.is_char_boundary(i))
            .unwrap_or(0);
        safe.truncate(end);
    }
    if safe.is_empty() {
        safe = "state".to_string();
    }
    let path = dir.join(format!("{safe}.json"));
    // Compared lexically after normalization rather than via `canonicalize`,
    // which needs the file to exist and would resolve a symlinked state dir
    // out from under us.
    if path.parent() != Some(dir) {
        return None;
    }
    Some(path)
}

/// Reads a session's state. `None` for absent, unreadable, malformed, or a
/// schema we do not recognise — all four are "start over", and C2b turns that
/// into a recovery from the daemon rather than a full re-ingest.
pub fn load(dir: &Path, session_id: &str) -> Option<SessionState> {
    let path = path_for(dir, session_id)?;
    let bytes = std::fs::read(path).ok()?;
    let state: SessionState = serde_json::from_slice(&bytes).ok()?;
    (state.schema == SCHEMA).then_some(state)
}

/// Writes a session's state atomically: temp file, `fsync`, `rename`.
///
/// The `fsync` is what makes the rename publish complete bytes rather than a
/// filename pointing at an empty inode. The containing directory is
/// deliberately *not* fsynced — this file is a cache (see the module comment),
/// so surviving a power cut is not worth a second sync on the per-turn path.
pub fn store(dir: &Path, state: &SessionState) -> std::io::Result<()> {
    let path = path_for(dir, &state.session_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session id does not map to a path inside the state dir",
        )
    })?;
    std::fs::create_dir_all(dir)?;
    // Named per session and per process: two of our own processes (an
    // `async: true` Stop still running when the next fires) must not share
    // one temp file, and neither must two different sessions.
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{stem}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(state)?;
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Runs `f` while holding an exclusive lock on the session's own lock file.
///
/// **`File::lock()` is advisory. It serializes MemGarden against MemGarden and
/// nothing else** — review demonstrated that a second handle which never calls
/// `lock()` writes straight through it. That is exactly enough for the one
/// race we have: an `async: true` `Stop` still running when the next one
/// fires, both of them our own processes. Do not read it as mutual exclusion
/// against anything we do not control.
///
/// A lock we cannot acquire is not a reason to fail a hook: `f` runs anyway,
/// unlocked, because the alternative is dropping a turn's state to protect
/// against a race with ourselves.
pub fn with_lock<T>(dir: &Path, session_id: &str, f: impl FnOnce() -> T) -> std::io::Result<T> {
    let path = path_for(dir, session_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session id does not map to a path inside the state dir",
        )
    })?;
    std::fs::create_dir_all(dir)?;
    let lock_path = path.with_extension("lock");
    let handle = File::create(&lock_path)?;
    let locked = handle.lock().is_ok();
    let out = f();
    if locked {
        let _ = handle.unlock();
    }
    Ok(out)
}

/// Deletes state files not modified since `cutoff_ms`, returning how many went.
///
/// Per-session files are cheap but unbounded over time; this is the bound.
/// Age comes from the filesystem mtime rather than a field in the JSON so that
/// a file too corrupt to parse is still collectable.
pub fn gc(dir: &Path, cutoff_ms: i64) -> std::io::Result<usize> {
    let mut removed = 0;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let modified_ms = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        if modified_ms.is_some_and(|m| m < cutoff_ms) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(session_id: &str) -> SessionState {
        SessionState {
            offset: 65536,
            chunk: 2,
            turns: 20,
            turns_since_retain: 3,
            compactions: 1,
            pending: Some(Pending {
                job_id: "job-1".to_string(),
                offset_from: 4096,
                offset_to: 65536,
                chunk_before: 1,
            }),
            transport_failures: 1,
            reject_failures: 2,
            breaker_open_until_ms: 1234,
            poisoned_at: Some(5678),
            ..SessionState::new(session_id, "claude-code::demo")
        }
    }

    #[test]
    fn round_trips_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let state = sample("s1");
        store(dir.path(), &state).unwrap();
        assert_eq!(load(dir.path(), "s1").unwrap(), state);
    }

    /// Recovery must take the durable cursor. The signature is the guarantee,
    /// so this test pins the signature's *effect*: what lands in `offset` is
    /// the number labelled confirmed.
    #[test]
    fn recovery_seeds_the_offset_from_the_confirmed_cursor() {
        let mirror_byte_offset = 99999u64;
        let mirror_confirmed_offset = 65536u64;
        let state = SessionState::recovered("s1", "b1", mirror_confirmed_offset, 2);
        assert_eq!(state.offset, mirror_confirmed_offset);
        assert_ne!(state.offset, mirror_byte_offset);
        assert_eq!(state.chunk, 2);
        // A recovered session has no in-flight job to reconcile: the mirror
        // carries no job id, and `confirmed_offset` is by construction behind
        // anything unresolved.
        assert!(state.pending.is_none());
    }

    #[test]
    fn a_replaced_file_never_leaves_a_half_written_one() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), &SessionState::new("s1", "b1")).unwrap();
        store(dir.path(), &sample("s1")).unwrap();
        assert_eq!(load(dir.path(), "s1").unwrap().offset, 65536);
        // The temp file is not left behind, and nothing but the state file
        // (plus any lock file) is in the dir.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn unusable_files_read_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), "never-written").is_none());

        std::fs::write(dir.path().join("corrupt.json"), b"{\"schema\": 1, tr").unwrap();
        assert!(load(dir.path(), "corrupt").is_none());

        // A future schema is not a parse error and must not be read as one.
        let future = serde_json::json!({
            "schema": SCHEMA + 1,
            "session_id": "future",
            "bank_id": "b1",
            "offset": 10,
            "chunk": 0,
            "turns": 0,
            "turns_since_retain": 0,
            "compactions": 0,
            "pending": null,
            "transport_failures": 0,
            "reject_failures": 0,
            "breaker_open_until_ms": 0,
            "poisoned_at": null,
        });
        std::fs::write(dir.path().join("future.json"), future.to_string()).unwrap();
        assert!(load(dir.path(), "future").is_none());
    }

    #[test]
    fn a_traversing_session_id_stays_inside_the_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        for hostile in [
            "../../etc/passwd",
            "..",
            "/etc/passwd",
            "a/../../b",
            "sub/dir/id",
            "nul\u{0}byte",
        ] {
            let path = path_for(dir.path(), hostile).unwrap();
            assert_eq!(
                path.parent(),
                Some(dir.path()),
                "{hostile} escaped to {path:?}"
            );
            // And a real write lands in the dir, not above it.
            let state = SessionState::new(hostile, "b1");
            store(dir.path(), &state).unwrap();
            assert_eq!(load(dir.path(), hostile).unwrap(), state);
        }
        // Nothing was created outside the temp dir.
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .all(|e| e.path().is_file())
        );
    }

    #[test]
    fn an_overlong_session_id_is_capped_and_still_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let long = "한".repeat(400);
        let path = path_for(dir.path(), &long).unwrap();
        let stem = path.file_stem().unwrap().to_string_lossy();
        assert!(stem.len() <= MAX_FILENAME_BYTES, "{} bytes", stem.len());
        // Truncated on a char boundary, so the name is still valid utf-8.
        assert_eq!(stem.chars().count(), MAX_FILENAME_BYTES / 3);
        let state = SessionState::new(&long, "b1");
        store(dir.path(), &state).unwrap();
        assert_eq!(load(dir.path(), &long).unwrap(), state);
    }

    /// Two of *our own* processes, which is the only race the advisory lock
    /// can arbitrate. The negative half — an unlocked writer sails through —
    /// is asserted too, so nobody upgrades this comment into a promise.
    #[test]
    fn concurrent_locked_writers_serialize_but_an_unlocked_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let counter = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));

        std::thread::scope(|scope| {
            for id in 0..4u32 {
                let path = path.clone();
                let counter = counter.clone();
                scope.spawn(move || {
                    with_lock(&path, "s1", || {
                        let mut state =
                            load(&path, "s1").unwrap_or_else(|| SessionState::new("s1", "b1"));
                        state.turns += 1;
                        // A gap wide enough that an unserialized
                        // read-modify-write would lose an increment.
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        store(&path, &state).unwrap();
                        counter.lock().unwrap().push(id);
                    })
                    .unwrap();
                });
            }
        });
        assert_eq!(load(&path, "s1").unwrap().turns, 4);
        assert_eq!(counter.lock().unwrap().len(), 4);

        // The advisory half, demonstrated rather than asserted in prose: a
        // writer that never calls `lock()` is not excluded.
        let held = File::create(path.join("s1.lock")).unwrap();
        held.lock().unwrap();
        store(&path, &SessionState::new("s1", "b1")).unwrap();
        assert_eq!(load(&path, "s1").unwrap().turns, 0);
        held.unlock().unwrap();
    }

    #[test]
    fn gc_drops_only_files_older_than_the_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), &SessionState::new("fresh", "b1")).unwrap();
        store(dir.path(), &SessionState::new("stale", "b1")).unwrap();
        let stale = dir.path().join("stale.json");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();

        // A cutoff between the two, expressed in the same unit the caller
        // passes: milliseconds since the epoch.
        let cutoff_ms = 2_000_000_000i64;
        assert_eq!(gc(dir.path(), cutoff_ms).unwrap(), 1);
        assert!(load(dir.path(), "fresh").is_some());
        assert!(load(dir.path(), "stale").is_none());

        // A missing dir is not an error — a fresh machine has no state yet.
        assert_eq!(gc(&dir.path().join("nope"), cutoff_ms).unwrap(), 0);
    }
}
