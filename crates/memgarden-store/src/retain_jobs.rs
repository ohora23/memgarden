//! `retain_jobs` persistence (migration `0002`). The background retain
//! worker owns a job's whole state in memory while it runs and flushes it
//! here, so there is exactly one `update` entry point rather than a setter
//! per counter.

use rusqlite::{OptionalExtension, params};

use memgarden_core::error::Result;
use memgarden_core::now_ms;

use crate::{Db, store_err};

#[derive(Debug, Clone, PartialEq)]
pub struct RetainJob {
    pub job_id: String,
    pub bank_id: String,
    pub document_id: Option<i64>,
    pub session_id: Option<String>,
    pub status: String,
    pub chunks_total: i64,
    pub chunks_done: i64,
    pub chunks_skipped: i64,
    pub chunks_failed: i64,
    pub facts_written: i64,
    pub error: Option<String>,
    pub detail: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// The transcript byte range this job carries, as the request reported it.
    ///
    /// `offset_from` is the guard the durable cursor needs: a clean job may
    /// only confirm past its own start, so a later job cannot confirm over an
    /// earlier one's gap (migration `0008`). `offset_to` is stored with it so
    /// a job row can answer "which bytes did you carry?" on its own — the
    /// question a shadow run asks the moment it sees a gap.
    ///
    /// `0, 0` on a row written before migration `0008`, and on any caller that
    /// is not the hook: it means "unknown", not "zero bytes".
    pub offset_from: i64,
    pub offset_to: i64,
}

/// Mutable job state, written wholesale on every flush.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobProgress {
    pub status: JobStatus,
    pub chunks_total: i64,
    pub chunks_done: i64,
    pub chunks_skipped: i64,
    pub chunks_failed: i64,
    pub facts_written: i64,
    pub error: Option<String>,
    /// JSON object string (the table has `CHECK (json_valid(detail))`).
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JobStatus {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
        }
    }
}

/// Creates the job row in `pending`. Called synchronously by the retain
/// endpoint before enqueueing, so `GET /v1/retain/{job_id}` is answerable
/// the instant the 202 lands.
/// `range` is the transcript byte span the job carries — `None` for any caller
/// that is not the hook, which stores `0, 0` and means "unknown".
pub fn insert(
    db: &Db,
    job_id: &str,
    bank_id: &str,
    document_id: Option<i64>,
    session_id: Option<&str>,
    detail: Option<&str>,
    range: Option<(i64, i64)>,
) -> Result<()> {
    let now = now_ms();
    let (offset_from, offset_to) = range.unwrap_or((0, 0));
    db.write(|tx| {
        tx.execute(
            "INSERT INTO retain_jobs
             (job_id, bank_id, document_id, session_id, status, detail, created_at, updated_at,
              offset_from, offset_to)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?6, ?7, ?8)",
            params![
                job_id,
                bank_id,
                document_id,
                session_id,
                detail,
                now,
                offset_from.max(0),
                offset_to.max(0)
            ],
        )
        .map_err(store_err)?;
        Ok(())
    })
}

pub fn update(db: &Db, job_id: &str, p: &JobProgress) -> Result<()> {
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "UPDATE retain_jobs SET status = ?1, chunks_total = ?2, chunks_done = ?3,
             chunks_skipped = ?4, chunks_failed = ?5, facts_written = ?6, error = ?7,
             detail = coalesce(?8, detail), updated_at = ?9
             WHERE job_id = ?10",
            params![
                p.status.as_str(),
                p.chunks_total,
                p.chunks_done,
                p.chunks_skipped,
                p.chunks_failed,
                p.facts_written,
                p.error,
                p.detail,
                now,
                job_id,
            ],
        )
        .map_err(store_err)?;
        Ok(())
    })
}

