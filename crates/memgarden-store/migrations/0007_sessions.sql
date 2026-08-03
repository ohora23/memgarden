-- MemGarden schema v7 (HK-1a / PR C1): Claude Code session and turn state.
--
-- The row Phase A deferred and `docs/parity-gaps.md` still owed. It is the
-- daemon-side **mirror** of the hook's per-session state file, and it answers
-- the two questions `retain_jobs` structurally cannot: "where are we in this
-- transcript" and "how many turns has this session had".
--
-- Division of labour, stated here because review enforces it:
--   * `retain_jobs` = one row per retain **request** (chunk counts, facts
--     written, per-chunk failures, the token-accounting `detail` blob).
--   * `sessions`    = one row per **(bank, session)** (the two transcript
--     cursors and turn accounting).
-- `sessions.retains` is a count; the detail behind each one stays in
-- `retain_jobs`, joined on `session_id`. Neither table carries a column of
-- the other.

CREATE TABLE sessions (
  bank_id           TEXT    NOT NULL REFERENCES banks(bank_id) ON DELETE CASCADE,
  session_id        TEXT    NOT NULL,
  cwd               TEXT,
  transcript_path   TEXT,
  -- SessionStart source: startup|resume|clear|compact|fork
  source            TEXT,
  -- SessionEnd reason: clear|resume|logout|prompt_input_exit|
  --                    bypass_permissions_disabled|other
  end_reason        TEXT,
  turns             INTEGER NOT NULL DEFAULT 0,  -- Stop hook invocations
  retains           INTEGER NOT NULL DEFAULT 0,  -- accepted retains
  chunk_index       INTEGER NOT NULL DEFAULT 0,
  -- Optimistic cursor: what the hook has POSTed. May be ahead of reality
  -- while a job is queued; never rewinds (see confirmed_offset).
  byte_offset       INTEGER NOT NULL DEFAULT 0,
  -- Durable cursor: bytes whose retain job reached status='done'. This is
  -- the one to trust; the gap between the two is work in flight or lost.
  confirmed_offset  INTEGER NOT NULL DEFAULT 0,
  messages_sent     INTEGER NOT NULL DEFAULT 0,
  compactions       INTEGER NOT NULL DEFAULT 0,
  started_at        INTEGER NOT NULL,
  last_seen_at      INTEGER NOT NULL,
  ended_at          INTEGER,
  PRIMARY KEY (bank_id, session_id)
) STRICT, WITHOUT ROWID;

-- `WITHOUT ROWID` for the same reason as `links` and `node_tags` in
-- `0001_init.sql`: a two-TEXT primary key, small rows, always accessed by PK.
-- Nothing keys off a `sessions` rowid, so unlike `mental_models` there is no
-- vec0/fts5 mirror to keep in step and therefore no `AFTER DELETE` trigger:
-- the `ON DELETE CASCADE` above is the whole cleanup story.

-- The dashboard's list order, and the GC scan (DB-1, C1's `sessions::gc`).
CREATE INDEX idx_sessions_last_seen ON sessions(last_seen_at DESC);
