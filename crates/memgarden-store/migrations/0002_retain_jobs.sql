-- MemGarden schema v2 (CE-5b / PR B3): durable retain-job tracking.
--
-- Durable rather than an in-memory map because Phase C's hook renders
-- retention progress and memdash reads it: a daemon restart must not lose a
-- job's outcome ("no restart races" — see the plan's §PR B3).

CREATE TABLE retain_jobs (
  job_id        TEXT    NOT NULL PRIMARY KEY,          -- uuid v7
  bank_id       TEXT    NOT NULL REFERENCES banks(bank_id) ON DELETE CASCADE,
  document_id   INTEGER REFERENCES documents(id) ON DELETE SET NULL,
  session_id    TEXT,
  status        TEXT    NOT NULL CHECK (status IN ('pending','running','done','failed')),
  chunks_total  INTEGER NOT NULL DEFAULT 0,
  chunks_done   INTEGER NOT NULL DEFAULT 0,
  -- Chunks with no extractable content (whitespace/punctuation only). Kept
  -- apart from chunks_done so an all-chunks-failed job cannot look partially
  -- successful just because some chunks were junk.
  chunks_skipped INTEGER NOT NULL DEFAULT 0,
  -- Critic Revision R14: a single chunk whose LLM call fails must not fail the
  -- whole job. It bumps this counter and the run continues; only an all-chunks
  -- failure marks the job 'failed'.
  chunks_failed INTEGER NOT NULL DEFAULT 0,
  facts_written INTEGER NOT NULL DEFAULT 0,
  error         TEXT,
  detail        TEXT    CHECK (detail IS NULL OR json_valid(detail)),  -- token accounting
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_retain_jobs_bank_status ON retain_jobs(bank_id, status, created_at DESC);
