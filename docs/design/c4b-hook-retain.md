# C4b / HK-1f — `hook retain`, cursor reconciliation, and a detached `session-end`

`crates/memgarden-cli/src/cmd/retain.rs` (the cursor state machine),
`src/cmd/session_end.rs` (the detached spawn), the post `catchup.rs` deferred
here, `state::SessionState::cwd`, and the three throttle helpers moved into
`cmd/mod.rs`. Sixth of seven Phase C PRs, and the one the plan calls the one
that needs undistracted review: it consumes C3's identity token and C4a's
delta reader, and it is the only hook that can lose memory.

## What one `Stop` does, in order

1. **Load state under `state::with_lock`.** Advisory, MemGarden-against-
   MemGarden only — which is exactly the race that exists, because `async:
   true` on the `Stop` entry means the previous `Stop` may still be running.
2. **`turns += 1`, `turns_since_retain += 1`**, from the payload path only.
3. **Throttles**: the circuit breaker, then the poison window. Before any
   socket.
4. **Reconcile `pending`**, if there is one: `GET /v1/retain/{job_id}`.
   Because this is *before* the gate and `pending` is set exactly for the
   window between an accept and the next invocation, **the first gated `Stop`
   after every accepted retain pays one loopback GET** — so the zero-network
   gated turn is 8 of every 10, not 9. Immaterial to the budget (~0.2 ms) and
   material to the table below, which is Phase F's evidence.
5. **The turn gate.** 9 of every 10 `Stop`s end here; 8 of them open no socket.
6. **`stat` the transcript**, which is also the guard that it *is* a
   transcript. `size < offset` resets to 0; `size <= offset` returns.
7. **`read_delta`** (C4a). An empty delta advances the cursor anyway and
   restarts the cadence.
8. **`POST /v1/banks/{bank}/retain`**, then the accept table.
9. **Store**, whatever happened — every early exit above still has a counter
   or a cursor worth persisting.

## The one property this file exists to guarantee

**A cursor never advances past bytes nothing ingested.**

