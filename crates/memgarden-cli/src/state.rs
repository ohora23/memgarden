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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Ceiling on one state file, applied by [`load`] and [`load_all`] as a
/// bounded read.
///
/// A real file is ~400 bytes. This is not about a hostile writer so much as
/// about the *shape* of the failure: `gc` prunes by mtime, so a single
/// oversized `*.json` — a botched write, a log accidentally renamed in — would
/// otherwise be re-read in full on **every** session start for
/// `session_retention_days`. Truncating makes it unparseable, which reads as
/// absent, which is already the handling for every other unusable file.
const MAX_STATE_FILE_BYTES: u64 = 64 * 1024;

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

    /// Where Claude Code is appending this session's transcript, copied from
    /// the `SessionStart` payload.
    ///
    /// The live hooks all receive it on stdin and would not need it stored.
    /// The **detached catch-up child** (C2b) is the caller that does: it wakes
    /// up with no hook payload at all and has to decide, per session, whether
    /// `offset < file_size`. Plan §Binding decisions #5 lists the state shape
    /// without this field, which is a gap rather than a decision — there is no
    /// other way for catch-up to find the file.
    ///
    /// `#[serde(default)]` so a state file written before this field existed
    /// still loads (as absent, which makes catch-up skip it) instead of
    /// failing to parse and costing a recovery round trip.
    #[serde(default)]
    pub transcript_path: String,

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
            transcript_path: String::new(),
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
    /// wrong one cannot be *reached for* by accident.
    ///
    /// It is not airtight — every field here is `pub`, so C2b could still
    /// assign `offset` directly. The enforcement that would be airtight is
    /// **omitting `byte_offset` from C2b's mirror-response struct entirely**:
    /// a field that is never deserialized cannot be misused. C2b should do
    /// that; this constructor makes the right thing the easy thing meanwhile.
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
    // Bounded for the same reason `load_all` is, and more urgently: C3 calls
    // this on **every prompt**, where `load_all` runs twice a session. `gc`
    // prunes by mtime only, so without the cap one oversized `<sid>.json` — a
    // botched write, a log renamed in — is re-read in full on every turn for
    // the whole retention window. Truncating makes it unparseable, which reads
    // as absent, which is already the handling for every unusable file.
    let mut bytes = Vec::new();
    File::open(&path)
        .ok()?
        .take(MAX_STATE_FILE_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let state: SessionState = serde_json::from_slice(&bytes).ok()?;
    // The `session_id` conjunct is not belt-and-braces: sanitization is
    // **many-to-one**, so `a/b` and `a_b` share one file. Without this check
    // the second session silently inherits the first's cursor and re-sends or
    // skips its transcript. With it, a collision reads as absent and C2b
    // recovers from the daemon — the same path a wiped state dir takes.
    (state.schema == SCHEMA && state.session_id == session_id).then_some(state)
}

/// Every state file in `dir` that [`load`] would accept, unordered.
///
/// The catch-up child (C2b) is the one caller that does not know which
/// sessions exist: it is spawned with only the *current* session id and has to
/// discover the rest. It therefore reads each file and takes the `session_id`
/// from **inside** it rather than from the filename — sanitization is
/// many-to-one, so a filename stem is not a session id and
/// `load(dir, stem)` would silently miss any session whose id needed
/// sanitizing.
///
/// The two conjuncts are `load`'s, for the same two reasons: an unrecognised
/// `schema` is "start over", and a file whose stored id does not map back to
/// its own path is a collision between two sessions rather than either of
/// them.
pub fn load_all(dir: &Path) -> Vec<SessionState> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "json" {
                return None;
            }
            // `file_type()` comes from the dirent / an `lstat`, so it does
            // **not** follow symlinks: a planted `s1.json -> /elsewhere` is
            // skipped rather than read. `path_for`'s containment check below
            // is lexical and cannot see through a symlink, so this is the half
            // that covers it.
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            let mut bytes = Vec::new();
            File::open(&path)
                .ok()?
                .take(MAX_STATE_FILE_BYTES)
                .read_to_end(&mut bytes)
                .ok()?;
            let state: SessionState = serde_json::from_slice(&bytes).ok()?;
            (state.schema == SCHEMA && path_for(dir, &state.session_id) == Some(path))
                .then_some(state)
        })
        .collect()
}

