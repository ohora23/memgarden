//! `sessions` persistence (migration `0007`, HK-1a). The daemon-side mirror
//! of the Phase C hook's per-session state file.
//!
//! **Two cursors, and they are not redundant.** `POST …/retain` answers `202`
//! when a job is *queued*, not when it is ingested (`routes/retain.rs`); the
//! worker can still fail a chunk and mark the job `Failed`, and it withholds
//! the document content hash on any non-clean run precisely so that
//! re-POSTing the same transcript starts a fresh job instead of being
//! dismissed as a duplicate (`retain/mod.rs`, "Review HIGH 1"). So:
//!
//! * [`Session::byte_offset`] is **optimistic** — what the hook has POSTed.
//! * [`Session::confirmed_offset`] is **durable** — bytes for which ingestion
//!   is a settled fact.
//!
//! Both are written `MAX(existing, incoming)` so an out-of-order `async: true`
//! `Stop` cannot rewind the mirror. A single monotonic column would keep that
//! invariant but make the hook's rollback (plan §Binding decisions #8)
//! inexpressible, and would hide the interesting quantity:
//! `byte_offset - confirmed_offset` is exactly the in-flight-or-lost window.
//!
//! **Who writes what.** Every field has exactly one writer class:
//!
//! | field | writer |
//! |---|---|
//! | `cwd`, `transcript_path`, `source`, `end_reason`, `ended_at`, `turns`, `chunk_index`, `byte_offset`, `compactions` | the hook, via `POST …/sessions` or the retain request |
//! | `retains`, `messages_sent` | the daemon, `+=` once per accepted retain |
//! | `confirmed_offset` | the daemon, only from a settlement it observed |
//!
//! No client field is named `confirmed_offset` on any endpoint. It advances
//! down two channels, and they are deliberately different
//! ([`SessionUpdate::confirmed_offset`] vs
//! [`SessionUpdate::confirm_if_settled`]):
//!
//! * the retain **worker**, unconditionally, on a clean run — it observed the
//!   whole range it is confirming;
//! * the retain **request path**, for the `skipped` and `duplicate` outcomes,
//!   **only when there is no open gap already**. Those outcomes prove nothing
//!   is outstanding for *this request's* bytes; they prove nothing about an
//!   earlier queued job that has not come back. Without the guard, an
//!   ordinary role-filtered `skipped` at a higher offset would swallow an
//!   earlier failure's gap and the instrument would read zero over lost
//!   bytes (review HIGH 1).
//!
//! ## Reconciliation with `retain_jobs`
//!
//! `retain_jobs` is one row per retain **request**; `sessions` is one row per
//! **(bank, session)**. Neither carries a column of the other, and no field
//! is written by both paths. The counts relate but are **not equal**:
//!
//! ```text
//! sessions.retains >= count(retain_jobs WHERE session_id = …)
//!    delta = the accepts that queue no job: `skipped` + `duplicate`
//! ```
//!
//! `messages_sent` is likewise a running sum of a value `retain_jobs.detail.
//! message_count` records per request — the same total only while no delta is
//! ever re-sent. It is additive, therefore **not** idempotent under the
//! rollback-and-resend of §Binding decisions #8; the cursors are the fields
//! that survive that, which is why they are the ones anything reasons over.

use rusqlite::{OptionalExtension, params};

use memgarden_core::error::{Error, Result};
use memgarden_core::now_ms;

use crate::{Db, store_err};

const SELECT_COLUMNS: &str = "bank_id, session_id, cwd, transcript_path, source, end_reason,
     turns, retains, chunk_index, byte_offset, confirmed_offset, messages_sent,
     compactions, started_at, last_seen_at, ended_at";

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub bank_id: String,
    pub session_id: String,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    /// SessionStart source: `startup|resume|clear|compact|fork`. Set once.
    pub source: Option<String>,
    /// SessionEnd reason. Last write wins.
    pub end_reason: Option<String>,
    pub turns: i64,
    pub retains: i64,
    pub chunk_index: i64,
    /// Optimistic: what the hook has POSTed.
    pub byte_offset: i64,
    /// Durable: bytes whose ingestion is settled. Never ahead of
    /// `byte_offset` in practice, because the hook posts the bytes before
    /// anything can confirm them.
    pub confirmed_offset: i64,
    pub messages_sent: i64,
    pub compactions: i64,
    pub started_at: i64,
    pub last_seen_at: i64,
    pub ended_at: Option<i64>,
}

