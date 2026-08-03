# C1 / HK-1a — session and turn state

Migration `0007_sessions.sql`, `store::sessions`, `POST/GET
/v1/banks/{bank_id}/sessions`, the retain-side mirror, and the session GC.
First PR of Phase C; daemon-side only, no CLI yet.

Closes the `docs/parity-gaps.md` row *"Session/turn-state tables"*.

## What this table is for

`retain_jobs` (0002) answers *"how did this one retain request go"*. Nothing
answered *"where are we in this transcript"* or *"how many turns has this
session had"* — the PRD's `턴/retention 상태`, deferred by Phase A. `sessions`
is that, and nothing more.

One row per `(bank_id, session_id)`. `WITHOUT ROWID` for the same reason as
`links` and `node_tags` in `0001_init.sql`: a two-TEXT primary key, small
rows, always accessed by PK.

## Two cursors, and why one would not do

| column | meaning |
|---|---|
| `byte_offset` | **optimistic** — the transcript position the hook has POSTed |
| `confirmed_offset` | **durable** — bytes whose ingestion is a settled fact |

`POST …/retain` answers **202 when a job is queued, not when it is ingested**
(`routes/retain.rs`). The worker can still fail a chunk, and it deliberately
withholds the document content hash on any non-clean run so that re-POSTing
the same transcript starts a fresh job instead of being dismissed as a
duplicate (`retain/mod.rs`, "Review HIGH 1"). The hook's recovery protocol
(plan §Binding decisions #8) depends on that: it commits its cursor
optimistically and rolls it back when `GET /v1/retain/{job_id}` says `failed`.

Both columns are merged `MAX(existing, incoming)`, so an out-of-order
`async: true` `Stop` cannot rewind the mirror. A **single** monotonic column
would keep that invariant and destroy the recovery path: there would be no way
to say "these bytes were handed over but never landed". With two,
`byte_offset - confirmed_offset` is exactly the in-flight-or-lost window. It
is served as `inflight_bytes` on every read rather than left to the caller,
because it is the number the runbook tells an operator to watch and a
dashboard that has to compute it is a dashboard that will forget to.

Observed on the manual run below: `inflight 57344` mid-job, `0` once the job
reached `done`.

## Reconciliation with `retain_jobs`

Enforced, not merely intended: **no field is written by both paths.**

| table | grain | owns |
|---|---|---|
| `retain_jobs` | one row per retain **request** | chunk counts, facts written, per-chunk failures, the token-accounting `detail` blob |
| `sessions` | one row per **(bank, session)** | the two cursors, turn accounting |

The counts relate but are **not equal**, and the earlier draft of this note
claimed an identity that its own test contradicts:

```text
sessions.retains >= count(retain_jobs WHERE session_id = …)
   delta = the accepts that queue no job: `skipped` + `duplicate`
```

`skipped_and_duplicate_settle_only_when_nothing_is_outstanding` ends at
`retains == 3` against **one** job row, and now asserts both numbers side by
side so the divergence is visible rather than surprising.

`messages_sent` is the same shape: a running sum of a value
`retain_jobs.detail.message_count` records per request. It equals the sum
over jobs only while no delta is ever re-sent — it is **additive, therefore
not idempotent** under the rollback-and-resend of §Binding decisions #8. The
cursors are the fields that survive that, which is why they are the ones
anything reasons over and `messages_sent` is only ever a rough volume figure.

Within `sessions`, every field has one writer class:

| field | writer |
|---|---|
| `cwd`, `transcript_path`, `source`, `end_reason`, `ended_at`, `turns`, `chunk_index`, `byte_offset`, `compactions` | the hook — `POST …/sessions` or the retain request |
| `retains`, `messages_sent` | the daemon, `+=` once per accepted retain |
| `confirmed_offset` | the daemon, only from a settlement it observed |

### The durable cursor has two channels, and they are not interchangeable

No client field on any endpoint is named `confirmed_offset`. It advances two
ways:

1. **The worker, unconditionally** (`SessionUpdate::confirmed_offset`), inside
   `retain/mod.rs`'s existing `if clean { … }` block — the same condition, and
   the same `spawn_blocking`, as the content hash. Unconditional is correct
   here: the worker observed the entire range it is confirming.
2. **The request path, guarded** (`SessionUpdate::confirm_if_settled`), for
   `skipped` and `duplicate`. Applied only when the row has no open gap
   (`confirmed_offset >= byte_offset`) at the moment of the write.

The guard is review HIGH 1, and the first draft of this PR did not have it.
The reasoning that produced the bug — "neither outcome queues work, so there
is nothing to leave behind" — is airtight about *this request's* bytes and
false about earlier ones. Because the column merges with `MAX`, an
unconditional confirm at a higher offset erases any gap an earlier
queued-then-failed job left:

```
POST …/retain byte_offset=5000            -> 202, byte=5000 confirmed=0   [gap 5000]
POST …/retain byte_offset=6000 (skipped)  -> 200, byte=6000 confirmed=6000 [gap gone]
```

Nothing ingested 0..5000, one `retain_jobs` row is still unfinished, and the
instrument this PR exists to build reads `inflight_bytes: 0` over 5000 lost
bytes. A role-filtered delta emptying out is what the plan itself calls
"ordinary, not exotic" with `include_tool_calls = false`, so this needed no
hook bug to fire. The `duplicate` arm had the same hole and the old test
asserted it: it confirmed at 1500 on a re-POST whose byte-identical payload
was itself proof that nothing had ingested 900..1500.

**Ordering walk-through**, since the guard is only as good as the orderings
it survives:

| ordering | outcome |
|---|---|
| `skipped` first, fresh row | confirms — a new row has nothing outstanding by construction, so the INSERT branch applies it unguarded |
| queued(5000) → `skipped`(6000) | gap survives at 6000; nothing confirmed |
| queued(5000) → clean → `skipped`(6000) | confirms to 6000 |
| stale `skipped`(3000) onto a settled row at 9000 | no-op; `MAX` absorbs it |
| accept with no `byte_offset` | no-op; the channel carries `0` |
| queued(5000) → `skipped`(6000) → worker confirms 5000 | gap narrows to 1000, not 0 |

Only the last one is lossy in the reporting direction, and it errs the safe
way: 5000..6000 was genuinely skipped, so the residual 1000 is **over**-
reporting outstanding work, and the next clean job confirming past 6000
closes it. For an instrument whose entire job is to notice loss, over-
reporting is the correct failure mode.

`0` is passed for "no conditional confirm", never `NULL`: SQLite's scalar
`max()` returns `NULL` if any argument is `NULL`, which would blank the
column instead of leaving it alone.

### The mirror never fails a retain

Review HIGH 2. `mirror_session` logs and swallows every error, matching what
the worker already did with the identical write. Before that, a `?` on the
mirror could return 4xx/5xx *after* the document, the ledger row and the job
row had committed — leaving a `pending` job nothing would ever dispatch or
fail, which a §Binding-#8 hook then polls for the rest of the session,
skipping every turn. It also violated an invariant `routes/retain.rs` states
a dozen lines above: *"a full queue must be a clean 429, not an orphaned
document + job row."*

The rule, stated once so everything else follows from it: **the mirror is
diagnostic and recovery state; ingestion is the product; the mirror never
fails a retain.**

The one thing that *is* a hard 400 is an over-long `session_id`, and it is
checked at the top of `retain_inner` with the other guards, before any DB
work — the same reason the queue permit is reserved there. (`POST …/retain`
did not bound `session_id` before this PR; discovering the bound inside the
mirror is what orphaned the job.)

## Merge semantics

* `source` — **first write wins**. A session's origin is decided by its
  `SessionStart`; a later `resume` upsert must not rewrite history.
* `end_reason` / `ended_at` / `cwd` / `transcript_path` — set when supplied,
  untouched when absent (last write wins).
* `turns`, `chunk_index`, `byte_offset`, `confirmed_offset`, `compactions` —
  monotonic `MAX`. These are cumulative **absolutes** read from the hook's own
  state file, not per-request increments, so a retry or a reordered `Stop` is
  idempotent rather than double-counted.
* `retains`, `messages_sent` — additive. Daemon-owned, one write per accepted
  retain, so each write is a distinct event and `+=` is the only thing that
  can be right.

`retain` with `byte_offset`/`turn` writes the row; `retain` without them
counts the accept and leaves every mirrored value alone
(`a_retain_without_the_hook_fields_does_not_clobber_the_mirror`).

`last_seen_at` is refreshed on every upsert. Obvious, and it was pinned by
nothing until review mutation-tested it: freezing it at the insert value
passes the whole suite while making GC expire a busy session 90 days after it
*started* and degrading the dashboard's `ORDER BY last_seen_at DESC` into
creation order. `insert_then_update_roundtrip` now forces it to `0` and
asserts the next upsert brings it back to ~now. The additive rule on
`messages_sent` had the same hole — `+` mutated to `max(...)` also survived —
and now has the same two-write assertion `retains` already had.

## Recovery seeds from `confirmed_offset`, never `byte_offset`

C2b rebuilds a wiped state file from `GET …/sessions/{session_id}`. **It must
take its `offset` from `confirmed_offset`.**

`byte_offset` is the obvious reading and it is the unsafe one. It is what the
hook *POSTed*, so it is already ahead of reality after a failed job — and
after the byte-budget 429 in `routes/retain.rs`, where the mirror advanced
and the hook deliberately did not. Seeding from it makes a recovering hook
skip exactly the bytes the dual cursor exists to protect: silent loss,
through the recovery door, in the PR that closed the gap.

Re-sending from the durable cursor is safe and cheap. Identical content under
the same `doc_key` is caught by the content-hash dedup and answers
`duplicate`, so **at-least-once is the correct posture** here; the cost of
over-sending is one round trip and the cost of under-sending is a fact that
never existed.

Corollary, and the reason the above is sufficient: the mirror carries **no
`pending` job id**, so a hook whose state dir was wiped cannot reconcile an
in-flight job it never saw. It does not need to — `confirmed_offset` is by
construction behind anything unresolved, so re-sending from it covers the
in-flight range too. **Do not add a `pending_job_id` column** to solve a
problem this ordering does not have; the `pending` record belongs in the
hook's local state (§Binding decisions #8), where the rollback also lives.

## No trigger, and the reason

`0004_consolidation.sql:30` needed one because an FK cascade removes rows no
Rust code sees, and `proof_count` is derived from them. `sessions` has the
same `ON DELETE CASCADE` from `banks` — but nothing is derived from a
`sessions` row: no vec0 or fts5 mirror keys off it (it has no rowid to key
off), and `retain_jobs.session_id` is a plain `TEXT` with no foreign key, so
it is cascaded independently by its own `bank_id`. The cascade is the whole
cleanup story. Checked, not assumed.

## GC

Rows outlive their last sighting by `SESSION_RETENTION_DAYS` (90), collected
on the existing metrics-snapshot tick — one indexed `DELETE` over
`idx_sessions_last_seen`, on a timer that already runs. Unbounded session
accumulation is what pushed legacy into its 10,000-entry truncation hack
(`state.py:111-114`); a slower, simpler answer is enough to prevent it.

`// ponytail:` the retention window is a constant, not config. C2a introduces
the `[hooks]` section with `session_retention_days` for the CLI, and this
becomes `cfg.hooks.session_retention_days` then; adding half of that section
here would only collide with that PR. 90 is the value C2a will default to.
`the_metrics_tick_expires_stale_sessions` asserts through `tick`, so removing
the wiring fails the test, and it derives its fixture ages from the constant
so it pins the window that actually reaches the `DELETE`.

## Diverged from legacy

* **Two cursors where legacy has none.** Legacy tracks retention position as
  a message *index* in `retention_tracking.json` and re-parses the whole
  transcript on every `Stop` (`retain.py:39-70` → `state.py:164-193`). Byte
  offsets are plan §Binding decisions #6; the split into optimistic and
  durable is C1's, and it exists because our daemon's 202 is a queue
  acknowledgement, which legacy's client-side model has no equivalent of.
* **Compaction is a counter, not a control signal.** `compactions` is
  recorded and never acted on. `state.py:178-183` treats "transcript shrank"
  as compaction and resets to index 0; the transcript is append-only on
  Claude Code 2.1.220 (verified: 2 compactions at 1-indexed lines 2170/4629,
  file only grows), so the compaction summary is *new content we want*, not a
  signal to restart.
* **Session state lives server-side.** Legacy keeps it only in the hook's
  local JSON files. Mirroring it in the daemon is what lets a hook whose
  state dir was wiped recover from `GET …/sessions/{session_id}` (C2b), and
  what gives the dashboard something to read (DB-1).
* **Bounded by age, not by count.** Legacy truncates `turns.json` at 10,000
  entries. A time-bounded delete over an index is cheaper and does not
  silently drop the newest thing when a burst arrives.
* **`source` and `end_reason` are stored, not validated.** Claude Code owns
  both vocabularies and extends them (`bypass_permissions_disabled` is
  recent). A mirror that 400s on a value it has not heard of would break the
  hook on a Claude Code upgrade. Bounded to 64 bytes; not enum-checked.

## Wire contract for C4b, decided here

C1 owns the mirror's wire contract, so the two fields review flagged are
settled in this PR rather than deferred.

* **`chunk` rides the retain payload.** The plan's §C4b step 7 payload had no
  `chunk`, which would have left `chunk_index` with no writer but
  `POST …/sessions`. Mirroring it at `session-end` does **not** close the case
  the column exists for: the failure is a state-dir wipe *mid*-session, where
  recovery restores `chunk = 0`, the hook reuses the bare `session_id` as its
  `document_id`, and `document_metadata` rebuilds `documents.metadata` from
  scratch — overwriting chunk 0's `message_count`/`files_modified`. A session
  that already reported `session-end` never needs recovering. Only a
  per-retain writer closes it, so `RetainRequest` gained `chunk`.
* **`compaction` is renamed `compactions`.** The semantics were already
  cumulative and correct — the only reading idempotent under
  rollback-and-resend — but the wire field was singular while the hook state
  field, the DB column and C4a's `Delta` are all plural. A C4b author reaches
  for `Delta.compactions`, and the wrong number never errors: it just reads
  `1` forever. Renamed while it has zero callers.

One thing C4b must handle rather than inherit: §Binding decisions #8 rolls
back `offset` and `chunk` on a failed job but **not** `compactions`, so a
rollback-and-resend re-counts the `compact_boundary` lines in the re-sent
delta. Diagnostic-only, and cheaper to live with than to make the rollback
carry a third field — but it means `sessions.compactions` is a lower-bound
sighting count, not an exact event count.

## Known limits

* **Two in-flight jobs per session would blur the durable cursor.** It is a
  single high-water mark, so if job B (5000..9000) finishes before job A
  (0..5000) fails, `confirmed_offset` is already at 9000 and A's failure is
  invisible. §Binding decisions #8 allows exactly one in-flight job per
  session, which is what makes the single mark sufficient; the existing
  `// ponytail: one in-flight job per session` note is the upgrade path.
* **Concurrent writers are pinned at the store layer, not through the
  router.** `concurrent_writers_on_a_file_database_keep_the_high_water_marks`
  runs two threads at one row over a real `tempfile` database — the
  configuration production runs. It is deliberately not an integration test:
  `Db::open_memory()` is shared-cache, where a second connection writing the
  same table gets `SQLITE_LOCKED`, which `busy_timeout` does **not** retry,
  unlike the `SQLITE_BUSY` a file database returns under WAL. So the overlap
  is fine in production and a coin flip in that harness. `retain_api`'s known
  `retain_jobs` lock flake is the same mechanism.
* **Legacy session state is not migrated, and MG-1 will not migrate it.**
  Legacy tracks retention position as a *message index* in
  `retention_tracking.json`; `sessions` tracks a *byte offset*. There is no
  function from one to the other without re-parsing every historical
  transcript, and several no longer exist. Every session starts at offset 0
  after cutover, which means one initial retain per session — bounded by
  `retain.max_initial_messages`, which is exactly the cap the
  102MB-transcript incident produced. The plan's trade-off table records
  this; it is repeated here because Phase F reads the design notes.

## Manual verification

`memgardend` on 127.0.0.1:9100 (release build, embeddings off, real Ollama at
:11434), `/healthz` reporting `schema_version: 7`. Fresh database.

```
POST /v1/banks                              -> claude-code::C1
POST /v1/banks/claude-code%3A%3AC1/sessions -> source=startup, byte_offset=0

POST .../retain  byte_offset=65536 turn=20 chunk=2 compactions=1   -> 202
  job running   byte=65536 confirmed=0     inflight=65536
                turns=20 chunk_index=2 compactions=1 retains=1

  # HIGH 1: a `skipped` at a HIGHER offset while that job is unresolved
POST .../retain  byte_offset=99999, messages=[{role:"system",…}]    -> 200 skipped
                byte=99999 confirmed=0     inflight=99999   <- gap NOT swallowed

  job done      byte=99999 confirmed=65536 inflight=34463
                                          ^ the skipped range, still outstanding
                                            (over-reporting, the safe direction)

POST .../retain  byte_offset=120000                                 -> 202
  job done      byte=120000 confirmed=120000 inflight=0   <- residual self-heals

  # HIGH 2: a 201-byte session_id
POST .../retain  session_id=<201 bytes>                             -> 400
                pending/running retain_jobs = 0
                sessions rows               = 1   (no row created)

reconciliation, read straight from the file DB:
  sessions.retains = 3   retain_jobs rows = 2   delta = the 1 skipped accept
```

Bank ids are percent-encoded in the path (`::` → `%3A%3A`), which is the shape
the hook will use (plan §Binding decisions #4). Both live shapes —
`claude-code::bank-b` and `claude-code::bank e`,
`::` plus a space — are covered by
`a_real_world_bank_id_survives_the_url_path`.

## Mutation evidence

Six mutations, each reverted after the run. The convention this repo keeps
getting wrong is a correct rule that no test pins, so the rules added here
were checked by deleting them:

| mutation | caught by |
|---|---|
| drop the `CASE WHEN confirmed_offset >= byte_offset` guard | `a_later_settled_accept_does_not_swallow_an_earlier_gap`, and `a_later_skipped_does_not_swallow_an_unresolved_jobs_gap` through the router |
| `last_seen_at = excluded.last_seen_at` → `last_seen_at = last_seen_at` | `insert_then_update_roundtrip` |
| `messages_sent + excluded` → `max(…)` | `the_daemon_side_counters_accumulate_rather_than_replace`, `concurrent_writers_on_a_file_database_keep_the_high_water_marks` |
| collapse the two cursors in **both** the INSERT and the DO UPDATE branch | 3 tests (mutating only DO UPDATE is not a collapse — a session's first write is always the insert) |
| remove the early `session_id` guard from `retain_inner` | `an_oversized_session_id_is_rejected_before_any_row_is_written` |
