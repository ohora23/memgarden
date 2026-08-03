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
5. **The turn gate.** 9 of every 10 `Stop`s end here.
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

### 11. The manual verification's "converging cursors" criterion is not always reachable

Plan §C4b asks the manual verification to show `byte_offset` and
`confirmed_offset` **converging**. They converge only when every chunk of every
job extracts cleanly. A single failed chunk leaves a gap that nothing closes —
see §Known limits, which is where the evidence for this lives. The criterion
should read "converging, or a gap attributable to a named `chunks_failed`".

### 12. `hook_bench`'s stub had to learn routes

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

What changed is that the hook now **says so**: a `done` with `chunks_failed > 0`
writes one stderr line under `[hooks] debug` naming the byte range that will
stay unconfirmed. The number the runbook tells the user to watch
(`byte_offset - confirmed_offset`, §Open questions 6) now has an explanation
next to it instead of being unattributed drift.

**Re-entry criterion.** A shadow run where the gap grows monotonically — i.e.
partial chunk failures are common rather than occasional — makes "one lost
chunk" the wrong trade. The fix then is not in this hook: it is a per-chunk
byte range in `retain_jobs`, so a re-send can carry the failed chunk alone.
That is a C1-shaped change, not a C4b one.

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

### `compactions` is a lower bound

See above: a rollback re-counts. Diagnostic only.

### The forced child inherits `[hooks] enabled` from a second config read

`session-end` reads the config to decide whether to spawn, and the child reads
it again. A config edited between the two makes the child act on the newer one.
Harmless — both answers are correct for the moment they were read — but it is
why `the_config_switch_makes_no_request_and_writes_no_state` waits before
asserting.

### Smaller ones

- The reconcile `GET` uses the **interactive** budget (400 ms), not the retain
  budget (5 s). A single-row read on a gated turn must not cost a `Stop` five
  seconds; the breaker covers the repeat.
- `advance` is `pub` so `catchup` can call it. There is no narrower visibility
  that spans two sibling modules without a third.
- A poisoned session's `session-end` retain is skipped inside the hour, which
  is the only place `force` losing to a throttle can cost a whole session's
  tail. `hooks status --clear-poison` (C5) is the exit.

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

**+82,840 bytes of binary for +0.006 ms**, which is inside the noise. The
reason is C3's lesson restated: the binary grew, the **relocation count did
not** — 221 before and 221 after.

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
hindsight) and 9090 (memdash) were never bound. The transcript is three growing
slices of a real Claude Code transcript — 400, 800 and 1,200 lines of
`e622d119-…jsonl`, copied out; the live file itself was only ever read.

See the PR body for the full session transcript.

## Mutation evidence

See the PR body.