impl Session {
    /// Bytes the hook has handed over that are not yet known-ingested — in
    /// flight, or lost to a failed job. The number the runbook tells an
    /// operator to watch, and the reason there are two cursors at all.
    pub fn inflight_bytes(&self) -> i64 {
        // An inverted pair would read as "all settled", which is the one
        // wrong answer this number must never give. Nothing can produce it —
        // the durable cursor only ever advances to an offset the optimistic
        // one already reached — so make a future writer that breaks the
        // invariant loud in tests and harmless in production.
        debug_assert!(
            self.confirmed_offset <= self.byte_offset,
            "cursor inversion: confirmed {} > byte {}",
            self.confirmed_offset,
            self.byte_offset
        );
        (self.byte_offset - self.confirmed_offset).max(0)
    }
}

/// One upsert's worth of change. Every field is optional so a caller states
/// only what it observed; `Default` plus struct-update syntax is the intended
/// call shape.
///
/// The three groups behave differently on purpose:
///
/// * `cwd`/`transcript_path`/`end_reason`/`ended_at` — set when supplied,
///   left alone when not (last write wins).
/// * `source` — **first write wins**. A session's origin is decided by its
///   `SessionStart`; a later `resume` upsert must not rewrite history.
/// * `turns`/`chunk_index`/`byte_offset`/`confirmed_offset`/`compactions` —
///   monotonic `MAX`. These are cumulative *absolutes* read from the hook's
///   own state file, not increments, so a retried or out-of-order POST is
///   idempotent rather than double-counted.
/// * `retains_delta`/`messages_sent_delta` — additive. Daemon-owned, one
///   write per accepted retain, so each write is a distinct event and `+=`
///   is the only thing that can be right.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionUpdate<'a> {
    pub session_id: &'a str,
    pub cwd: Option<&'a str>,
    pub transcript_path: Option<&'a str>,
    pub source: Option<&'a str>,
    pub end_reason: Option<&'a str>,
    pub ended_at: Option<i64>,
    pub turns: Option<i64>,
    pub chunk_index: Option<i64>,
    pub byte_offset: Option<i64>,
    /// **Unconditional** advance of the durable cursor. Only the retain
    /// worker may use this, and only on a clean run: it has observed the
    /// whole range it is confirming, so it may close a gap it did not open.
    pub confirmed_offset: Option<i64>,
    /// **Conditional** advance of the durable cursor: applied only when the
    /// row has no open gap (`confirmed_offset >= byte_offset`) at the moment
    /// of the write.
    ///
    /// This is the request path's channel, for the `skipped` and `duplicate`
    /// outcomes. Those outcomes are proof about *this request's* bytes and
    /// say nothing about an earlier queued job that has not come back — and
    /// because the column merges with `MAX`, an unconditional confirm at a
    /// higher offset would erase that earlier gap outright (review HIGH 1).
    ///
    /// When the guard blocks it, the gap simply stays open until the next
    /// clean job confirms past it. Over-reporting outstanding work is the
    /// safe direction for an instrument whose whole job is to notice loss.
    pub confirm_if_settled: Option<i64>,
    pub compactions: Option<i64>,
    pub retains_delta: i64,
    pub messages_sent_delta: i64,
}

/// Session ids come off the wire. Long enough for a UUID and then some;
/// short enough that the PK stays small on a `WITHOUT ROWID` table.
pub const MAX_SESSION_ID_BYTES: usize = 200;