/// Writes a session's state atomically: temp file then `rename`.
///
/// **No `fsync`.** `rename(2)` publishes the full contents to every reader on
/// a live system with or without one — the page cache is coherent, so no
/// reader can observe the empty inode. An fsync would buy only power-cut
/// survival, which this file explicitly does not need (it is a cache; the
/// daemon's `sessions` row is authoritative and C2b recovers from it). Paying
/// a real disk sync on the per-turn hot path for a property the next sentence
/// declines is the wrong trade, and arm B cannot see the cost because `noop`
/// never touches state. **This cache is not crash-durable, deliberately.**
pub fn store(dir: &Path, state: &SessionState) -> std::io::Result<()> {
    let path = path_for(dir, &state.session_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session id does not map to a path inside the state dir",
        )
    })?;
    ensure_dir(dir)?;
    let bytes = serde_json::to_vec(state)?;
    let (mut file, tmp) = create_temp(dir, &path)?;
    let written = file.write_all(&bytes);
    drop(file);
    match written.and_then(|()| std::fs::rename(&tmp, &path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// `create_new` so the open fails rather than following a symlink someone
/// planted at the temp path; the counter is the retry, because the only way
/// `create_new` fails benignly is a leftover from a crashed run with the same
/// pid.
fn create_temp(dir: &Path, path: &Path) -> std::io::Result<(File, PathBuf)> {
    // Named per session and per process: two of our own processes (an
    // `async: true` Stop still running when the next fires) must not share one
    // temp file, and neither must two different sessions.
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let pid = std::process::id();
    let mut last = None;
    for attempt in 0..4 {
        let tmp = dir.join(format!(".{stem}.{pid}.{attempt}.tmp"));
        // 0600, not umask. The state dir is already 0700 so this is
        // belt-and-braces — but the daemon chmods `memgarden.db` explicitly
        // for the same reason, and these files carry transcript paths and
        // cursors. Observed 0664 before this line.
        match open_private(&tmp) {
            Ok(f) => return Ok((f, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&tmp);
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free temp path")
    }))
}

/// `create_new` at mode 0600 — the `File::create_new` this replaces, with the
/// permission bits made explicit instead of left to the process umask.
fn open_private(path: &Path) -> std::io::Result<File> {
    let mut options = File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// `create_dir_all` with mode 0700, via the helper the daemon already uses.
///
/// Not cosmetic: `state_dir` defaults to `<data>/hooks`, so a hook running
/// before `memgardend` has ever started is what **creates the data directory**
/// that will later hold `memgarden.db`. A bare `create_dir_all` left that at
/// 0755 — and `<data>` is where `memgarden.db` lands. The parent gets the same
/// treatment for exactly that reason: `ensure_data_dir` only chmods its
/// argument, so calling it on `<data>/hooks` alone would still leave `<data>`
/// itself world-readable when the hook is what created it.
///
/// Public because C3 writes two non-state files into the same directory
/// (`shadow-recall.jsonl`, `last_recall.json`) and must not be the caller that
/// creates it at 0755.
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        memgarden_core::paths::ensure_data_dir(parent).map_err(std::io::Error::other)?;
    }
    memgarden_core::paths::ensure_data_dir(dir).map_err(std::io::Error::other)
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
    ensure_dir(dir)?;
    // A separate inode on purpose, and it must **not** be the state file:
    // `store` renames a fresh inode over that path, so a second process
    // opening it afterwards would lock a different inode and the exclusion
    // would silently stop working. `gc` collects these along with the json.
    let lock_path = path.with_extension("lock");
    // **Never `File::create`.** That is `O_CREAT|O_WRONLY|O_TRUNC`, which
    // follows a symlink and *truncates the target*: review planted
    // `state/sX.lock -> /outside/precious.conf`, ran one `session-start`, and
    // the file came back 0 bytes. `create_temp` twelve lines above already
    // refuses exactly this class, with a comment saying why.
    //
    // The steady state is that the lock file already exists, so `create_new`
    // failing is normal, not an error — and the fallback is a **read-only**
    // open, which cannot truncate anything a symlink points at. `flock(2)`
    // does not require write access, so the lock still works. No `O_NOFOLLOW`
    // and therefore no `libc`: this crate's dependency closure is CI-enforced
    // and a `read(true)` open is already the property that matters.
    let handle = open_private(&lock_path).or_else(|_| File::open(&lock_path))?;
    let locked = handle.lock().is_ok();
    let out = f();
    if locked {
        let _ = handle.unlock();
    }
    Ok(out)
}

/// Deletes state files not modified since `cutoff_ms`, returning how many went.
///
/// Per-session files are cheap but unbounded over time; this is the bound —
/// and it has to cover **everything a session leaves behind**, not just the
/// json. `with_lock` writes a `.lock` sibling and a crashed `store` can leave
/// a `.tmp`; collecting only `.json` bounded a third of what lands here and
/// let the other two accumulate forever.
pub fn gc(dir: &Path, cutoff_ms: i64) -> std::io::Result<usize> {
    let mut removed = 0;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|e| e == "json" || e == "lock" || e == "tmp")
        {
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
            transcript_path: "/tmp/transcript.jsonl".to_string(),
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

    /// A C2a-era file has no `transcript_path`. It must still load — as a
    /// session catch-up skips, not as a parse failure that costs a recovery
    /// round trip.
    #[test]
    fn a_state_file_written_before_transcript_path_existed_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let old = serde_json::json!({
            "schema": SCHEMA,
            "session_id": "old",
            "bank_id": "b1",
            "offset": 4096,
            "chunk": 1,
            "turns": 3,
            "turns_since_retain": 3,
            "compactions": 0,
            "pending": null,
            "transport_failures": 0,
            "reject_failures": 0,
            "breaker_open_until_ms": 0,
            "poisoned_at": null,
        });
        std::fs::write(dir.path().join("old.json"), old.to_string()).unwrap();
        let state = load(dir.path(), "old").expect("an older shape is not a parse failure");
        assert_eq!(state.offset, 4096);
        assert_eq!(state.transcript_path, "");
    }

    /// `load_all` is the catch-up child's only way to find the sessions it did
    /// not receive on stdin, so it has to accept exactly what `load` accepts —
    /// **including** ids that needed sanitizing, which the filename cannot
    /// round-trip.
    #[test]
    fn load_all_finds_every_session_load_would_accept_and_no_others() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), &sample("s1")).unwrap();
        store(dir.path(), &SessionState::new("a/b", "bank")).unwrap();

        // Rejected: an unknown schema, a corrupt file, a collision, and the
        // non-json leftovers `gc` also collects.
        std::fs::write(dir.path().join("corrupt.json"), b"{\"schema\": 1, tr").unwrap();
        let mut future = sample("future");
        future.schema = SCHEMA + 1;
        std::fs::write(
            dir.path().join("future.json"),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();
        // `a_b.json` would be read as the state of `a/b`, whose stored id does
        // not map back to that path... except it does, so plant the reverse:
        // a file whose *name* claims a different session than its contents.
        std::fs::write(
            dir.path().join("impostor.json"),
            serde_json::to_vec(&SessionState::new("somebody-else", "bank")).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.path().join(".s1.999.0.tmp"), b"partial").unwrap();

        let mut ids: Vec<String> = load_all(dir.path())
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["a/b".to_string(), "s1".to_string()]);

        // A dir that does not exist yet is empty, not an error.
        assert!(load_all(&dir.path().join("nope")).is_empty());
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

    /// A session leaves up to three files behind. The bound has to cover all
    /// of them — review measured `gc(dir, i64::MAX)` removing 3 `.json` and
    /// leaving 3 immortal `.lock`s.
    #[test]
    fn gc_collects_lock_and_temp_leftovers_too() {
        let dir = tempfile::tempdir().unwrap();
        for sid in ["s1", "s2", "s3"] {
            with_lock(dir.path(), sid, || {
                store(dir.path(), &SessionState::new(sid, "b1")).unwrap();
            })
            .unwrap();
        }
        // A crashed `store`'s leftover.
        std::fs::write(dir.path().join(".s1.999.0.tmp"), b"partial").unwrap();
        assert_eq!(gc(dir.path(), i64::MAX).unwrap(), 7);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Sanitization is many-to-one, so two session ids can land on one file.
    /// Returning the other session's cursor would re-send or skip a
    /// transcript; reading as absent routes into C2b's recovery instead.
    #[test]
    fn a_filename_collision_reads_as_absent_rather_than_as_another_session() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            path_for(dir.path(), "a/b").unwrap(),
            path_for(dir.path(), "a_b").unwrap(),
            "the premise: these collide"
        );
        let mut first = SessionState::new("a/b", "b1");
        first.offset = 4096;
        store(dir.path(), &first).unwrap();

        assert_eq!(load(dir.path(), "a/b").unwrap().offset, 4096);
        assert!(
            load(dir.path(), "a_b").is_none(),
            "a colliding id must not inherit another session's cursor"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_state_dir_and_its_parent_are_created_0700() {
        use std::os::unix::fs::PermissionsExt;
        // The real shape: `<data>/hooks`, where a hook can be what creates
        // `<data>` — the directory `memgarden.db` will later live in.
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("memgarden");
        let hooks = data.join("hooks");
        store(&hooks, &SessionState::new("s1", "b1")).unwrap();
        for dir in [&data, &hooks] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{dir:?}");
        }
    }

    /// The same rule as the temp path, on the **lock** path — which C2a wrote
    /// with `File::create` and which C2b is the first PR to reach on a real
    /// hook path. Measured before the fix: the target came back 0 bytes.
    #[test]
    fn a_planted_symlink_at_the_lock_path_is_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("precious.conf");
        std::fs::write(&victim, b"do not truncate").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join("s1.lock")).unwrap();

        // The hook still does its work — a lock we cannot take cleanly is
        // never a reason to drop a turn's state.
        with_lock(dir.path(), "s1", || {
            store(dir.path(), &SessionState::new("s1", "b1")).unwrap();
        })
        .unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"do not truncate");
        assert!(load(dir.path(), "s1").is_some());
    }

    /// `path_for`'s containment check is lexical, so it cannot see through a
    /// symlink. `file_type()` is the half that can: it comes from an `lstat`.
    #[test]
    fn load_all_skips_a_symlinked_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let planted = outside.path().join("elsewhere.json");
        std::fs::write(
            &planted,
            serde_json::to_vec(&SessionState::new("outsider", "b1")).unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&planted, dir.path().join("outsider.json")).unwrap();
        store(dir.path(), &SessionState::new("real", "b1")).unwrap();

        let ids: Vec<String> = load_all(dir.path())
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, vec!["real".to_string()]);
    }

    /// The same ceiling on the single-session path, which C3 calls on **every
    /// prompt** — ten times the exposure `load_all` has.
    #[test]
    fn load_bounds_an_oversized_state_file_too() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), &sample("s1")).unwrap();
        assert!(load(dir.path(), "s1").is_some(), "the premise");

        let mut bloated = serde_json::to_vec(&sample("s1")).unwrap();
        bloated.pop();
        bloated.extend(format!(",\"pad\":\"{}\"}}", "x".repeat(200 * 1024)).bytes());
        assert!(bloated.len() as u64 > MAX_STATE_FILE_BYTES);
        std::fs::write(dir.path().join("s1.json"), &bloated).unwrap();

        // Truncated -> unparseable -> absent, which is already the handling
        // for every other unusable file.
        assert!(load(dir.path(), "s1").is_none());
    }

    /// `gc` prunes by mtime, so without a ceiling one oversized file is
    /// re-read in full on every session start for the whole retention window.
    #[test]
    fn an_oversized_state_file_is_bounded_rather_than_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let mut bloated = serde_json::to_vec(&SessionState::new("bloated", "b1")).unwrap();
        // Valid JSON that a naive read would parse; truncation makes it not.
        bloated.pop();
        bloated.extend(format!(",\"pad\":\"{}\"}}", "x".repeat(200 * 1024)).bytes());
        assert!(bloated.len() as u64 > MAX_STATE_FILE_BYTES);
        std::fs::write(dir.path().join("bloated.json"), &bloated).unwrap();
        store(dir.path(), &SessionState::new("normal", "b1")).unwrap();

        let ids: Vec<String> = load_all(dir.path())
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, vec!["normal".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn state_files_are_created_0600_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        with_lock(dir.path(), "s1", || {
            store(dir.path(), &SessionState::new("s1", "b1")).unwrap();
        })
        .unwrap();
        for name in ["s1.json", "s1.lock"] {
            let mode = std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{name}");
        }
    }

    /// `create_new` rather than `create`: a pre-existing symlink at the temp
    /// path must not be followed into somewhere we were never meant to write.
    #[test]
    fn a_planted_symlink_at_the_temp_path_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"do not clobber").unwrap();
        let planted = dir.path().join(format!(".s1.{}.0.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();

        store(dir.path(), &SessionState::new("s1", "b1")).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not clobber");
        assert!(load(dir.path(), "s1").is_some());
    }
}
