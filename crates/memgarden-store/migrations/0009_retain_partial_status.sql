-- MemGarden schema v9: `partial` is a status a retain job can end in.
--
-- Measured on the live daemon 2026-08-25: of the last twelve jobs, **four
-- finished `done` having lost chunks** — 16 of 95 chunks failed overall — and
-- the only trace was a counter nobody reads.
--
-- The recovery route already existed. `retain::run` withholds the document's
-- content hash whenever a chunk failed, so re-posting the same transcript
-- re-ingests it rather than being dismissed as a duplicate. What was missing
-- was anything saying re-posting was needed: two jobs both read `done`
-- whether or not memories had been dropped.
--
-- SQLite cannot alter a CHECK in place, so the table is rebuilt. The column
-- list, the foreign keys, the `detail` json_valid check and `STRICT` are
-- reproduced exactly; only the status CHECK gains a value.
--
-- **Existing rows carry over untouched.** A job already recorded `done` with
-- `chunks_failed > 0` stays `done`: rewriting history into a status that did
-- not exist when it ran would make the record claim something nobody saw.

PRAGMA foreign_keys = OFF;

CREATE TABLE retain_jobs_new (
  job_id        TEXT    NOT NULL PRIMARY KEY,
  bank_id       TEXT    NOT NULL REFERENCES banks(bank_id) ON DELETE CASCADE,
  document_id   INTEGER REFERENCES documents(id) ON DELETE SET NULL,
  session_id    TEXT,
  status        TEXT    NOT NULL CHECK (status IN ('pending','running','done','partial','failed')),
  chunks_total  INTEGER NOT NULL DEFAULT 0,
  chunks_done   INTEGER NOT NULL DEFAULT 0,
  chunks_skipped INTEGER NOT NULL DEFAULT 0,
  chunks_failed INTEGER NOT NULL DEFAULT 0,
  facts_written INTEGER NOT NULL DEFAULT 0,
  error         TEXT,
  detail        TEXT    CHECK (detail IS NULL OR json_valid(detail)),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  offset_from   INTEGER NOT NULL DEFAULT 0,
  offset_to     INTEGER NOT NULL DEFAULT 0
) STRICT;

INSERT INTO retain_jobs_new
  (job_id, bank_id, document_id, session_id, status, chunks_total, chunks_done,
   chunks_skipped, chunks_failed, facts_written, error, detail, created_at,
   updated_at, offset_from, offset_to)
SELECT
   job_id, bank_id, document_id, session_id, status, chunks_total, chunks_done,
   chunks_skipped, chunks_failed, facts_written, error, detail, created_at,
   updated_at, offset_from, offset_to
FROM retain_jobs;

DROP TABLE retain_jobs;
ALTER TABLE retain_jobs_new RENAME TO retain_jobs;

CREATE INDEX idx_retain_jobs_bank_status ON retain_jobs(bank_id, status, created_at DESC);

PRAGMA foreign_keys = ON;