/// Insert-or-merge, one `BEGIN IMMEDIATE` for the write and the read-back.
///
/// Returns the row as it stands after the merge, so a caller never has to
/// guess which of its values won.
pub fn upsert(db: &Db, bank_id: &str, u: &SessionUpdate) -> Result<Session> {
    if u.session_id.is_empty() || u.session_id.len() > MAX_SESSION_ID_BYTES {
        return Err(Error::Invalid(format!(
            "invalid session_id: {} bytes (1..={MAX_SESSION_ID_BYTES})",
            u.session_id.len()
        )));
    }
    let now = now_ms();
    // Clamped at the store boundary rather than trusted: the monotonic
    // columns are `MAX(existing, incoming)`, and a negative incoming value
    // is a no-op only because of this clamp. `i64` overflow is not a
    // concern for a byte offset, and `MAX` cannot grow one anyway.
    let clamp = |v: Option<i64>| v.unwrap_or(0).max(0);
    let turns = clamp(u.turns);
    let chunk_index = clamp(u.chunk_index);
    let byte_offset = clamp(u.byte_offset);
    let confirmed_offset = clamp(u.confirmed_offset);
    // `0`, never NULL: SQLite's scalar `max()` returns NULL if *any* argument
    // is NULL, which would blank the column instead of leaving it alone.
    let confirm_if_settled = clamp(u.confirm_if_settled);
    let compactions = clamp(u.compactions);
    let retains_delta = u.retains_delta.max(0);
    let messages_sent_delta = u.messages_sent_delta.max(0);

    db.write(|tx| {
        tx.execute(
            "INSERT INTO sessions
               (bank_id, session_id, cwd, transcript_path, source, end_reason,
                turns, retains, chunk_index, byte_offset, confirmed_offset,
                messages_sent, compactions, started_at, last_seen_at, ended_at)
             -- A brand-new row has no earlier job outstanding by
             -- construction, so the conditional channel always applies here;
             -- the guard below is what makes it conditional on an existing
             -- row. Mutating only the DO UPDATE branch is not a collapse of
             -- the two cursors, because a session's first write is always
             -- this insert.
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, max(?11, ?16), ?12, ?13, ?14, ?14, ?15)
             ON CONFLICT (bank_id, session_id) DO UPDATE SET
               cwd              = coalesce(excluded.cwd, cwd),
               transcript_path  = coalesce(excluded.transcript_path, transcript_path),
               -- first write wins
               source           = coalesce(source, excluded.source),
               -- last write wins, but only when the caller supplied one
               end_reason       = coalesce(excluded.end_reason, end_reason),
               ended_at         = coalesce(excluded.ended_at, ended_at),
               -- monotonic: a stale async Stop cannot rewind the mirror
               turns            = max(turns, excluded.turns),
               chunk_index      = max(chunk_index, excluded.chunk_index),
               byte_offset      = max(byte_offset, excluded.byte_offset),
               -- Two channels into one column. ?11 is the worker's
               -- unconditional confirm; ?16 is the request path's, gated on
               -- the row having no open gap *before* this write. Bound
               -- positionally rather than through `excluded` on purpose:
               -- `excluded.confirmed_offset` is `max(?11, ?16)` from the
               -- VALUES list above, which would smuggle the conditional
               -- value past its own guard.
               confirmed_offset = max(
                                    confirmed_offset,
                                    ?11,
                                    CASE WHEN confirmed_offset >= byte_offset
                                         THEN ?16 ELSE 0 END),
               compactions      = max(compactions, excluded.compactions),
               -- additive: one write per accepted retain
               retains          = retains + excluded.retains,
               messages_sent    = messages_sent + excluded.messages_sent,
               last_seen_at     = excluded.last_seen_at",
            params![
                bank_id,
                u.session_id,
                u.cwd,
                u.transcript_path,
                u.source,
                u.end_reason,
                turns,
                retains_delta,
                chunk_index,
                byte_offset,
                confirmed_offset,
                messages_sent_delta,
                compactions,
                now,
                u.ended_at,
                confirm_if_settled,
            ],
        )
        .map_err(|e| map_session_err(e, bank_id))?;

        tx.query_row(
            &format!(
                "SELECT {SELECT_COLUMNS} FROM sessions WHERE bank_id = ?1 AND session_id = ?2"
            ),
            params![bank_id, u.session_id],
            from_row,
        )
        .map_err(store_err)
    })
}

