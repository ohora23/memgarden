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

`sessions.retains` is a *count*; the detail behind each one stays in
`retain_jobs`, joined on `session_id`. `sessions.messages_sent` is the
session-level **sum** of a value `retain_jobs.detail.message_count` records
per request — an aggregate at a different grain, not a duplicated column.
The manual run confirms both identities hold:

```
sessions.retains = 2          retain_jobs rows = 2
sessions.messages_sent = 26   sum(retain_jobs.detail.message_count) = 26
```

Within `sessions`, every field has one writer class:

| field | writer |
|---|---|
| `cwd`, `transcript_path`, `source`, `end_reason`, `ended_at`, `turns`, `chunk_index`, `byte_offset`, `compactions` | the hook — `POST …/sessions` or the retain request |
| `retains`, `messages_sent` | the daemon, `+=` once per accepted retain |
| `confirmed_offset` | the daemon, only from a completion fact |

**`confirmed_offset` is not a request field anywhere.** Its only meaning is
"ingestion is settled for these bytes", and that is a fact the daemon
establishes, never a claim a client makes. A hook that could set it could mark
unwritten bytes durable and lose them silently — the exact failure the split
exists to prevent. Pinned by `a_client_cannot_set_the_durable_cursor`.

It advances in exactly two places, both of them completion facts:

1. `retain/mod.rs`, inside the existing `if clean { … }` block — the same
   condition, and the same `spawn_blocking`, as the content hash. The two
   writes say the same thing to two audiences.
2. `routes/retain.rs`, for the `skipped` and `duplicate` outcomes. Neither
   queues work: `skipped` means the delta emptied out under role filtering,
   `duplicate` means an earlier clean job already stamped the hash. Leaving
   the durable cursor behind for these would open a gap that nothing can ever
   close, and it would read as permanently lost work.

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

## Known limits, for the PRs that build on this

* **`chunk_index` has no writer on the retain path.** C4b's retain payload
  (plan §PR C4b step 7) carries `byte_offset`, `turn` and `compaction` but no
  `chunk`, so `chunk_index` only advances through `POST …/sessions`. A hook
  recovering from a wiped state dir therefore restores its `chunk` from the
  last value any `POST …/sessions` mirrored. C4b should either mirror `chunk`
  on the session upsert it already makes at `session-end`, or add a `chunk`
  field to the retain request. Flagged rather than fixed here, because the
  wire contract for retain is C4b's to widen.
* **`compaction` on the retain request is the hook's cumulative total, not
  this delta's count.** The plan names the field in the singular and C4a's
  `Delta` exposes a per-delta `compactions`; those are different numbers.
  Cumulative is the one that survives a rollback-and-resend without
  double-counting, so that is what the `MAX` merge expects. C4b must send
  the state file's running total.
* **Concurrent writers to one session row are not covered by an integration
  test.** `Db::open_memory()` is a shared-cache database, where two
  connections writing the same table get `SQLITE_LOCKED` — which
  `busy_timeout` does not retry, unlike the `SQLITE_BUSY` a file database
  returns under WAL. The overlap is fine in production and a coin flip in
  that harness, so the merge semantics are pinned by sequential store-level
  tests instead. Pre-existing harness property; `retain_api`'s known
  `retain_jobs` lock flake is the same mechanism.

## Manual verification

`memgardend` on 127.0.0.1:9100 (release build, embeddings off, real Ollama at
:11434), `/healthz` reporting `schema_version: 7`.

```
POST /v1/banks                                   -> claude-code::C1
POST /v1/banks/claude-code%3A%3AC1/sessions      -> source=startup, byte_offset=0
POST .../retain  byte_offset=8192 turn=10 compaction=1
                                                 -> 202 accepted
     mid-job   byte_offset=65536 confirmed_offset=8192  inflight=57344
     job done  byte_offset=65536 confirmed_offset=65536 inflight=0
POST .../sessions {source: resume, end_reason: logout, ended_at: …}
                                                 -> source stays "startup", end_reason "logout"
GET  .../sessions?active=true                    -> []
GET  .../sessions?limit=5                        -> ["sess-c1"]
```

Bank ids are percent-encoded in the path (`::` → `%3A%3A`), which is the shape
the hook will use (plan §Binding decisions #4).
