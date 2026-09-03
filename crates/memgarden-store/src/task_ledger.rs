//! `task_ledger` persistence (migration `0012`). Working state, one row per
//! bank.
//!
//! Everything else in this crate stores what WAS true. This stores what is
//! being worked on: the open commitment, what is not done, and the next
//! action. See `0012_task_ledger.sql` for why that is a separate tier rather
//! than more facts, and why the key is the bank rather than the session.
//!
//! There is no "what is done" field. There was one until migration `0013`,
//! and on all five live rows read before anything consumed them it was a
//! shorter copy of a `memory_nodes` row from the same job. Completed steps
//! are facts, and the fact tier already holds them.
//!
//! **Nothing reads this yet.** [`get`] exists so the write path can be
//! verified and the rows inspected; no recall or hook path calls it. The read
//! side is deliberately unbuilt until the stored content has been looked at,
//! because the only published measurement on this machine — MX-3 — found
//! memory costing 5% more tokens on its sample, and a fact-recall win does
//! not imply a state-carryover win.
//!
//! # Replace, do not accumulate
//!
//! [`upsert`] overwrites every content field. The ledger is a snapshot of
//! current state, not a log: a superseded goal that stays behind is the
//! survey's "stale commitment", which is the failure this whole tier has to
//! avoid rather than cause. History of what was worked on already exists —
//! that is what `memory_nodes` is.
//!
//! `created_at` is the exception and is preserved across upserts, so "how
//! long has this bank had a ledger" survives the overwrite.

use rusqlite::OptionalExtension;

use memgarden_core::error::{Error, Result};
use memgarden_core::now_ms;

use crate::Db;
use crate::store_err;

/// Per-field cap, applied at the store boundary.
///
/// The writer's JSON schema already bounds each string, but a schema bound is
/// a request to Ollama and this is an invariant of the table. They are not
/// the same thing: `/api/chat` silently ignores `format` entirely, which is a
/// documented way for an unbounded reply to reach a caller that asked for a
/// bounded one (`mental::REFRESH_CONTENT_MAX_CHARS`).
pub const MAX_FIELD_CHARS: usize = 2000;

/// `anchors` is small and structured; a large one means the writer is putting
/// something in it that does not belong.
pub const MAX_ANCHORS_CHARS: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskLedger {
    pub bank_id: String,
    pub goal: String,
    pub open: String,
    pub next_action: String,
    /// JSON object: `{"branch": …, "head": …, "paths": [...]}`.
    pub anchors: String,
    pub session_id: Option<String>,
    pub job_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// What a writer supplies. `bank_id` is separate because it is the key.
#[derive(Debug, Clone, Default)]
pub struct LedgerUpdate<'a> {
    pub goal: &'a str,
    pub open: &'a str,
    pub next_action: &'a str,
    pub anchors: &'a str,
    pub session_id: Option<&'a str>,
    pub job_id: Option<&'a str>,
}

/// Truncates on a char boundary. Silent, because the alternative — refusing
/// the write — discards a ledger over a field that is merely too long, and a
/// truncated ledger is worth more than no ledger.
fn cap(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Writes the bank's ledger, replacing any existing content.
///
/// Rejects an empty `goal`: a row that cannot say what is being worked toward
/// is not a degraded ledger, it is noise that a future reader would have to
/// filter. The caller's correct response is to write nothing.
///
/// Malformed `anchors` is refused by the table's `json_valid` CHECK, mapped
/// here to an error that names the field. Validating in Rust instead would
/// mean a `serde_json` dependency this crate does not have and does not need
/// — `banks::disposition` already takes exactly this route.
pub fn upsert(db: &Db, bank_id: &str, u: &LedgerUpdate<'_>) -> Result<TaskLedger> {
    if u.goal.trim().is_empty() {
        return Err(Error::Invalid("task ledger goal is empty".into()));
    }
    let anchors = if u.anchors.trim().is_empty() {
        "{}"
    } else {
        cap(u.anchors, MAX_ANCHORS_CHARS)
    };
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "INSERT INTO task_ledger
             (bank_id, goal, open, next_action, anchors,
              session_id, job_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(bank_id) DO UPDATE SET
             goal        = excluded.goal,
             open        = excluded.open,
             next_action = excluded.next_action,
             anchors     = excluded.anchors,
             session_id  = excluded.session_id,
             job_id      = excluded.job_id,
             updated_at  = excluded.updated_at",
            rusqlite::params![
                bank_id,
                cap(u.goal, MAX_FIELD_CHARS),
                cap(u.open, MAX_FIELD_CHARS),
                cap(u.next_action, MAX_FIELD_CHARS),
                anchors,
                u.session_id,
                u.job_id,
                now,
            ],
        )
        .map_err(map_ledger_err)?;
        Ok(())
    })?;

    get(db, bank_id)?.ok_or_else(|| Error::Invalid("task ledger vanished after write".into()))
}