pub fn get(db: &Db, bank_id: &str, session_id: &str) -> Result<Option<Session>> {
    let conn = db.read()?;
    conn.query_row(
        &format!("SELECT {SELECT_COLUMNS} FROM sessions WHERE bank_id = ?1 AND session_id = ?2"),
        params![bank_id, session_id],
        from_row,
    )
    .optional()
    .map_err(store_err)
}

/// Most recently seen first — the index order. `active_only` drops sessions
/// that have reported a `SessionEnd`.
pub fn list(db: &Db, bank_id: &str, limit: usize, active_only: bool) -> Result<Vec<Session>> {
    let conn = db.read()?;
    let filter = if active_only {
        "AND ended_at IS NULL"
    } else {
        ""
    };
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {SELECT_COLUMNS} FROM sessions
             WHERE bank_id = ?1 {filter}
             ORDER BY last_seen_at DESC, session_id
             LIMIT ?2"
        ))
        .map_err(store_err)?;
    let rows = stmt
        .query_map(
            params![bank_id, i64::try_from(limit).unwrap_or(i64::MAX)],
            from_row,
        )
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// Drops sessions last seen before `cutoff_ms` — an **absolute unix-ms
/// instant**, not a duration. Returns the number of rows removed.
///
/// Unbounded session accumulation is what pushed legacy into its
/// 10,000-entry truncation hack (`state.py:111-114`); a time-bounded delete
/// over `idx_sessions_last_seen` is the cheaper answer.
pub fn gc(db: &Db, cutoff_ms: i64) -> Result<usize> {
    db.write(|tx| {
        tx.execute(
            "DELETE FROM sessions WHERE last_seen_at < ?1",
            params![cutoff_ms],
        )
        .map_err(store_err)
    })
}

fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        bank_id: r.get(0)?,
        session_id: r.get(1)?,
        cwd: r.get(2)?,
        transcript_path: r.get(3)?,
        source: r.get(4)?,
        end_reason: r.get(5)?,
        turns: r.get(6)?,
        retains: r.get(7)?,
        chunk_index: r.get(8)?,
        byte_offset: r.get(9)?,
        confirmed_offset: r.get(10)?,
        messages_sent: r.get(11)?,
        compactions: r.get(12)?,
        started_at: r.get(13)?,
        last_seen_at: r.get(14)?,
        ended_at: r.get(15)?,
    })
}