`recall` fails open and the turn proceeds with no memories. `session-start`
fails and the next session's catch-up covers it. Here the transcript is the
only spool (§Binding decisions #9), so a cursor that advanced wrongly has
thrown the bytes away.

`POST …/retain` answers **202 = queued, not ingested**. The worker can still
fail a chunk (`retain/mod.rs:317-337`), mark the job `Failed` (`:341-347`),
and — deliberately — *withhold the content hash* so a re-POST starts a fresh
job rather than being dismissed as a duplicate (`:349-353`). That recovery
path is designed, and without the protocol in this file it is **unreachable**:
a hook that commits on the 202 never sends those bytes again, and the daemon's
arrangement to accept them has no caller.

| response | cursor | `pending` | counters |
|---|---|---|---|
| `202` + `job_id` | advance | recorded | all cleared |
| `202`, no `job_id` | **unchanged** | — | none move |
| `200 {"status":"duplicate"}` | advance | cleared | all cleared |
| `200 {"status":"skipped"}` | advance | cleared | all cleared |
| `200`, unreadable | unchanged | — | `transport_failures += 1` |
| `429` | unchanged | — | `transport_failures += 1` |
| `503` | unchanged | — | **none move** |
| `404` | create the bank, retry **once** | — | durable on the second |
| other `4xx` | unchanged | — | `reject_failures += 1` |
| other `5xx` | unchanged | — | `transport_failures += 1` |
| connect/timeout/protocol | unchanged | — | `transport_failures += 1` |
| bad `daemon_url` | unchanged | — | none move |

And the reconcile, on the next invocation:

| `GET /v1/retain/{job_id}` | action |
|---|---|
| `200 done` | clear `pending`, proceed |
| `200 failed` | `offset = offset_from`, `chunk = chunk_before`, clear, proceed |
| `200 pending` / `running` / unknown | leave it, **end the invocation** |
| `404` | treat as `failed` — see §Diverged from the plan 5 |
| unreachable | leave it, end the invocation, `transport_failures += 1` |

`chunk_before` is captured **before** the increment. It has to be: the re-send
must reuse the same `document_id`, or the failed delta's provenance row is
orphaned and the next delta's `message_count`/`files_modified` overwrite a
chunk that never landed.

`compactions` is deliberately **not** rolled back. It is a cumulative sighting
count merged `MAX` server-side, so a rollback-and-resend re-counts the
boundaries in the re-sent delta and the column is a lower bound by
construction. A third field in `Pending` would buy exactness in a number
nothing reads for control (§Binding decisions #6).

## The lock is held across the network, and `recall` must not wait for it

C4b is the first thing in this crate that holds `<sid>.lock` across a socket:
the reconcile `GET` (400 ms), the retain `POST` (5,000 ms), and on a 404 a
bank-create plus a second `POST` — **up to ~10.8 s** of configured budget.
`recall::record` derives the **same** lock path and runs on every
`UserPromptSubmit`.

Three things make that worse than a slow path:

* **The breaker is checked *inside* the lock.** Everything C3 built to make a
  hung daemon cheap is bypassed by a caller stuck on acquisition.
* **`hook_bench` structurally cannot see it.** It measures one hook in
  isolation, so the gated-turn 0.373 ms is real *and* blind to this.
* It grows precisely in the Ollama-contention scenario §Open questions 6
  predicts — and C4b is the gate on starting that shadow run.

So `recall::record` uses **`state::with_try_lock`**: one `try_lock`, and if
another MemGarden process holds it, `f` runs unlocked.

| recall, behind a retain holding the lock for 2 s | run 1 | run 2 | run 3 |
|---|---|---|---|
| `with_lock` (blocking) | 1.752 s | 1.753 s | 1.752 s |
| **`with_try_lock`** | **0.002 s** | **0.001 s** | **0.002 s** |

**Skipping the wait is not a new degradation.** `with_lock` already runs `f`
unlocked when `lock()` errors, and `record` already re-reads state inside the
lock precisely because the value may have moved. The cost of losing the race is
one recall's counter update — the same thing the existing fallback risks —
against an unbounded interactive stall on the event where a stall is most
expensive.

**Retain keeps the blocking lock.** Dropping it around the POST would
reintroduce the double-post race the lock exists for.

```
// ponytail: no retry. One `try_lock`, then proceed. A short bounded spin if
// a counter update ever turns out to matter more than a prompt.
```

## One in-flight job per session, and why that is a correctness bound

`sessions.confirmed_offset` is a single high-water mark. Two in-flight jobs —
A covering 0..5000 and B covering 5000..9000 — with B finishing first would
confirm straight over A's gap, and A's failure would then be **invisible**:
the dashboard's `byte_offset - confirmed_offset` would read 0 with 5,000 bytes
missing. The bound is what makes the one number mean what it says.

```
// ponytail: one in-flight job per session. A queue — and a
// `confirmed_offset` that is a set of intervals rather than a mark — if
// retain ever fires per-turn instead of every `retain_every_n_turns`.
```

## Three callers, one state machine

`retain::advance` is everything from the throttle check to the accept table,
called under `state::with_lock` by all three:

| caller | `force` | `turns` | transcript path |
|---|---|---|---|
| `hook retain` (`Stop`) | no — the gate decides | `+1` | the **payload's** |
| `hook retain --force` (the `session-end` child) | yes | unchanged | the **stored** one |
| `hook catchup` (C2b's child) | yes | unchanged | the **stored** one |

`force` bypasses the turn gate and nothing else. It does not bypass the
breaker or the poison window: a `SessionEnd` against a daemon that has durably
rejected this session ten times is not the moment to try an eleventh inside
the hour.

`turns` counts `Stop` invocations, so neither detached caller increments it —
`SessionEnd` is not a `Stop`, and catch-up is not even the session's process.

### The catch-up marker, honoured

C2b left the post to this PR with a specific instruction: **re-load under the
lock and re-check `offset < file_size` before posting**, because `Candidate` is
a *selection snapshot* — `state` came from `load_all` outside any lock and
`file_size` is a `stat` from the same moment. C2b's first draft also claimed
the current-session filter was "the one race the lock cannot arbitrate"; that
claim was **retracted in review**, and correctly: a session live in another
Claude Code window passes every filter in `select`, and its own `Stop` hook is
on the same cursor. `advance` does both halves inside the lock — it re-`stat`s
the transcript and returns on `size <= offset` — so the worst case is a
redundant post the daemon answers `duplicate`.

`--dry-run` returns before the loop. It is the only window into a process whose
three streams are `/dev/null`, so it has to stay observation-only.

## `transcript_path` is validated at the read, by property

C2b decided the *where* deliberately: `retain` reads the **payload's** path on
every `Stop` and never consults the state file for it, so a store-time guard
would cover the once-per-session path and leave the once-per-ten-turns path
wide open. One guard, in the reader, where every caller passes.

The *what* is a property, not a spelling. A `.jsonl` allowlist constrains a
vocabulary Claude Code owns and would break on the next rename; "is this a
regular file" refuses a directory, a device, a socket and a fifo, which is the
class that matters.

The *how* is `std::fs::metadata`, and it diverges from the brief's "open it,
`fstat` the handle" for a reason that is not stylistic: **opening a fifo blocks
until a writer appears.** The file type has to be settled *before* anything
opens it, and `O_NONBLOCK` needs `libc`, which this crate's CI-enforced
dependency closure refuses. `advance` needs the size anyway for the
`size < offset` reset, so this is one syscall doing both jobs.

```
// ponytail: the residual is a swap between this `stat` and `read_delta`'s
// `open`, which needs write access to the transcript's own directory — at
// which point the attacker can simply write the transcript. `O_NOFOLLOW |
// O_NONBLOCK` on the open plus an `fstat` on the handle is the airtight
// version, and it costs `libc`.
```

## `session-end` detaches, and the reason is measured

The earlier draft did an inline retain with a 2 s client timeout. `prepare()`
tokenizes the transcript **twice** with `cl100k_base`, measured at
**19.18 MB/s** on this machine, and the uncapped pass is not bounded by
`retain.max_initial_messages`. At `max_post_bytes = 24 MB` that is ≈2.1 s of
tokenization alone — over any ceiling that fits `SessionEnd`'s documented 1.5 s
shared budget, on exactly the path the oversize fallback exists for. And the
failure mode is the bad one: **the daemon queues the job while the hook records
a timeout and does not advance.**

Detaching removes the deadline instead of tuning it, takes `SessionEnd` off the
shared budget instead of raising it, and reuses C2b's `spawn_detached`
(`Stdio::null()` ×3 + `process_group(0)`). Measured at **0.353 ms** arm A.

The child never stops the daemon (§Binding decisions #10). It also posts the
`end_reason`/`ended_at` session update, **after** the retain and outside the
lock — the retain is the part that can lose something; the update is a label
on a row.

### The argv, and the trap it is shaped around

The child is `hook retain --force --session <sid> --end-reason <reason>`, with
**fixed slots**: `args[2]` must be exactly `--force`, `args[3]` exactly
`--session`, and `args[4]`/`args[6]` are values whatever they look like.

C2b's review demonstrated the alternative against the real binary: scanning
argv for `--dry-run` let a *session id* of `--dry-run` empty the exclusion set
— a filter turned off by its own subject. Here both untrusted values arrive on
stdin and would be sitting next to a `--force` that decides whether the turn
gate applies. `a_flag_is_only_a_flag_in_its_own_slot` pins it from both
directions: a session id of `--force` reads as a *value*, and a `--force` in
the wrong slot is absent rather than found elsewhere.

`reason` is bounded to the daemon's own `MAX_REASON_BYTES` (64) **before** it
becomes an argv element, on a char boundary. The daemon's check is one layer
too far out: an 8 MB argv element is an `E2BIG` on the `execve`, which is a
lost final retain rather than a 400.

## Diverged from **the plan**

### 1. The breaker and the poison window are checked **before** the reconcile

Plan §C4b orders it reconcile (2) → gate (3) → breaker (4). That makes the
breaker unreachable from the path that most needs it: a *hung* daemon answers
the reconcile `GET` with a full timeout on every `Stop`, and a breaker checked
afterwards never gets to skip a socket it has already opened. The breaker's
entire measured value (C3: 1536 ms per prompt without one) is that it is
checked *before* the connect. Pinned by
`the_breaker_skips_the_socket_inside_its_window_and_not_beyond_it`, which
asserts on the stub's **accept** count, not its request count.

### 2. The POST body: `chunk` is required and `compaction` is spelled `compactions`

The plan's body list is
`{messages, session_id, cwd, is_initial, document_id, event_date, byte_offset,
turn, compaction, metadata}`. The daemon's `RetainRequest` has no `compaction`
field — C1 renamed it to `compactions` at zero callers — and it has a `chunk`
field the plan's list omits entirely. serde ignores unknown fields, so both
mistakes are **silent**: the compaction count would never reach the mirror,
and `chunk` is what C1 added specifically so a state-dir wipe *mid-session*
cannot make the hook reuse chunk 0's `document_id` and overwrite its metadata.

### 3. Catch-up posts `is_initial = offset == 0`, not a hardcoded `false`

Plan §C2b says catch-up "posts each delta with `is_initial = false`".
Catch-up at offset 0 **is** a session's first retain — a whole transcript, the
largest payload in the system — so `false` there takes the daemon's *uncapped*
branch on exactly the shape `retain.max_initial_messages` exists to bound. It
is the 102 MB-transcript incident's shape, reached through the recovery door.
`advance` derives it from the cursor, once, for all three callers.

### 4. The `session-end` child needs a `--session` argument the plan does not give it

Plan §C4b spells the child `memgarden hook retain --force --end-reason
<reason>`, which names no session. The child is spawned with `Stdio::null()` on
all three streams, so it has no payload — a `SessionEnd` child that cannot
identify its session cannot retain it. Same shape of gap as C2b's:
§Binding decisions #5's state shape omits `transcript_path` for the same reason.

### 5. The reconcile has no `404` arm, and without one a lost job row wedges a session forever

`GET /v1/retain/{job_id}` 404s an unknown id. Reading that as "not settled yet"
means `pending` is never cleared and **every subsequent turn returns at step 4
for the rest of the session's life** — over a row a database wipe took.
`failed` is the safe reading: re-sending is at-least-once and the content-hash
dedup answers `duplicate` if the job did in fact finish. Pinned by
`a_missing_job_row_rolls_back_rather_than_wedging_the_session`.

### 6. §Failure posture contradicts itself on `503`, and this PR takes the prose

The prose says a `503` "increments **neither** counter: it is a correct answer,
it is fast, and a 9 s model load must not blind us for 60 s". The retain row of
the table two paragraphs later says it is "treated as a transport failure;
`offset` unchanged". Implemented as the prose — no counter moves, cursor
unchanged — because the prose gives a reason that applies verbatim to retain
and the table cell's clause is about the *cursor*, which both readings agree
on. `a_503_moves_no_counter_at_all`. **Flagged for review: if the table's
reading was meant, one line in `classify` changes it.**

### 7. An empty delta restarts the cadence

Step 6 of the plan says an empty delta advances `offset` to `consumed_to` and
exits, and says nothing about `turns_since_retain`. Leaving it high makes
**every** subsequent `Stop` pass the gate and re-read the tail — the 0.30 ms
gated path becoming a 0.32 ms delta read on every turn, for a session that has
nothing to say. Reset, so "one delta read per `retain_every_n_turns` `Stop`s"
stays true.

### 8. A `202` without a `job_id` does not advance

Not in the plan's accept table, which assumes 202 always carries one (the real
daemon's does). Advancing on it would be silent loss the moment that job
failed, with no id to discover that it did. Not advancing is at-least-once and
**self-terminating**: the job either finishes and stamps the content hash, so
the next attempt is answered `duplicate` and advances, or it fails and the next
attempt is the re-send we wanted.

### 9. A rolled-back delta waits for the gate rather than re-POSTing immediately

The plan says `failed` "rolls back and proceeds", and step 3 (the gate) is what
it proceeds to. Kept literally. Re-POSTing immediately would, under the
chunk-failure storm §Open questions 6 predicts for a shadow run, put an
expensive `prepare()` on **every** `Stop`. The gate re-sends within
`retain_every_n_turns` and `session-end` forces it regardless; neither can lose
the bytes, because the transcript is still the spool.

### 10. `SessionState` gains a `cwd`

Not in §Binding decisions #5's state shape, for `transcript_path`'s reason: the
plan lists what the *live* hook needs, and both detached children need more.
The retain POST carries `cwd` so the daemon can relativize its `file:` tags; a
child posting `null` produces absolute tags for the same files the live hook
tagged relatively — one session, two spellings of one path.

### 11. Retain must consult the mirror on a state-file miss, mid-session

§Failure posture says a missing state file is "offset 0, `is_initial = true`,
let the backfill cap bound it", and adds that the wiped-state-dir case is
covered because `session-start` prefers the mirror. That is true only of a wipe
**between** sessions. `session-start` does not fire mid-session and `retain` is
the hook that does — so a mid-session wipe re-ingested the whole transcript
under **chunk 0's bare `document_id`**, and `routes/retain.rs` rebuilds
`documents.metadata` from scratch, overwriting the real chunk 0's
`message_count` and `files_modified`.

The daemon's own `RetainRequest::chunk` doc names this exact scenario as the
reason C1 added the column. Without this call, **C1 built a column for a
recovery nothing performed.** `cmd::recover` now does the
`GET …/sessions/{sid}`, sharing C2b's `Mirror` struct — the one that cannot
deserialize `byte_offset` at all.

### 12. The manual verification's "converging cursors" criterion is not always reachable

Plan §C4b asks the manual verification to show `byte_offset` and
`confirmed_offset` **converging**. They converge only when every chunk of every
job extracts cleanly. A single failed chunk leaves a gap that nothing closes —
see §Known limits, which is where the evidence for this lives. The criterion
should read "converging, or a gap attributable to a named `chunks_failed`".

### 13. `hook_bench`'s stub had to learn routes

Up to C3 every arm hit `POST …/recall`, so one reply was one reply. `hook
retain` routes on `status` and `job_id`, and a recall body served to a retain
POST is a 200 it cannot classify — the **transport-failure** branch. Arm A
would have measured the failure path under a heading that said steady-state
retain. The stub now answers per route and drains the whole request body (a
retain body is ~100 KB; a stub that replies and closes mid-write leaves the
hook's `write_all` on a reset socket, which is the same failure branch from the
other side).

## Diverged from legacy

- **Optimistic cursor plus job reconciliation.** Legacy commits on the HTTP
  response (`retain.py:262-263`), which is correct for legacy: its server
  ingests synchronously. Ours returns 202 and can still fail per chunk.
- **`document_id` suffix kept, rationale replaced.** `retain.py:110-116` says
  reusing a `document_id` "replaces the server-side document". True of legacy's
  server. Ours UPDATEs and keeps the row id (`documents.rs:55-70`) and
  `routes/retain.rs:395-409` rebuilds `documents.metadata` from scratch each
  time, so without the suffix each delta's `message_count`/`files_modified`
  **overwrite the previous delta's**. The suffix protects per-delta provenance,
  not facts.
- **`session-end` detaches instead of retaining inline.** Legacy's
  `session_end.py` retains inline and then calls `stop_daemon`
  (`lib/daemon.py`). We do neither.
- **The hook never spools a payload** (§Binding decisions #9). Legacy has no
  spool either, but it also has no rollback, so a failure there is simply lost.
- **Poisoning is a slow-retry state.** Legacy has no equivalent; the earlier
  draft of this plan had a latch, which turned a transient class of failure
  into permanent loss of a session's remaining content.
- **Never exit 2.** `retain.py:283-287` exits 2 under `debug`, and on `Stop`
  that prevents the turn from ending.

## Known limits and accepted risks

### A `done` job with a failed chunk leaves the two cursors apart **forever**

This is the finding the manual verification produced, and it is the one to read
first, because it contradicts the criterion the plan sets for that very
verification ("`byte_offset` and `confirmed_offset` **converging**").

The daemon fails a job only when *nothing* was written:

```rust
let all_failed = progress.facts_written == 0 && progress.chunks_failed > 0;
let clean = aborted.is_none() && !all_failed && progress.chunks_failed == 0;
```

`status` follows `all_failed`; the content hash and `confirmed_offset` follow
`clean`. Those are **different conditions**. A job with 3 of 4 chunks extracted
is `done` — so this hook clears `pending` and moves on — while the daemon has
deliberately declined to settle it. Nothing will ever close that gap: the
`pending` record that could have re-sent is gone, and `confirmed_offset` only
ever advances from a settlement.

It is not hypothetical and it is not rare. It happened **unprompted** on the
first round of the manual verification below: one chunk's Ollama reply
truncated mid-JSON, four retries all truncated at the same place, and the job
finished `done chunks 3/4 failed=1 facts=34` with `confirmed_offset` stuck at
0 and `inflight_bytes` at 1,008,399.

**The plan's reading is kept**, and the alternative was considered rather than
overlooked. Treating `chunks_failed > 0` as `failed` would:

* duplicate the facts the successful chunks *did* write, on every retry; and
* wedge the session at that offset **permanently** when the chunk fails
  deterministically — which is exactly what was observed, since the model
  truncated at the same token on all four attempts. Losing one chunk beats
  losing every subsequent delta.

What changed is that the hook **counts** it. `SessionState::unconfirmed_bytes`
accumulates `offset_to - offset_from` in that arm, and C5's `hooks status` —
which already reads state files, so no endpoint and no schema — surfaces it.
A stderr line under `[hooks] debug` names the byte range as well.

The counter, not just the line, is the load-bearing half: **`[hooks] debug`
defaults to `false`**, so a log-only mitigation records nothing in the default
install — and the re-entry criterion below is then unmeasurable, because the
daemon-side `inflight_bytes` is only a lower bound (next section).

**Re-entry criterion.** A shadow run where `unconfirmed_bytes` grows
monotonically — i.e. partial chunk failures are common rather than occasional —
makes "one lost chunk" the wrong trade. The fix then is not in this hook: it is
a per-chunk byte range in `retain_jobs`, so a re-send can carry the failed
chunk alone. That is a C1-shaped change, not a C4b one.

### A later clean job confirms straight over an earlier job's gap

> **CLOSED by `fix/retain-cursor-gap` (HK-1g), 2026-08-05**, in exactly the
> four pieces this section specifies. The first half — a `done` job with a
> failed chunk leaving a gap — is unchanged and still open; what is fixed is
> that a later clean job can no longer erase the evidence of it. See
> `docs/design/hk-1g-retain-cursor-gap.md`. The paragraph below is kept as
> written because the *wrong* fix it names is the part worth not
> rediscovering.

Read from the source rather than observed, because both jobs in the run above
failed a chunk. The worker's clean-run block writes
`confirmed_offset = task.byte_offset` unconditionally, and `store::sessions`
merges it with `MAX`. So a clean job covering 1008399..2163159 sets the durable
cursor to 2,163,159 **even if an earlier job left 0..1008399 unsettled**.

This is the same *shape* as the defect C1's review already fixed on the other
path — `confirm_if_settled` exists precisely because "a duplicate at offset
1500 says nothing about bytes 900..1500 that some other job was supposed to
carry" — but **it is not the same fix, and reusing that channel here would
break every session.**

`confirm_if_settled`'s guard is `confirmed_offset >= byte_offset` on the
pre-update row, and the *request* path has already written
`byte_offset = this job's end` before the worker runs. So an ordinary clean job
reads `0 >= 5000` → false and **never confirms**: `confirmed_offset` freezes at
0 forever. The in-tree test `sessions.rs:606-617` pins exactly this — it needs
the unconditional channel to reach `confirmed_offset = 5000` on a row whose
`byte_offset` is 6000.

**The correct shape needs the job's *start*, which no persisted row currently
has:**

1. `offset_from` on `RetainRequest` — the hook already holds it as `from`;
2. persisted on the `retain_jobs` row, so the worker can read it back;
3. a **third** `SessionUpdate` channel guarded on
   `confirmed_offset >= ?offset_from`;
4. one line in this hook to send it.

That is a migration + route + store + hook change, **not a store-only edit**.
Said explicitly because a future PR will follow this paragraph literally.

Consequence meanwhile: `inflight_bytes` is a **lower bound** on unsettled
bytes, not a measurement — which is why `unconfirmed_bytes` above exists
hook-side.

**How the two daemon defects relate.** They are one defect in two places.
`chunks_failed` opens the gap; the unconditional confirm erases the evidence of
it. Under this hook's protocol — one in-flight job per session — a `done` job
with `chunks_failed > 0` is the **only** producer of a gap at all, so this
section fires only downstream of the previous one. Both stay out of C4b. The
daemon PR lands after C5, during the shadow run, and **must precede any Phase F
claim built on `inflight_bytes`.**

### The rollback window is one job wide

A session with a `pending` job whose daemon then dies mid-run keeps that
`pending` until the daemon restarts and `retain_jobs::fail_stale` marks it
`failed` — which is what makes the rollback reachable. Between those two
moments the session retains nothing. Bounded by the daemon's own restart, not
by anything here, and visible as a growing `inflight_bytes` in `hooks status`
(C5).

### `retain_cap_saving` under-reports on a truncated delta

When C4a's oversize fallback drops leading messages the daemon never sees those
bytes, so the ledger's ratio is computed against a payload that is already
smaller than the transcript. `metadata.truncated` is posted so a reader can
qualify the row; nothing recomputes it.

### A rewrite to an equal or greater length is undetected

`size < offset` catches the shrinking rewrite. A transcript replaced with one
of equal or greater length leaves `size >= offset` while the offset points
mid-content, and the delta from there is garbage the reader skips as corrupt
lines. Verified append-only on Claude Code 2.1.220; the guard costs one
comparison and is the tripwire if that changes.

### The forced child inherits `[hooks] enabled` from a second config read

`session-end` reads the config to decide whether to spawn, and the child reads
it again. A config edited between the two makes the child act on the newer one.
Harmless — both answers are correct for the moment they were read — but it is
why `the_config_switch_makes_no_request_and_writes_no_state` waits before
asserting.

### `hookio::HookInput` has twelve non-`Option` `#[serde(default)]` fields

Named rather than fixed. `#[serde(default)]` covers an **absent** key; an
explicit `null` against a non-`Option` is a type error that fails the whole
struct — which is precisely the defect found in `RetainReply` below, on a
schema **Anthropic** controls rather than one we do. A future Claude Code build
emitting `"cwd": null` on one event would fail this parse and **every hook
would silently no-op** for anyone on that version.

`hookio`'s module doc now states the rule ("a field whose JSON is produced by
Claude Code or by `memgardend` is `Option<T>`, not `#[serde(default)] T`") and
`Mirror`'s points at it. Converting the twelve fields touches every
subcommand's field access, so it is a follow-up rather than a C4b change; the
durable part — the rule, written where the next person edits these structs —
is here.

### Smaller ones

- The reconcile `GET` uses the **interactive** budget (400 ms), not the retain
  budget (5 s). A single-row read on a gated turn must not cost a `Stop` five
  seconds; the breaker covers the repeat.
- `now_ms` is threaded from `run` into `reconcile` rather than re-read. One
  invocation must not evaluate its breaker window against one clock and stamp
  it with another; under lock contention the two readings can be seconds apart.
- `advance` is `pub` so `catchup` can call it. There is no narrower visibility
  that spans two sibling modules without a third.
- A poisoned session's `session-end` retain is skipped inside the hour, which
  is the only place `force` losing to a throttle can cost a whole session's
  tail. `hooks status --clear-poison` (C5) is the exit.

## AC mapping

**AC-2** and **AC-7**: discharged; the tables below are the evidence.

**AC-3 ("nothing is dropped between hook and store"): _not_ discharged.** The
rollback evidence proves the *recovery path* works, but the same run lost two
chunks' facts permanently and left `confirmed_offset` at 0 across all three
rounds. The plan lists AC-3 against C4b; on this evidence it is gated on the
per-chunk byte-range change in §Known limits, not on this PR. Recorded here
because Phase F reads this line.

## Measurement

Interleaved-paired, one driver process, arm B = `memgarden hook noop`. Absolute
cross-run comparison is invalid on this box (+1.5 ms measured on identical
bits), so every number below is `A_i - B_i` from the same run.

| arm | p50 ms | p95 ms | p99 ms | min ms | N |
|---|---|---|---|---|---|
| **A `hook retain`, gated turn** (9 of every 10 `Stop`s) | 0.373 | 0.424 | 0.457 | 0.347 | 300 |
| B `hook noop` | 0.274 | 0.327 | 0.376 | 0.245 | 300 |
| **paired A−B** | **0.103** | **0.149** | 0.170 | −0.085 | 300 |
| **A `hook retain`, steady-state delta** (200 KB) | 0.925 | 1.019 | 1.069 | 0.875 | 300 |
| B `hook noop` | 0.285 | 0.331 | 0.341 | 0.250 | 300 |
| **paired A−B** | **0.644** | **0.729** | 0.774 | 0.528 | 300 |
| **A `hook session-end`** | 0.353 | 0.406 | 0.416 | 0.328 | 300 |
| B `hook noop` | 0.274 | 0.318 | 0.341 | 0.251 | 300 |
| **paired A−B** | **0.078** | **0.129** | 0.149 | −0.018 | 300 |

The gated turn is the number that matters, because it is 9 of every 10 `Stop`s.
Arm A **0.373 ms** absolute against the plan's predicted 0.30 ms; the paired
delta — the hook's own work, with the binary's fixed cost removed — is
**0.103 ms**.

`session-end` at **0.353 ms** absolute matches the plan's predicted ~0.4 ms,
and it is flat by construction: the child's cost is the child's.

### The declared exception: the initial retain

| transcript | arm A p50 | p95 | paired A−B p50 | N |
|---|---|---|---|---|
| **21,880,741 B live transcript**, offset 0 | **68.581 ms** | 70.873 | **67.993** | 15 |

C4a measured the *read* half of that at 32.86 ms for 21.6 MB; the rest is
serializing the ~10 MB body and writing it to the socket. It is over the 10 ms
AC-2 budget and it is the exception the plan declares, made invisible by
`async: true` on the `Stop` entry (C5's table). It happens **once per session**.

### Arm B did not move, and the control says so rather than an inference

C2b's inference about arm B was right in mechanism and **wrong by 5×** until it
ran the paired-binary control, so this PR ran it: `--bin-b` against master's
binary (`cf7245d`), both arms `hook noop`, three runs of N=200.

| run | A (C4b binary) | B (master binary) | paired A−B |
|---|---|---|---|
| 1 | 0.263 | 0.259 | **+0.004** |
| 2 | 0.265 | 0.258 | **+0.007** |
| 3 | 0.258 | 0.252 | **+0.007** |

Three of three positive and tightly clustered, so this reads as a **real
≈6 µs — 0.4 % of arm B, 0.06 % of the 1.5 ms budget** — rather than as noise.
+82,840 bytes of binary for 6 µs. The reason is C3's lesson restated: the
binary grew, the **relocation count did not** — 221 before and 221 after.

### The lock contention number, which `hook_bench` cannot produce

`hook_bench` runs one hook at a time, so this is measured by racing two real
processes: a retain against a stub whose retain `POST` sleeps 2 s, and a
`hook recall` on the same session starting 250 ms later.

| recall wall clock, behind a retain holding the lock | run 1 | run 2 | run 3 |
|---|---|---|---|
| `with_lock` (the build before this fix) | 1.752 s | 1.753 s | 1.752 s |
| **`with_try_lock`** | **0.002 s** | **0.001 s** | **0.002 s** |

Both binaries built from the same tree with one line differing, so this is a
paired build comparison in the same sense as the arm B control.

### `scripts/hook-budget.sh`

```
== 1. size (human check, budget 8 MB) ==      1,550,072 bytes (1.48 MB)  ok
== 2. ldd (human check) ==                    vdso, libgcc_s, libc, ld    ok
== 3. cargo tree containment (CI-wired) ==    21 crates, allowlist match  ok
== 4. LD_DEBUG=statistics (diagnostic) ==     221 relocations
```

**Only #3 is a CI gate.** #1, #2 and #4 are human PR-body checks.

Daemon-side numbers, where they are quoted at all, are `under_35ms` /
`under_60ms` counts. `/metrics.json` p95/p99 are linear interpolations inside
20 fixed buckets while `hook_bench` produces exact order statistics; comparing
them is invalid at any percentile (C3).

## Manual verification

A real `memgardend` (schema 7, embedding ready, Ollama `qwen3-14b-nothink`
ready) on a throwaway port **9142** against a throwaway database. 9077 (legacy
hindsight) and 9090 (memdash) were never bound, and the user's own Ollama on
11434 was never stopped. The transcript is three growing slices of a real
Claude Code transcript — 400, 800 and 1,200 lines of `e622d119-…jsonl`, copied
out; the live file itself was only ever read.

### Three rounds against a growing file

```
### round 1 — 400 real lines, 1,008,399 bytes
  state    : offset=1008399 chunk=1 turns=10 tsr=0 pending=019fc9a6 (0..1008399, chunk_before=0)
  sessions : byte_offset=1008399 confirmed_offset=0 inflight=1008399 chunk=0 turns=10 retains=1 messages_sent=35
  job 019fc9a6: running chunks 0/4 failed=0 facts=0
  documents: 1 | doc_keys: ['verify-session-1']
```

The dual cursor, doing the only thing it exists for: **`inflight=1,008,399`
while the job is in flight.** The optimistic cursor has moved, the durable one
has not, and the difference is exactly the bytes whose fate is unknown.

```
  job 019fc9a6: done chunks 3/4 failed=1 facts=34
      error=failed to parse ollama response as JSON after retries:
            expected `,` or `}` at line 1 column 4369

### round 2 — 800 real lines, 2,163,159 bytes
  state    : offset=2163159 chunk=2 turns=20 tsr=0 pending=019fc9af (1008399..2163159, chunk_before=1)
  sessions : byte_offset=2163159 confirmed_offset=0 inflight=2163159 chunk=1 turns=20 retains=2 messages_sent=67
  documents: 2 | doc_keys: ['verify-session-1', 'verify-session-1-c1']

### round 3 — 1,200 real lines, 2,900,135 bytes
  state    : offset=2900135 chunk=3 turns=30 tsr=0 pending=019fc9b2 (2163159..2900135, chunk_before=2)
  sessions : byte_offset=2900135 confirmed_offset=0 inflight=2900135 chunk=2 turns=30 retains=3 messages_sent=101
  documents: 3 | doc_keys: ['verify-session-1', 'verify-session-1-c1', 'verify-session-1-c2']
```

Round 2 proves the reconcile: it found job 1 `done`, cleared `pending`, and
posted the next delta from `1008399` under `-c1`. The `document_id` ladder is
visible in `doc_keys` — bare, `-c1`, `-c2`, one document per accepted delta,
which is the provenance §Binding decisions #7 is about.

**The cursors did not converge**, and that is the finding above rather than a
failure of the run: both jobs hit `chunks_failed=1` on a truncated model reply,
so the daemon withheld both content hashes.

### The forced failure, and the rollback

Extraction was forced to fail by repointing **this throwaway daemon's**
`[ollama] base_url` at a dead port and restarting it — which also exercised
`retain_jobs::fail_stale`. The user's Ollama was not stopped; the blast radius
is one temp directory.

```
closed out retain jobs orphaned by a restart count=1
  job 019fc9b2: failed chunks 3/35 failed=0 facts=5
      error=daemon restarted before the job finished

### Stop 31 — the reconcile finds a failed job
memgarden: retain: job 019fc9b2-6273-7ce2-87df-6214addd5085 failed, rolling back to 2163159
  state    : offset=2163159 chunk=2 turns=31 tsr=1 pending=none

### Stops 32-40 — the gate; the rolled-back delta waits for it
  state    : offset=2900135 chunk=3 turns=40 tsr=0 pending=019fc9b5 (2163159..2900135, chunk_before=2)
  sessions : byte_offset=2900135 confirmed_offset=0 chunk=2 turns=40 retains=4 messages_sent=135
  job 019fc9b5: running chunks 0/35 failed=0 facts=0
  documents: 3 | doc_keys: ['verify-session-1', 'verify-session-1-c1', 'verify-session-1-c2']
```

This is the whole PR in eleven lines:

* `offset` went **backwards**, 2,900,135 → 2,163,159, and `chunk` 3 → 2.
* The re-send carries the **same byte range** and the **same `document_id`** —
  `documents` stays at **3**, no fourth row, `chunk_before=2` restored.
* `messages_sent` 101 → 135: the same 34 messages, sent again.
* It waited for the turn gate rather than re-POSTing on the spot (§Diverged
  from the plan 9).

Without `pending`, `offset` would have stayed at 2,900,135 and those 736,976
bytes would have been gone — the daemon's `fail_stale` marking the job
`failed` would have had no reader.

### The ledger

```
retain_cap_saving: raw=367118 capped=48010 saved=319108 ratio=0.8692  ->  -86.9%
```

**−86.9 %**, inside the **−55 % / −87 %** band from
`/home/user/z_Setup/bank-b/docs/measurement.md` and next to that
file's "Build session, 1,193 msgs → −87 %" row. Over the whole 5,793,165-byte
real transcript, one forced retain.

It took a second run to get it, and the reason is worth recording: with
`[profile] name` unset — the **code** default, though `config.example.toml`
ships `"coding"` — `include_tool_calls` is `false`, tool content is dropped in
normalization before token counting, `saved_tokens` is 0 and
`insert_ledger` is never called. **A default-profile install produces no
`retain_cap_saving` rows at all**, which the example config already says in
prose and which is easy to read as a bug in this hook when it is not one.

## Mutation evidence

47 mutations, three rounds. Each reverted after its run.

| # | mutation | caught by |
|---|---|---|
| 1 | rollback keeps the advanced offset | `a_missing_job_row_rolls_back_rather_than_wedging_the_session` |
| 2 | rollback keeps the incremented chunk | `a_failed_job_rolls_the_cursor_back_and_the_same_bytes_are_re_sent` |
| 3 | `chunk_before` captured after the increment | `an_accepted_retain_advances_the_cursor_and_records_the_job` |
| 5 | `skipped` is not an accept | `duplicate_and_skipped_both_advance_without_a_pending_job` |
| 6 | `duplicate` is not an accept | `duplicate_and_skipped_both_advance_without_a_pending_job` |
| 7 | the breaker is not checked | `the_breaker_skips_the_socket_inside_its_window_and_not_beyond_it` |
| 8 | poisoning is not checked | `ten_rejections_poison_and_the_retry_is_hourly_rather_than_per_turn` |
| 9 | the breaker window has no upper bound | `cmd::tests::the_breaker_is_open_only_inside_a_window_it_could_have_written` |
| 10 | a future `poisoned_at` throttles | `catchup::tests::a_poisoned_at_in_the_future_does_not_throttle_a_session_out_of_existence` |
| 11 | `--force` does not bypass the gate | `catchup_posts_each_selected_session_and_re_checks_under_the_lock` |
| 12 | the gate is off by one | `the_turn_gate_retains_on_the_tenth_stop_and_connects_on_no_other` |
| 13 | `is_initial` is always false | `an_accepted_retain_advances_the_cursor_and_records_the_job` (5 tests) |
| 14 | `document_id` has no suffix | `a_compaction_is_counted_and_never_drives_the_chunk` |
| 15 | chunk 0 is suffixed too | `a_failed_job_rolls_the_cursor_back_and_the_same_bytes_are_re_sent` |
| 16 | `429` poisons | `a_429_never_poisons_however_many_times_it_arrives` |
| 17 | `503` counts a transport failure | `a_503_moves_no_counter_at_all` |
| 19 | poisoning is off by one | `ten_rejections_poison_and_the_retry_is_hourly_rather_than_per_turn` |
| 20 | an accept does not clear poisoning | `retain::tests::an_accept_advances_the_chunk_and_clears_every_failure_state` |
| 21 | a shrunken transcript does not reset | `a_cursor_past_the_end_resets_and_a_caught_up_one_makes_no_request` |
| 22 | any file type is a transcript | `retain::tests::only_a_regular_file_is_a_transcript` |
| 23 | an empty delta does not advance | `a_delta_with_nothing_to_send_advances_the_cursor_without_a_post` |
| 24 | compactions are not accumulated | `a_compaction_is_counted_and_never_drives_the_chunk` |
| 25 | a missing job row is unsettled | `a_missing_job_row_rolls_back_rather_than_wedging_the_session` |
| 26 | a running job does not skip the turn | `a_running_job_skips_the_turn_without_a_second_post` |
| 27 | an unreachable reconcile counts nothing | `an_unanswerable_reconcile_skips_the_turn_and_counts_a_transport_failure` |
| 29 | the 404 bank retry is skipped | `a_missing_bank_is_created_once_and_the_retain_retried_once` |
| 30 | an end reason is sliced by bytes | `session_end::tests::a_reason_is_bounded_on_a_char_boundary` |
| 31 | catch-up does not re-check under the lock | `catchup_posts_each_selected_session_and_re_checks_under_the_lock` |
| 32 | catch-up posts on a dry run | `a_dry_run_catchup_still_posts_nothing` |

**Round 1: 29 caught, 5 survived.** Each survivor got a test, and all five are
caught in round 2:

| # | survivor | now caught by |
|---|---|---|
| 4 | a `202` with no job id advances anyway | `a_202_without_a_job_id_does_not_advance_and_moves_no_counter` |
| 18 | a 5xx poisons | `a_500_counts_a_transport_failure_and_never_poisons` |
| 28 | `--force` scanned anywhere in argv | `force_in_a_value_slot_does_not_force_the_retain` |
| 34 | the retain POST uses the interactive budget | `the_retain_post_waits_longer_than_the_interactive_budget` |
| 35 | `cwd` sent as `""` rather than `null` | `an_absent_cwd_is_null_on_the_wire` |

### Round 3 — the review's new arms

Twelve more mutations against the code this review added. **8 caught, 4
survived, all four then closed:**

| # | mutation | now caught by |
|---|---|---|
| 36 | the `chunks_failed` arm is deleted | `a_done_job_with_a_failed_chunk_settles_but_records_the_gap` |
| 37 | a partial failure rolls back instead of settling | same, + `..._does_not_roll_back` |
| 38 | the gap is logged but never counted | same |
| 39 | every `done` job counts a gap | same |
| 41 | `try_lock` silently blocks | `state::tests::a_try_lock_does_not_wait_for_a_held_lock_but_a_plain_one_does` |
| 43 | compactions accumulate before the send decision | `retain::tests::an_accept_advances_the_chunk_and_clears_every_failure_state` |
| 45 | the mirror is never consulted on a state miss | `a_mid_session_state_wipe_resumes_from_the_mirror_rather_than_re_ingesting` |
| 46 | recovery seeds from the optimistic cursor | `session_start::tests::the_mirror_struct_cannot_carry_the_optimistic_cursor` |
| **40** | **`recall` waits on the lock again** | `recall_does_not_wait_for_a_holder_of_the_session_lock` |
| 42 | a rollback does not restore `compactions` | `a_rollback_restores_the_compaction_count_with_the_cursor` |
| 44 | an empty delta drops its compactions | `an_empty_delta_still_carries_its_compaction_boundaries` |
| 47 | a caught-up cursor does not restart the cadence | `a_caught_up_cursor_restarts_the_cadence` |

**#40 is the one worth reading.** Swapping `with_try_lock` back to `with_lock`
survived the entire suite, because every other test runs **one hook at a
time** — which is the same blindness that makes `hook_bench` unable to see the
stall it measures around. The test holds the lock from the test process and
asserts a bound on `hook recall`'s wall clock.

**Survivors after three rounds: none.**

### A corrected attribution from round 2

Review sampled the round-2 claims rather than accepting them, and #28's was
wrong. `force_in_a_value_slot_does_not_force_the_retain` does kill the scanning
mutant — but **not** via `accepts() == 0`, which was the stated mechanism.
Traced by deleting each assertion under the mutant:

| assertion removed | mutant |
|---|---|
| `accepts() == 0` | still caught |
| `offset == 0` | still caught |
| **`turns == 1`** | **survives** |

Under a scan, `force` becomes true, `--session`'s fixed slot then holds
`--force`, no session id is found, and the hook returns **before reading
stdin**. So the first two assertions pass for the wrong reason — they are also
satisfied by a hook that did nothing at all — and only the turn counter
distinguishes "read the payload and gated it" from "bailed out". The test now
says so.

Two of the five deserve their reason recorded, because they are the ones that
say something about the code rather than about the tests:

* **#28** is not reachable today. Only `session-end` spawns this argv and it
  always passes `--force` first, so a scan and a slot agree on every input the
  binary can currently receive. It is pinned anyway, because C2b's version of
  the identical defect was also unreachable right up until it was not.
* **#34** is C3's known unpinned survivor, inherited and now closed. The test
  costs 800 ms of wall clock and buys the difference between a 5 s budget and
  one that abandons a retain the daemon has already queued.

### And one defect no mutation could have found

`duplicate_and_skipped_both_advance_without_a_pending_job` **failed on first
run**, against code that looked right: `#[serde(default)]` does not cover an
explicit `null`, and the daemon sends `"job_id": null` on every `duplicate` and
every `skipped`. The struct failed to parse exactly the two answers the accept
table was added for, and a failed parse is a transport failure — so the cursor
stayed wedged on the response designed to unwedge it. Fixed by making both
fields `Option<String>`.