/// A CHECK violation on this table can only be `json_valid(anchors)`; anything
/// else falls through to the generic storage mapping.
fn map_ledger_err(e: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(ref ffi_err, _) = e
        && ffi_err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
    {
        return Error::Invalid("task ledger anchors is not valid JSON".into());
    }
    store_err(e)
}

/// The bank's ledger, or `None` when it has none.
pub fn get(db: &Db, bank_id: &str) -> Result<Option<TaskLedger>> {
    let conn = db.read()?;
    conn.query_row(
        "SELECT bank_id, goal, open, next_action, anchors,
                session_id, job_id, created_at, updated_at
           FROM task_ledger WHERE bank_id = ?1",
        [bank_id],
        |r| {
            Ok(TaskLedger {
                bank_id: r.get(0)?,
                goal: r.get(1)?,
                open: r.get(2)?,
                next_action: r.get(3)?,
                anchors: r.get(4)?,
                session_id: r.get(5)?,
                job_id: r.get(6)?,
                created_at: r.get(7)?,
                updated_at: r.get(8)?,
            })
        },
    )
    .optional()
    .map_err(store_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::banks;

    fn db() -> Db {
        let db = Db::open_memory().expect("open");
        banks::create(&db, "b", None, None).expect("bank");
        db
    }

    fn update<'a>(goal: &'a str) -> LedgerUpdate<'a> {
        LedgerUpdate {
            goal,
            ..Default::default()
        }
    }

    #[test]
    fn absent_ledger_is_none_not_an_error() {
        assert_eq!(get(&db(), "b").expect("get"), None);
    }

    #[test]
    fn upsert_replaces_rather_than_accumulating() {
        let db = db();
        upsert(
            &db,
            "b",
            &LedgerUpdate {
                goal: "old goal",
                open: "old open",
                ..Default::default()
            },
        )
        .expect("first");
        let second = upsert(
            &db,
            "b",
            &LedgerUpdate {
                goal: "new goal",
                ..Default::default()
            },
        )
        .expect("second");

        // The stale-commitment failure this tier exists to avoid: an open
        // item from a finished goal must not survive into the next one.
        assert_eq!(second.goal, "new goal");
        assert_eq!(second.open, "");
    }

    #[test]
    fn created_at_survives_an_upsert() {
        let db = db();
        let first = upsert(&db, "b", &update("a")).expect("first");
        let second = upsert(&db, "b", &update("b")).expect("second");
        assert_eq!(first.created_at, second.created_at);
    }

    #[test]
    fn an_empty_goal_is_refused() {
        let db = db();
        assert!(upsert(&db, "b", &update("   ")).is_err());
        assert_eq!(get(&db, "b").expect("get"), None, "nothing was written");
    }

    #[test]
    fn absent_anchors_becomes_an_empty_object_not_an_empty_string() {
        // The table's CHECK requires valid JSON, so "" would fail the write.
        let led = upsert(&db(), "b", &update("g")).expect("upsert");
        assert_eq!(led.anchors, "{}");
    }

    #[test]
    fn malformed_anchors_is_refused_by_field_name() {
        let db = db();
        let err = upsert(
            &db,
            "b",
            &LedgerUpdate {
                goal: "g",
                anchors: "{not json",
                ..Default::default()
            },
        )
        .expect_err("must refuse");
        assert!(err.to_string().contains("anchors"), "got {err}");
    }

    #[test]
    fn an_overlong_field_is_truncated_on_a_char_boundary() {
        let db = db();
        // Multi-byte, so a naive byte slice would panic mid-character.
        let long = "가".repeat(MAX_FIELD_CHARS);
        let led = upsert(
            &db,
            "b",
            &LedgerUpdate {
                goal: "g",
                open: &long,
                ..Default::default()
            },
        )
        .expect("upsert");
        assert!(led.open.len() <= MAX_FIELD_CHARS);
        assert!(!led.open.is_empty());
    }

    #[test]
    fn the_ledger_cascades_with_its_bank() {
        let db = db();
        upsert(&db, "b", &update("g")).expect("upsert");
        banks::delete(&db, "b").expect("delete bank");
        assert_eq!(get(&db, "b").expect("get"), None);
    }
}