/// An upsert against a bank that does not exist is a 404, not a 500 — same
/// mapping discipline as `banks::map_bank_err`. The FK is the only check the
/// table can fail, so this saves the caller a `banks::get` round trip.
fn map_session_err(e: rusqlite::Error, bank_id: &str) -> Error {
    if let rusqlite::Error::SqliteFailure(ref ffi_err, _) = e
        && ffi_err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
    {
        return Error::NotFound(format!("bank {bank_id}"));
    }
    store_err(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::banks;

    fn db_with_bank() -> Db {
        let db = Db::open_memory().unwrap();
        banks::create(&db, "b1", None, None).unwrap();
        db
    }

    #[test]
    fn insert_then_update_roundtrip() {
        let db = db_with_bank();
        let created = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                cwd: Some("/repo"),
                transcript_path: Some("/t.jsonl"),
                source: Some("startup"),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(created.turns, 0);
        assert_eq!(created.cwd.as_deref(), Some("/repo"));
        assert_eq!(created.started_at, created.last_seen_at);

        let updated = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                turns: Some(7),
                byte_offset: Some(4096),
                retains_delta: 1,
                messages_sent_delta: 12,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(updated.turns, 7);
        assert_eq!(updated.byte_offset, 4096);
        assert_eq!(updated.retains, 1);
        assert_eq!(updated.messages_sent, 12);
        // Untouched fields survive a partial update.
        assert_eq!(updated.cwd.as_deref(), Some("/repo"));
        assert_eq!(updated.transcript_path.as_deref(), Some("/t.jsonl"));
        assert_eq!(updated.started_at, created.started_at);
        assert_eq!(get(&db, "b1", "s1").unwrap().unwrap(), updated);

        // `last_seen_at` must be *refreshed* by an upsert, not left at its
        // insert value. Nothing else pins this and everything depends on it:
        // GC expires by `last_seen_at`, so a frozen clock would collect a
        // busy session 90 days after it started, and the dashboard's
        // `ORDER BY last_seen_at DESC` would degrade to creation order.
        // Forced to an unmistakable value first, because two upserts a
        // microsecond apart share a millisecond.
        db.write(|tx| {
            tx.execute("UPDATE sessions SET last_seen_at = 0", [])
                .map_err(store_err)?;
            Ok(())
        })
        .unwrap();
        let before = now_ms();
        let touched = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            touched.last_seen_at >= before,
            "an upsert must refresh last_seen_at, got {}",
            touched.last_seen_at
        );
        assert_eq!(
            touched.started_at, created.started_at,
            "...without disturbing started_at"
        );
    }

    /// `retains` and `messages_sent` are the two additive columns, and both
    /// need the two-write assertion: with `+` mutated to `max(...)` a single
    /// write still looks right.
    #[test]
    fn the_daemon_side_counters_accumulate_rather_than_replace() {
        let db = db_with_bank();
        for _ in 0..2 {
            upsert(
                &db,
                "b1",
                &SessionUpdate {
                    session_id: "s1",
                    retains_delta: 1,
                    messages_sent_delta: 5,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let row = get(&db, "b1", "s1").unwrap().unwrap();
        assert_eq!(row.retains, 2);
        assert_eq!(row.messages_sent, 10);
    }

    /// The `async: true` `Stop` race: two retains overlap and the older one
    /// lands last. Every monotonic field must hold its high-water mark.
    #[test]
    fn a_stale_update_never_rewinds_the_monotonic_fields() {
        let db = db_with_bank();
        upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                turns: Some(20),
                chunk_index: Some(3),
                byte_offset: Some(9000),
                confirmed_offset: Some(8000),
                compactions: Some(2),
                ..Default::default()
            },
        )
        .unwrap();

        let stale = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                turns: Some(10),
                chunk_index: Some(1),
                byte_offset: Some(1000),
                confirmed_offset: Some(500),
                compactions: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(stale.turns, 20);
        assert_eq!(stale.chunk_index, 3);
        assert_eq!(stale.byte_offset, 9000);
        assert_eq!(stale.confirmed_offset, 8000);
        assert_eq!(stale.compactions, 2);
    }

    /// The point of splitting the cursors: the gap is a real quantity, it
    /// opens when work is queued and closes only when work is confirmed.
    /// A single-cursor implementation passes a monotonicity test and fails
    /// this one.
    #[test]
    fn the_gap_between_the_cursors_is_the_in_flight_window() {
        let db = db_with_bank();
        // Hook POSTs bytes 0..5000; nothing is ingested yet.
        let posted = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                byte_offset: Some(5000),
                retains_delta: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(posted.confirmed_offset, 0);
        assert_eq!(posted.inflight_bytes(), 5000);

        // The job for the first 2000 bytes completes cleanly. The optimistic
        // cursor does not move; the durable one catches up partway.
        let partly = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                confirmed_offset: Some(2000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            partly.byte_offset, 5000,
            "confirming must not move the optimistic cursor"
        );
        assert_eq!(partly.inflight_bytes(), 3000);

        let done = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                confirmed_offset: Some(5000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(done.inflight_bytes(), 0);
    }

    /// Review HIGH 1. The two confirm channels are not interchangeable: the
    /// worker's is unconditional because it observed the range, the request
    /// path's is guarded because `skipped`/`duplicate` are evidence about
    /// this request's bytes only. With one unconditional channel, a later
    /// accept at a higher offset silently erases an earlier open gap — the
    /// instrument reading zero over lost bytes.
    #[test]
    fn a_later_settled_accept_does_not_swallow_an_earlier_gap() {
        let db = db_with_bank();
        // A queued retain: bytes 0..5000 handed over, nothing confirmed.
        upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                byte_offset: Some(5000),
                retains_delta: 1,
                ..Default::default()
            },
        )
        .unwrap();

        // A `skipped` at 6000 arrives while that job is still unresolved.
        let after = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                byte_offset: Some(6000),
                confirm_if_settled: Some(6000),
                retains_delta: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(after.byte_offset, 6000);
        assert_eq!(
            after.confirmed_offset, 0,
            "the gap opened by the queued job must survive a later accept"
        );
        assert_eq!(after.inflight_bytes(), 6000);

        // The worker's channel is unconditional and does close it.
        let confirmed = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                confirmed_offset: Some(5000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(confirmed.confirmed_offset, 5000);
        // 5000..6000 is the skipped delta. It stays outstanding — over-
        // reporting, which is the safe direction — until the next clean job
        // confirms past it.
        assert_eq!(confirmed.inflight_bytes(), 1000);

        // With nothing outstanding, the guard opens and the request path
        // confirms normally.
        let settled = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                confirmed_offset: Some(6000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(settled.inflight_bytes(), 0);
        let next = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                byte_offset: Some(7000),
                confirm_if_settled: Some(7000),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            next.confirmed_offset, 7000,
            "with no open gap the request path may settle"
        );
    }

    /// The guard is evaluated against the row as it stood *before* the
    /// write, so the very first thing a session ever sees may settle: a
    /// fresh row has nothing outstanding by construction. Without this the
    /// `skipped`-arrives-first ordering would never confirm anything.
    #[test]
    fn a_settled_accept_on_a_fresh_row_confirms() {
        let db = db_with_bank();
        let row = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                byte_offset: Some(100),
                confirm_if_settled: Some(100),
                retains_delta: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(row.byte_offset, 100);
        assert_eq!(row.confirmed_offset, 100);
        assert_eq!(row.inflight_bytes(), 0);
    }

    #[test]
    fn source_is_set_once_and_end_reason_is_last_write_wins() {
        let db = db_with_bank();
        upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                source: Some("startup"),
                end_reason: Some("other"),
                ended_at: Some(100),
                ..Default::default()
            },
        )
        .unwrap();
        let second = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                source: Some("resume"),
                end_reason: Some("logout"),
                ended_at: Some(200),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(second.source.as_deref(), Some("startup"));
        assert_eq!(second.end_reason.as_deref(), Some("logout"));
        assert_eq!(second.ended_at, Some(200));

        // An update that says nothing about the ending leaves it in place.
        let third = upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                turns: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(third.end_reason.as_deref(), Some("logout"));
        assert_eq!(third.ended_at, Some(200));
    }

    #[test]
    fn deleting_the_bank_cascades_its_sessions() {
        let db = db_with_bank();
        upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s1",
                ..Default::default()
            },
        )
        .unwrap();
        banks::delete(&db, "b1").unwrap();
        assert!(get(&db, "b1", "s1").unwrap().is_none());
    }

    #[test]
    fn upserting_into_a_missing_bank_is_not_found() {
        let db = Db::open_memory().unwrap();
        let err = upsert(
            &db,
            "nope",
            &SessionUpdate {
                session_id: "s1",
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "got {err:?}");
    }

    #[test]
    fn session_id_is_bounded() {
        let db = db_with_bank();
        for sid in ["", &"x".repeat(MAX_SESSION_ID_BYTES + 1)] {
            let err = upsert(
                &db,
                "b1",
                &SessionUpdate {
                    session_id: sid,
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(matches!(err, Error::Invalid(_)), "got {err:?}");
        }
    }

    #[test]
    fn list_orders_by_recency_and_active_only_excludes_ended() {
        let db = db_with_bank();
        banks::create(&db, "b2", None, None).unwrap();
        for sid in ["s1", "s2", "s3"] {
            upsert(
                &db,
                "b1",
                &SessionUpdate {
                    session_id: sid,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        upsert(
            &db,
            "b2",
            &SessionUpdate {
                session_id: "other-bank",
                ..Default::default()
            },
        )
        .unwrap();
        // now_ms() has millisecond resolution and three upserts can share a
        // millisecond, so pin the ordering explicitly instead of racing it.
        db.write(|tx| {
            for (sid, seen) in [("s1", 300i64), ("s2", 200), ("s3", 100)] {
                tx.execute(
                    "UPDATE sessions SET last_seen_at = ?1 WHERE bank_id = 'b1' AND session_id = ?2",
                    params![seen, sid],
                )
                .map_err(store_err)?;
            }
            Ok(())
        })
        .unwrap();

        let all = list(&db, "b1", 10, false).unwrap();
        assert_eq!(
            all.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["s1", "s2", "s3"],
            "most recently seen first, and no other bank's rows"
        );
        assert_eq!(list(&db, "b1", 2, false).unwrap().len(), 2, "limit applies");

        upsert(
            &db,
            "b1",
            &SessionUpdate {
                session_id: "s2",
                end_reason: Some("logout"),
                ended_at: Some(500),
                ..Default::default()
            },
        )
        .unwrap();
        let active = list(&db, "b1", 10, true).unwrap();
        assert!(
            !active.iter().any(|s| s.session_id == "s2"),
            "active_only must exclude a session that reported an end"
        );
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn gc_drops_only_rows_older_than_the_cutoff() {
        let db = db_with_bank();
        for sid in ["old", "new"] {
            upsert(
                &db,
                "b1",
                &SessionUpdate {
                    session_id: sid,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        db.write(|tx| {
            tx.execute(
                "UPDATE sessions SET last_seen_at = 1000 WHERE session_id = 'old'",
                [],
            )
            .map_err(store_err)?;
            tx.execute(
                "UPDATE sessions SET last_seen_at = 3000 WHERE session_id = 'new'",
                [],
            )
            .map_err(store_err)?;
            Ok(())
        })
        .unwrap();

        assert_eq!(gc(&db, 2000).unwrap(), 1);
        assert!(get(&db, "b1", "old").unwrap().is_none());
        assert!(get(&db, "b1", "new").unwrap().is_some());
        // The cutoff is exclusive on equality: a row seen exactly at the
        // cutoff is kept.
        assert_eq!(gc(&db, 3000).unwrap(), 0);
    }

    /// Two writers on one session row, over a **file** database — the shape
    /// C4b produces routinely (a retain arriving while the worker confirms
    /// the previous job) and the configuration production actually runs.
    ///
    /// Deliberately not `Db::open_memory()`: that is a shared-cache database
    /// where a second connection writing the same table gets `SQLITE_LOCKED`,
    /// which `busy_timeout` does **not** retry. A file database under WAL
    /// returns `SQLITE_BUSY` instead, which it does. So this test is also the
    /// evidence that the harness limitation is a harness limitation.
    #[test]
    fn concurrent_writers_on_a_file_database_keep_the_high_water_marks() {
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(Db::open(dir.path().join("sessions.db")).unwrap());
        banks::create(&db, "b1", None, None).unwrap();

        // Interleaved ascending values from both threads: whatever order the
        // writes land in, every monotonic column must end at its maximum and
        // the additive ones at the full count.
        let threads: Vec<_> = [0i64, 1]
            .into_iter()
            .map(|which| {
                let db = db.clone();
                std::thread::spawn(move || {
                    for i in 0..20i64 {
                        let n = i * 2 + which;
                        upsert(
                            &db,
                            "b1",
                            &SessionUpdate {
                                session_id: "s1",
                                turns: Some(n),
                                byte_offset: Some(n * 100),
                                confirmed_offset: Some(n * 100),
                                retains_delta: 1,
                                messages_sent_delta: 1,
                                ..Default::default()
                            },
                        )
                        .expect("a file database must not fail a concurrent upsert");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        let row = get(&db, "b1", "s1").unwrap().unwrap();
        assert_eq!(row.turns, 39);
        assert_eq!(row.byte_offset, 3900);
        assert_eq!(row.confirmed_offset, 3900);
        assert_eq!(row.retains, 40, "no increment may be lost to a race");
        assert_eq!(row.messages_sent, 40);
    }
}
