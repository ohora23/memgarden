-- MemGarden schema v12: the task ledger — working state, not remembered facts.
--
-- Every table before this one stores things that WERE true. This one stores
-- what is being worked on right now: the open commitment, what is done, what
-- is not, and what the next action is. A 2026-06 survey of 435 works names
-- this exact gap and calls the separation its main taxonomic correction:
--
--   "Workflow and task-ledger state is the agent's record of open
--    commitments: tasks in progress, deadlines, dependencies, partially
--    completed multi-step operations, and the obligations they imply. It lets
--    an always-on agent resume a long-running goal after a restart or
--    interruption rather than begin afresh, and it is invisible to a
--    memory-form taxonomy because it is live control state, not remembered
--    information."   (arXiv:2606.30306 §3.2.1)
--
-- Every system that solved cross-session continuity keeps this as a second,
-- structurally separate tier: LangGraph splits thread-scoped checkpointers
-- from its cross-thread Store, CoALA splits working memory (which explicitly
-- holds active goals) from long-term episodic/semantic memory, Letta splits
-- in-context memory blocks from archival memory. MemGarden had only the fact
-- side. This is the other one.
--
-- # One row per bank, not one per session
--
-- This is the load-bearing decision, and it is measured rather than assumed.
-- The live `sessions` table, all 15 rows of it, says a session is not a unit
-- of work here:
--
--     source     : startup 11 · resume 4    <- 'compact', 'clear', 'fork': NEVER
--     end_reason : prompt_input_exit 8 · NULL 6 · other 1
--     lifetime   : 116.7h · 116.0h · 95.7h · 70.2h · 49.7h ...
--
-- A 116-hour session is not one task, and the user switches banks without
-- ending it. Keying the ledger by session would produce fifteen rows a month,
-- each spanning a dozen unrelated tasks. Keyed by bank, "what is going on in
-- this project" has exactly one answer, which is the question a resuming
-- agent asks. `scripts/boundary-replay.py` is the harness; its header carries
-- the full census.
--
-- # Why the fields are NOT NULL with '' defaults
--
-- The extractor is a local 14B model, and this codebase has now been bitten
-- three times by asking it for optional structure: CE-12's `superseded_by`
-- came back unfilled on every call. A field it may omit is a field it will
-- omit. So every field is required in the JSON schema and NOT NULL here, and
-- "nothing to say" is the empty string rather than an absent key.
--
-- # `anchors` is the safety mechanism, not decoration
--
-- The survey names the failure mode of automatic carryover as "false
-- continuity and stale commitments: the agent believes a task is open that
-- has been completed or cancelled, or it resumes an obligation whose
-- preconditions have since changed" — and finds that the corpus almost never
-- tests for it. There is no method to copy.
--
-- Detecting completion by classifying the transcript is the wall CE-12 hit
-- (22 false positives against 0 detections). `anchors` sidesteps it: it holds
-- the git branch, HEAD and touched paths as of the write, so a future reader
-- re-checks them against the filesystem instead of asking a model whether the
-- work is still live. A stat is cheaper than an inference and it is a
-- question that actually has an answer.
--
-- Nothing reads this table yet. The read side is deliberately not built: the
-- write path runs first so the content can be inspected before anything is
-- injected into a prompt on the strength of it.

CREATE TABLE task_ledger (
  bank_id     TEXT    NOT NULL PRIMARY KEY REFERENCES banks(bank_id) ON DELETE CASCADE,
  -- What is being worked toward. The one field whose absence makes the row
  -- worthless, so the writer drops the row rather than storing it empty.
  goal        TEXT    NOT NULL DEFAULT '',
  -- Completed steps, newest first, newline-separated.
  done        TEXT    NOT NULL DEFAULT '',
  -- Open steps, blockers, unresolved decisions.
  open        TEXT    NOT NULL DEFAULT '',
  -- The single next action. Kept separate from `open` because "what do I do
  -- now" and "what is outstanding" are different questions, and a reader that
  -- has to pick the first line out of a list is a reader that will pick wrong.
  next_action TEXT    NOT NULL DEFAULT '',
  -- JSON: {"branch": "...", "head": "...", "paths": [...]}. Re-checked
  -- against the filesystem before this row is ever believed. JSON rather than
  -- columns because it is read as a unit and its shape will grow.
  anchors     TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(anchors)),
  -- Which session and retain job produced this. Plain TEXT with no foreign
  -- key, matching `retain_jobs.session_id`: `sessions` rows are GC'd by age
  -- and this row must outlive them — a ledger that vanished when its session
  -- was collected would be useless for exactly the long gaps it exists for.
  session_id  TEXT,
  job_id      TEXT,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
) STRICT;

-- No index. One row per bank, always fetched by primary key, and there are
-- ten banks. A `WITHOUT ROWID` table was considered and rejected: unlike
-- `sessions` these rows are wide free text, which is the case the SQLite docs
-- name as preferring a rowid table.