pub fn get(db: &Db, job_id: &str) -> Result<Option<RetainJob>> {
    let conn = db.read()?;
    conn.query_row(
        "SELECT job_id, bank_id, document_id, session_id, status, chunks_total, chunks_done,
                chunks_skipped, chunks_failed, facts_written, error, detail, created_at,
                updated_at, offset_from, offset_to
         FROM retain_jobs WHERE job_id = ?1",
        params![job_id],
        |r| {
            Ok(RetainJob {
                job_id: r.get(0)?,
                bank_id: r.get(1)?,
                document_id: r.get(2)?,
                session_id: r.get(3)?,
                status: r.get(4)?,
                chunks_total: r.get(5)?,
                chunks_done: r.get(6)?,
                chunks_skipped: r.get(7)?,
                chunks_failed: r.get(8)?,
                facts_written: r.get(9)?,
                error: r.get(10)?,
                detail: r.get(11)?,
                created_at: r.get(12)?,
                updated_at: r.get(13)?,
                offset_from: r.get(14)?,
                offset_to: r.get(15)?,
            })
        },
    )
    .optional()
    .map_err(store_err)
}

/// Marks every job still `pending`/`running` as `failed`. Called once at
/// startup: the queue lives in memory, so a job left mid-flight by a crash
/// or restart will never be picked up again and must not sit "running"
/// forever in the Phase C hook's progress view.
pub fn fail_stale(db: &Db, reason: &str) -> Result<usize> {
    let now = now_ms();
    db.write(|tx| {
        tx.execute(
            "UPDATE retain_jobs SET status = 'failed', error = ?1, updated_at = ?2
             WHERE status IN ('pending','running')",
            params![reason, now],
        )
        .map_err(store_err)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::banks;

    #[test]
    fn insert_update_get_roundtrip() {
        let db = Db::open_memory().unwrap();
        banks::create(&db, "b1", None, None).unwrap();

        insert(
            &db,
            "job-1",
            "b1",
            None,
            Some("sess-1"),
            Some(r#"{"a":1}"#),
            Some((100, 5000)),
        )
        .unwrap();
        let job = get(&db, "job-1").unwrap().unwrap();
        assert_eq!(job.status, "pending");
        assert_eq!(job.session_id.as_deref(), Some("sess-1"));
        assert_eq!(job.detail.as_deref(), Some(r#"{"a":1}"#));

        update(
            &db,
            "job-1",
            &JobProgress {
                status: JobStatus::Done,
                chunks_total: 4,
                chunks_done: 2,
                chunks_skipped: 1,
                chunks_failed: 1,
                facts_written: 7,
                error: None,
                detail: None,
            },
        )
        .unwrap();
        let job = get(&db, "job-1").unwrap().unwrap();
        assert_eq!(job.status, "done");
        assert_eq!(job.chunks_skipped, 1);
        assert_eq!(job.chunks_failed, 1);
        assert_eq!(job.facts_written, 7);
        // detail: None must preserve the insert-time value, not null it.
        assert_eq!(job.detail.as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn unknown_job_is_none() {
        let db = Db::open_memory().unwrap();
        assert!(get(&db, "nope").unwrap().is_none());
    }

    #[test]
    fn bad_status_violates_check() {
        let db = Db::open_memory().unwrap();
        banks::create(&db, "b1", None, None).unwrap();
        insert(&db, "job-1", "b1", None, None, None, None).unwrap();
        let err = db.write(|tx| {
            tx.execute(
                "UPDATE retain_jobs SET status = 'bogus' WHERE job_id = 'job-1'",
                [],
            )
            .map_err(store_err)
        });
        assert!(err.is_err(), "status CHECK must reject unknown values");
    }

    #[test]
    fn fail_stale_closes_inflight_jobs() {
        let db = Db::open_memory().unwrap();
        banks::create(&db, "b1", None, None).unwrap();
        insert(&db, "job-1", "b1", None, None, None, None).unwrap();
        insert(&db, "job-2", "b1", None, None, None, None).unwrap();
        update(
            &db,
            "job-2",
            &JobProgress {
                status: JobStatus::Done,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(fail_stale(&db, "daemon restarted").unwrap(), 1);
        assert_eq!(get(&db, "job-1").unwrap().unwrap().status, "failed");
        assert_eq!(get(&db, "job-2").unwrap().unwrap().status, "done");
    }
}
