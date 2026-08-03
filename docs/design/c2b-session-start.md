# C2b / HK-1c — `hook session-start` and the detached catch-up

The first subcommand that reads stdin, loads config and makes a request.
C2a shipped `hook noop` and deliberately nothing else, so this is the PR where
several things C2a could only *describe* become testable — and where two of
them turned out to be wrong.

Third PR of Phase C. `src/cmd/{mod,session_start,catchup}.rs`, plus
`transcript_path` and `load_all` in `src/state.rs`.

## What `session-start` does, in order

1. **Parse stdin.** Unusable payload → return, before the config read. This
   ordering is what makes `empty_and_malformed_stdin_exit_zero` cover
   `hookio` rather than only an exit code, and it is what lets that test run
   with no daemon anywhere near it.
2. **Load config; honour `[hooks] enabled`.**
3. **Derive the bank** from `CLAUDE_PROJECT_DIR` (else `cwd`), percent-encoded
   into the URL path.
4. **`POST /v1/banks`** — 409 is the expected answer on every session but the
   first, and is not a failure.
5. **`POST /v1/banks/{bank}/sessions`** with `source`, `cwd`,
   `transcript_path`.
6. **Ensure a local state file exists**, seeding a missing one from the
   mirror's `confirmed_offset`.
7. **Spawn `hook catchup <session_id>` detached** and return.

**Nothing on stdout.** `SessionStart` is one of the three events whose stdout
becomes model context (plan §Binding decisions #3); the recall hook fires
milliseconds later and has something worth saying.

## Recovery: the wrong cursor is not reachable, not merely discouraged

C1 and C2a both wrote down that recovery must seed `offset` from
`confirmed_offset` and never from `byte_offset`. C2a's review then measured its
own enforcement and found it wanting: `SessionState::recovered`'s parameter is
*named* for the right column, but every field of `SessionState` is `pub`, so
C2b could have assigned `offset` directly and no test would have noticed.

The airtight version C2a asked for is what shipped here — **the mirror response
is deserialized into a struct that has no `byte_offset` field**:

```rust
struct Mirror {
    #[serde(default)] confirmed_offset: i64,
    #[serde(default)] chunk_index: i64,
}
```

serde ignores unknown fields, so the optimistic cursor never enters the
process. `the_mirror_struct_cannot_carry_the_optimistic_cursor` pins it by
feeding in a body that carries **only** `byte_offset` and asserting the
recovered offset is 0; adding the field back, or renaming `confirmed_offset`
to it, fails there. The integration test does the same end to end against a
real `SessionResponse` body with `byte_offset: 99999, confirmed_offset: 65536`
— the shape C1's own manual run produced, where taking the wrong one silently
skips 34,463 bytes nothing ingested.

Both fields are `i64`-then-clamped rather than `u64`. A negative would make the
*whole* struct fail to parse, which would silently drop `chunk_index` too and
fall back to a full re-ingest over a value the daemon cannot produce.

## Diverged from **the plan** (three things)

The plan has been right about the hard parts and wrong about three mechanical
ones. Each is a deviation, with what it cost to keep.

### 1. There is no `GET .../sessions/{sid}`. The POST already answered.

The plan's step is *"if the local state file is absent, seed it from
`GET .../sessions/{sid}`"*. But `POST .../sessions` is an **upsert that returns
the merged row** (`routes/sessions.rs::upsert_session` → `Json(SessionResponse)`),
and `session-start` always sends it. The GET can only return what the POST just
returned.

Dropping it removes a round trip, a 404 arm and a second timeout budget from
`SessionStart`. Nothing is lost: the POST does not send `turns`,
`chunk_index` or `byte_offset`, so it cannot move a cursor before reading it,
and `source` is first-write-wins.

One knock-on worth recording, because C2a's `http.rs` comment cites it: the
path guard's stated motivation was that *"C2b puts `session_id` — which arrives
on untrusted stdin — into `GET /v1/banks/{bank}/sessions/{session_id}`"*. It
does not. `session_id` only ever reaches the daemon inside a JSON body, where
serde escapes it. The guard is still right and still enforced at the choke
point; its first live caller is now C4b's `GET /v1/retain/{job_id}`.

### 2. The bank is created without a `mission`

The plan says to post `[profile] bank_mission`. `routes/banks.rs::create_bank`
**already** applies the daemon's own `[profile] bank_mission` to a bank created
without one. Sending ours would be a client overriding a server policy with a
value read from a config the server may not share — and the plan's own
divergence table gives the reason not to: *"the daemon already owns mission
precedence"*. The body is `{"bank_id": …}` and nothing else.

### 3. The catch-up child cannot post a delta in C2b, and must not try

The plan's C2b line says `catchup` *"posts each delta with `is_initial =
false`"*, and its manual-verification step asks to observe that. Neither is
reachable:

* the transcript delta reader is **C4a**;
* the retain POST and the `pending` reconciliation are **C4b**.

The second is not merely missing, it is *gating*. The plan's own sequencing
note says §Binding decisions #8 must be merged before the shadow run because
"committing the cursor on a bare 202 turns silent loss from a rare event into
the normal case". A C2b catch-up that posted would do exactly that, in a
detached process whose three streams are `/dev/null` — the worst possible place
to put an unreconciled cursor.

So C2b ships the **selection**, which is what all four tests the plan lists for
this file are actually about (excludes the current session, honours poison,
right argv, null stdio). The single call it gains in C4b is marked in
`catchup.rs`.

### 3b. …so the child was given the job C2a left unowned

A child that only selects would be a `fork` for nothing. It also runs
`state::gc`, which **C2a shipped with no caller at all**: one state file per
session, forever, in a directory nothing pruned. Once per session, off every
latency budget, already reading the directory — this is the right place, and it
gives the C2b child an observable side effect, which is how
`session_start_spawns_the_child_and_the_child_collects_the_state_dir` can prove
the spawn happened at all.

The cutoff is `session_retention_days`, the same window `memgardend`'s metrics
tick uses for the `sessions` row, so a row and its cache cannot disagree about
whether a session exists. Its doc comment said *"not by the CLI"*; that is no
longer true and has been corrected in `config.rs` and `config.example.toml`.

Consequently the child is spawned **unconditionally**, not gated on
`catchup_max_sessions > 0` as the plan has it. A knob meaning "catch up on
fewer sessions" must not also mean "leak one state file per session, forever";
`0` now means housekeeping only.

## The detached child

`Stdio::null()` on all three streams and `process_group(0)`. Two independent
reasons, both load-bearing:

* **On `SessionStart`, stdout is the model's context channel.** Anything the
  child writes lands in the conversation.
* **An inherited stdout fd keeps the pipe open after the parent exits**, which
  is the standard way a "detached" child hangs its supervisor — and it would
  have made C2a's measured arm B a fiction, because the hook's observed cost
  would become the child's lifetime.
* `process_group(0)` is the `setsid` half: the terminal going away, or a
  `SIGINT` to the foreground group, does not take the child with it.

Asserted by spawning a fake that writes to all three streams and then reports,
from a subshell, what `/proc/<shell>/fd/{0,1,2}` actually point at, plus its
`pgid`. `cat` in the fake is the stdin half: on an inherited pipe it blocks
forever; on `/dev/null` it returns instantly.

**The reporting has to happen in a subshell**, and finding out why is worth
recording, because the two obvious spellings produce a *false failure* that
looks exactly like a real bug. Measured on dash: `readlink /proc/$$/fd/1 > file`
and `{ …; } > file` **both** make fd 1 of the shell being inspected read back as
`file` — the first draft of this test reported `evidence.txt` for fd 1 against a
child that was, in fact, correctly wired to `/dev/null`. A subshell is a
separate pid, so `$p` still names the process the parent configured, and fd 1
reads `/dev/null`.

## Catch-up selection

`select(dir, current_session_id, cfg, now)`, in this order:

| filter | why it is not optional |
|---|---|
| `session_id != current` | The one race the advisory lock genuinely cannot arbitrate: a `source: resume` `SessionStart` has catch-up and the live retain hook on one cursor. `File::lock()` *does* serialize two of our own processes — but making two read-modify-writes atomic does not make "catch-up posts 0..N while retain posts M..N" correct. Free to avoid, so avoided |
| not poisoned within `poison_retry_secs` | Selecting purely on `offset < file_size` re-attempts a durably-rejected session on **every** launch, which turns a slow-retry state into a hot loop |
| transcript exists | Gone is not recoverable by retrying and is not the daemon's fault. An empty `transcript_path` — a C2a-era state file — lands here too |
| `file_size > offset` | The definition of stale. `<=` also covers a cursor past EOF |
| most recent transcript mtime first, then `truncate(catchup_max_sessions)` | The cap **drops** sessions, so the order decides which ones. That makes ordering a correctness property, not a presentation one |

`state::load_all` is new and reads the `session_id` from **inside** each file
rather than from its name: sanitization is many-to-one, so a filename stem is
not a session id, and `load(dir, stem)` would silently miss any session whose
id needed sanitizing. It applies `load`'s two conjuncts (`schema`, and the
stored id mapping back to this path) so a collision is invisible to catch-up
for the same reason it is invisible to `load`.

## `transcript_path` is new state, and the plan's state shape is short one field

Plan §Binding decisions #5 lists the per-session file's fields. Catch-up needs
a transcript path and there is no other way for it to get one — it wakes up
with no hook payload at all. Added with `#[serde(default)]`, so a C2a-era file
still loads (as absent, which makes catch-up skip it) instead of failing to
parse and costing a recovery round trip. No schema bump: an added optional
field is not an incompatible shape change, and
`a_state_file_written_before_transcript_path_existed_still_loads` pins that.

It is refreshed on every start, including a `resume`. **`bank_id` deliberately
is not**: a session's cursor belongs to the bank its bytes were posted to, and
re-deriving it mid-session — a `resume` from a different cwd, an edited
`directory_bank_map` — would leave the offset pointing into another bank's
document.

## Failure posture, as implemented

`session-start` has four outcomes and they move different things, which is why
the code has a four-arm enum rather than a `Result`:

| outcome | counter | reason |
|---|---|---|
| 2xx | `transport_failures = 0`, `breaker_open_until_ms = 0` | any success clears the breaker |
| non-2xx | **none** | §Failure posture's row is "ignore, exit 0". `reject_failures` exists to poison a *cursor*, and this subcommand does not advance one — counting rejections here would let a daemon-side validation bug disable a session's memory |
| connect / timeout / unparseable 2xx body | `transport_failures += 1` | a daemon that answers something we cannot read is the same class of problem as one that does not answer |
| bad `daemon_url` | **none** | a config fault. A typo that opened the circuit breaker would look exactly like an outage in `hooks status` |

The state file is still written on every one of those, including the failures:
losing it would cost the *next* hook its bank id for no reason.

`session_id` is bounded at 200 bytes client-side — the daemon's own
`store::sessions::MAX_SESSION_ID_BYTES`, mirrored rather than imported because
`memgarden-store` is what this crate's CI-enforced dependency budget keeps out.
It arrives on stdin, which `hookio` bounds at 8 MB; without the check an 8 MB id
becomes an 8 MB state file, a body the daemon 400s, and an argv element over
`ARG_MAX`.

The timeouts are `connect_timeout_ms` + **`recall_timeout_ms`** (400 ms).
The plan names no knob for this subcommand. Both requests are single-row writes
with no `prepare()` behind them — nothing like the tokenize-twice-then-202 that
justifies retain's 5 s — so the interactive budget is the right one. Chosen once
in `cmd::interactive_timeouts` so `session-end` (C4b) inherits it rather than
picking again.

## Measurement

Release build, N=300, 20 discarded warm-ups, embedded stub daemon, hermetic
`MEMGARDEN_CONFIG`/`HOME`/`XDG_DATA_HOME`. Arm B gets **the same stdin as arm
A** (`--stdin-b` defaults to `--stdin-a`), which C2a added and this is the
first PR that needed: a delta quoted without it charges the subcommand for the
driver's pipe write.

| arm | p50 ms | p95 ms | p99 ms | min ms |
|---|---|---|---|---|
| A `hook session-start` | 0.566 | 0.782 | 1.158 | 0.495 |
| B `hook noop` (baseline) | 0.327 | 0.446 | 0.537 | 0.270 |
| paired A−B | **0.238** | 0.369 | 0.777 | -0.290 |

**0.238 ms of own work against the 10 ms per-hook budget** — two loopback
POSTs, a config load, a locked state write and a `fork`. Arm B 0.327 ms against
its 1.5 ms gate: **PASS**. Null control on the same build (A = B = `hook noop`,
payload on both arms): arm B 0.311, paired **0.002** — the harness still
measures "no difference".

### Did arm B move? Measured directly, not by elimination

C2a's note records arm B at 0.243 ms and this run reads ~0.31, so the obvious
story is "the binary grew and that is the cost". The first version of this note
said exactly that, by elimination. **It over-attributed, and the direct
measurement says so.**

`hook_bench` gained `--bin-b`, which pairs two *builds* the same way it already
pairs two subcommands — because the reason not to compare across runs applies
here with full force: C2a's own note measured **+1.5 ms on identical bits**
between sessions, which is fifty times the effect being attributed. So C2a's
binary was rebuilt from `c27e681` in this session (496,160 bytes, byte-for-byte
the size its note quotes) and run as arm B against this one as arm A, same
driver, same `hook noop`, alternating:

| run | A (C2b binary) | B (C2a binary) | paired A−B |
|---|---|---|---|
| 1 | 0.288 | 0.271 | **0.016** |
| 2 | 0.283 | 0.270 | **0.014** |
| 3 | 0.280 | 0.268 | **0.015** |

**The binary growth costs 0.015 ms**, stable to a microsecond across three
runs of N=300. It is real, it is the expected direction, and it is *one fifth*
of the apparent 0.07 ms gap against C2a's recorded number. The remainder is
cross-session drift — the thing this harness exists to refuse to measure. The
honest statement is therefore:

* arm B's **paired** cost of the C2b binary over the C2a binary: **+0.015 ms**;
* arm B's **absolute** number moved from 0.243 to ~0.31 between sessions, and
  that difference is not attributable and should not be attributed.

The mechanism is still the expected one: **496,160 → 1,390,184 bytes**, 165 →
220 relocations, because `hook noop` never referenced `Config::load` so the
TOML parser was dead-stripped from the C2a binary. What changed is the size of
the claim, from an inference to a number.

`scripts/hook-budget.sh`:

```
1. size    1390184 bytes (1.33 MB)             <= 8 MB budget   ok   [human check]
2. ldd     linux-vdso, libgcc_s, libc, ld-linux-x86-64          ok   [human check]
3. tree    21 crates, unchanged from C2a, diffed against the allowlist  [CI-WIRED]
4. LD_DEBUG  220 relocations, 7 from cache                             [diagnostic]
```

**Only #3 is a CI gate.** #1, #2 and #4 are human PR-body checks.

## Diverged from legacy

* **`session_start.py` does almost nothing, and what it does is what we
  refuse.** Legacy's SessionStart is a health check plus
  `prestart_daemon_background` (`session_start.py:42-50`, `lib/daemon.py:255`)
  — a hook that spawns a model-loading daemon, which is the pg0 restart race
  the PRD exists to remove (plan §Binding decisions #10). Ours never starts,
  stops or restarts `memgardend`.
* **The bank is ensured through the API, not memoized in a client file.**
  Legacy tracks "which banks have had their mission set" in
  `bank_missions.json` with a 10,000-entry truncation hack
  (`bank.py:146-179`). One idempotent `POST /v1/banks` per session replaces the
  file, the truncation and the global flock.
* **Session state is mirrored server-side and recovered from there.** Legacy
  has no equivalent: a wiped state dir means re-ingesting a transcript from
  index 0. Here it means one `POST` whose response carries `confirmed_offset`.
* **Catch-up across sessions has no legacy counterpart at all.** Legacy's
  retain commits its cursor on the HTTP response and has nothing that revisits
  a session after it ends, so a daemon outage that spans a whole session is
  permanent loss there.
* **Never exit 2**, inherited from C2a and now exercised by a subcommand that
  actually does work: a malformed payload, a 200-byte-over session id, a dead
  daemon, a 400, and a bad `daemon_url` are all silent exit 0.

## Known limits and deferred items

### The exclusion filter is not a complete answer to the concurrency it names

The first version of this note called the current session *"the one race the
advisory lock genuinely cannot arbitrate"*. **That is false, and it is the same
class of error C2a's review caught in the `byte_offset` doc comment: an
enforcement claim stronger than the code supports.**

A session **live in another Claude Code window** passes every filter in
`select` — it is not the current id, its transcript exists, and `file_size >
offset` is true almost all of the time between its 10-turn retains. Its own
`Stop` hook is on the same cursor. The `session_id != current` filter removes
the one instance we can identify for free; it does not remove the class.

What bounds it is **C4b's re-load under `state::with_lock`**, which the marker
in `catchup.rs` now instructs rather than leaving to memory: re-read the state
and re-check `offset < file_size` inside the lock, and the worst case is a
redundant post the daemon answers `duplicate` — not byte loss, because both
writers only ever move the cursor forward and the content hash dedups.

A per-`state_dir` `catchup.lock` was considered and **rejected**: the
per-session lock C4b will already take arbitrates two catch-up children against
each other for the same reason it arbitrates catch-up against retain, so a
second lock file buys nothing the first does not already cover. Recorded here
because "settle it before C4b" was the ask, and this is the settlement.

### Accepted risks, with the reason each was accepted

* **`transcript_path` is stored from stdin unvalidated, and C4b reads that
  file.** Demonstrated: `"transcript_path": "/etc/passwd"` is stored verbatim
  and the child selects it; `metadata()` follows symlinks and needs no read
  permission, so `/etc/shadow` is selected too. In C2b the child only `stat`s
  it — the risk lands in C4b's `send_delta`, which reads the file and POSTs its
  contents into a bank the model later recalls.
  **Validating at store time was considered and deliberately not done**; see
  §"Where to validate `transcript_path`" below for the argument. The C4b marker
  carries the requirement.
* **`with_lock` is the only place left that opens a path it did not create.**
  It no longer truncates (`create_new` at 0600, falling back to a **read-only**
  `File::open`), so a planted `sX.lock -> /outside/precious.conf` is opened and
  flocked but never written. It is still *opened*, which on a FIFO would block —
  a case that requires write access to the 0700 state dir, at which point the
  attacker can simply write the state file.
  `// ponytail:` `O_NOFOLLOW` is the airtight version and needs `libc`, which
  the CI-enforced dependency closure refuses. The read-only fallback is the
  property that matters; revisit if a `libc` ever enters for another reason.
* **A state file named `--dry-run.json` is creatable.** `path_for` sanitizes
  path separators and control characters but not a leading `-`, so a session id
  that looks like a flag produces a filename that looks like one. Harmless to
  us — `gc` and `load_all` use full paths, never a shell — and a hazard only
  for a future `find … | xargs` over the state dir. Not sanitized because
  Claude Code sends 36-character uuids and changing `path_for` would change
  filenames for a hazard we do not have. The argv half, which was **not**
  harmless, is fixed and tested.
* **An empty daemon-side `[profile] bank_mission` now yields a bank with no
  mission at all.** Since the hook sends none (divergence 2), the daemon's is
  the only source. Legacy's `ensure_bank_mission` returns early on an empty
  mission too, so this is parity; it just means "no mission anywhere" is now
  reachable in one step instead of two.
* **A *rejected* sessions POST loses the recovery, where the GET would have
  had it.** An over-long `cwd`/`transcript_path` (400) or a missing bank (404)
  falls to `SessionState::new` → offset 0 → a full re-ingest on the next
  retain. Not lossy — `doc_key` answers `duplicate` — but not free on a 100 MB
  transcript. **This is the only real cost of dropping the GET** (divergence 1),
  and it is bounded by `retain.max_initial_messages`.

### Where to validate `transcript_path` — at the read, not at the store

Review's finding is correct and its suggested fix (absolute, `.jsonl`, no
`ParentDir`, checked before storing) is the obvious one. **It is the wrong
place, and three things say so.**

1. **A store-time guard misses the caller that matters.** C4b's `retain` reads
   the transcript from the **payload's** `transcript_path` on every `Stop` — it
   never goes near the state file. Only catch-up reads the stored copy.
   Validating at store time therefore guards the once-per-session path and
   leaves the once-per-ten-turns path completely open. One guard in the reader
   covers both callers; that is the same "fix it where all callers route
   through" rule that put the escaping guard in `http::request` rather than in
   six subcommands.
2. **A store-time check is a time-of-check/time-of-use gap by construction.**
   The stored path can be 89 days old. Whatever it named when we wrote it says
   nothing about what it resolves to when catch-up finally reads it — and a
   *stale* stored path is precisely the case most likely to have gone wrong,
   because a session old enough to need catch-up is a session whose files have
   had the most time to move.
3. **It constrains a vocabulary Claude Code owns.** `.jsonl` under
   `~/.claude/projects/…` is true today. This repo already has a decided
   position on that shape of validation, from C1: `source` and `end_reason` are
   *"stored, not validated… a mirror that 400s on a value it has not heard of
   would break the hook on a Claude Code upgrade"*. A `.jsonl` allowlist has
   the same failure mode with a worse blast radius — retain silently stops for
   everyone on the day the extension changes, and the hook that stops working
   is the one whose entire contract is "never break the session".

So: **store it verbatim, validate at the point of the read in C4b**, and make
the check about the *properties that matter at the read* rather than about the
path's spelling — open it, `fstat` the handle, refuse anything that is not a
regular file. That is not a pattern match, so a Claude Code path change cannot
break it, and it is checked against the file we are actually about to send.

The counter-argument, stated so it is not lost: this leaves a stored capability
that a future author could read without the guard. The C4b marker in
`catchup.rs` names the requirement at the call site, and this section is the
reason it is a requirement rather than a preference.

### Smaller ones

* **The child does not post.** See divergence 3. C4b adds the one call.
* **`session-start` does not consult the circuit breaker.** It runs once per
  session, and skipping it on an open breaker would skip the state recovery
  the breaker has nothing to do with. It still *feeds* the breaker.
* **A `resume` in a different project keeps the original bank.** Deliberate
  (see `transcript_path` above), and it means a session that moves repos
  keeps writing to the bank it started in. `// ponytail:` the upgrade path is
  a bank change ending the session and starting a new one, which needs C4b's
  `session-end` to exist first.
* **`load_all` reads every state file on every catch-up.** Bounded two ways
  now: by the GC the same child performs (count), and by
  `MAX_STATE_FILE_BYTES` (size) — `gc` prunes by mtime only, so without the
  second one an oversized `*.json` is re-read in full on every session start
  for the whole retention window. Note the count is **2× sessions**, not 1×:
  every session leaves a `.json` *and* a `.lock`, and a crashed `store` can
  leave a `.tmp`. `// ponytail:` an index if a machine ever accumulates enough
  sessions for a `read_dir` to matter.
* **A symlinked state file is skipped, not resolved.** `path_for`'s
  containment check is lexical and cannot see through a symlink;
  `entry.file_type()` (an `lstat`) is what covers it. The effect is that a
  legitimate symlinked state file would also be ignored — nobody creates one,
  and "reads as absent" is already the handling for every unusable file.
* **Two `SessionStart`s for the same session racing** would both see an absent
  state file and both recover from the mirror. They would recover the *same*
  numbers, and the write is locked, so the outcome is identical either way.
* **The bank POST's response is discarded.** A failure there is reported by
  the sessions POST that follows, which 404s without a bank.

## Manual verification

`memgardend` (release, embeddings off, real Ollama) on **127.0.0.1:9101**,
fresh database, `schema_version: 7`. Full transcript in the PR body; the
observed facts:

```
echo '<SessionStart payload>' | memgarden hook session-start   -> exit 0, EMPTY stdout

GET /v1/banks            -> claude-code::repo
                            mission "Track MemGarden Phase C decisions."
                            ^ the DAEMON's [profile] bank_mission. The hook sent
                              no mission at all — divergence 2, demonstrated.

GET .../sessions         -> source=startup, cwd, transcript_path, byte_offset=0
<state_dir>/<sid>.json   -> schema 1, bank_id, transcript_path, offset 0

  # recovery: make the two cursors differ, the way an unconfirmed POST does
  real retain (job done)          -> byte=65536 confirmed=65536
  POST .../sessions byte=99999    -> byte=99999 confirmed=65536 inflight=34463
  rm <state_dir>/<sid>.json
  memgarden hook session-start    -> offset=65536  chunk=2  pending=None
                                     ^ the DURABLE cursor. 99999 would have
                                       skipped 34463 bytes nothing ingested.

  # the detached child
  plant: stale (offset 0, 4096-byte transcript), poisoned 10 min ago,
         and a state file dated 1970
  memgarden hook session-start    -> exit 0; 1 s later the 1970 file is GONE
                                     (the child ran; the other three survive)
  memgarden hook catchup <sid> --dry-run
      excluded c2b-manual-0001
      selected 1
        c2b-stale-0002 offset=0 size=4096 behind=4096
                                     ^ current excluded, poisoned skipped
  move poisoned_at to 2 h ago (> poison_retry_secs 3600)
      selected 2   -> c2b-stale-0002, c2b-poisoned-0003
                                     ^ slow retry, not a latch

  # daemon down
  memgarden hook session-start x2  -> exit 0, 0.001 s wall, transport_failures=2,
                                      reject_failures=0, bank_id still recorded
```

Nothing bound 9077 (legacy hindsight) or 9090 (memdash) — both were confirmed
live and left alone; the daemon used a throwaway port and every listener in the
tests and the bench binds port 0.

**What is NOT shown, and cannot be:** the child posting a delta. See divergence
3 — that is C4a plus C4b, and doing it here would commit a cursor on a bare
202.

## Mutation evidence

**32 mutations, applied one at a time by a script, each reverted after its
run** (C2a's standard was 13 + 16, and its own rule is that the convention this
repo keeps getting wrong is a correct rule that no test pins). **29 caught, 3
survive and are named below.** Two of the runs were written as *predicted*
survivors, to check the prediction rather than to pass; one of the two was
wrong in an instructive way and produced an extra run.

The last six are the review round: each one reverts a fix to **exactly** the
code that shipped before it, so "caught" means the new test fails on the code
review demonstrated the defect against.

| mutation | caught by |
|---|---|
| `Mirror` reads `byte_offset` instead of `confirmed_offset` (via `serde(rename)`) | `the_mirror_struct_cannot_carry_the_optimistic_cursor`, and `recovery_seeds_the_offset_from_confirmed_offset_and_never_from_byte_offset` |
| the `[hooks] enabled` check deleted from `cmd::enabled_config` | `the_config_switch_makes_no_request_and_writes_no_state` |
| an existing state file is ignored and the mirror always recovers | `an_existing_state_file_is_not_rewound_by_the_mirror` |
| the `session_id != current_session_id` filter neutralized | `the_current_session_is_excluded_even_when_it_is_the_stalest`, `the_child_selects_stale_sessions_and_excludes_the_one_it_was_given` |
| the poison filter neutralized | `a_poisoned_session_is_skipped_inside_the_throttle_and_retried_outside_it` |
| `now_ms < poisoned_at + window` → `<=` | the same test's boundary case — which is why it asserts at `+ window - 1` **and** `+ window` |
| `file_size <= state.offset` → `<` | `only_sessions_whose_transcript_has_grown_are_selected` (the cursor-past-EOF row) |
| `sort_by_key(Reverse(..))` → ascending | `the_most_recently_active_transcripts_win_the_capped_slots` |
| `truncate(catchup_max_sessions)` removed | the same test |
| `Stdio::null()` → `Stdio::inherit()` on **stdout** | `a_detached_child_gets_dev_null_on_all_three_streams_and_its_own_process_group` |
| `process_group(0)` removed | the same test's `pgid` half |
| `Mirrored::Rejected` moved into the `transport_failures += 1` arm | `a_rejected_mirror_moves_no_counter` |
| `Mirrored::Config` folded into `Transport` | `a_non_loopback_daemon_url_is_not_counted_as_a_transport_failure` |
| the success arm stops clearing `transport_failures`/`breaker_open_until_ms` | `a_daemon_that_is_down_exits_zero_and_counts_exactly_one_transport_failure` |
| the `MAX_SESSION_ID_BYTES` guard removed | `an_unusable_session_id_writes_nothing_and_makes_no_request` |
| `state::gc` call removed from `catchup::run` | `session_start_spawns_the_child_and_the_child_collects_the_state_dir` |
| the `spawn_detached` call removed from `session_start::run` | the same test |
| `load_all`'s `path_for(…) == Some(path)` conjunct removed | `load_all_finds_every_session_load_would_accept_and_no_others` |
| `load_all`'s `schema` conjunct removed | the same test |
| `transcript_path` refresh removed | `an_existing_state_file_is_not_rewound_by_the_mirror` |
| the bank `POST` dropped | `the_bank_is_created_on_the_first_run_and_a_409_is_not_a_failure` (request count) |
| `encode_path_segment` dropped from the sessions path | the same test's three-token request-line assertion |
| `none_if_empty` returns `Some(value)` unconditionally | `empty_payload_fields_are_sent_as_null_rather_than_as_an_empty_string` — **but see the survivors**: this pins the helper, not its use |
| **review round** — argv back to skipping a `--`-prefixed session id and scanning all of argv | `a_session_id_that_looks_like_a_flag_is_still_the_session_id` |
| **review round** — the `poisoned_at <= now_ms` half dropped | `a_poisoned_at_in_the_future_does_not_throttle_a_session_out_of_existence` |
| **review round** — `with_lock` back to `File::create` | `a_planted_symlink_at_the_lock_path_is_not_truncated` |
| **review round** — `load_all`'s `file_type().is_file()` removed | `load_all_skips_a_symlinked_state_file` |
| **review round** — `.mode(0o600)` removed | `state_files_are_created_0600_regardless_of_umask` |
| **review round** — `load_all`'s read unbounded | `an_oversized_state_file_is_bounded_rather_than_read_whole` |

**Survivors, reported rather than papered over:**

* **`Stdio::null()` → `Stdio::inherit()` on *stdin* survives.** The stdout and
  stderr halves are caught; fd 0 is not, because under `cargo test` the harness
  process's own stdin is *already* `/dev/null`, so "inherit" and "null" are
  indistinguishable from inside the child. Pinning it would need the test
  process to put a real pipe on its own fd 0 before spawning, which needs
  `libc` — a dependency this crate's CI-enforced budget refuses for a mutation
  on the least dangerous of the three streams. The `cat` in the fake still
  demonstrates the property that matters (a child that does not block on its
  input); what is unpinned is the *mechanism* that guarantees it.
* **Dropping `none_if_empty` at a call site survives.** `"cwd":
  input.cwd.as_str()` — sending `""` where `null` was meant — passes all 541
  tests. The daemon reads `Option<String>`, where `null` means "leave the
  column alone" and `""` means "set it to empty", so this would let a payload
  without a `cwd` erase a `cwd` an earlier start recorded. Pinning it needs a
  `sessions` row read **back through the daemon** after two starts with
  differing payloads, which is a `memgardend` integration test rather than a
  CLI one. The helper itself is pinned (row 23 above); its three uses are not.
* **`interactive_timeouts` returning `retain_timeout_ms` instead of
  `recall_timeout_ms` survives.** Nothing here waits long enough to tell 400 ms
  from 5 s. C2a's `the_read_timeout_that_arrives_is_the_one_the_caller_passed`
  pins the *mechanism* with two disjoint windows; what is unpinned is this
  subcommand's *choice* of knob. That would need a trickling-stub test per
  subcommand, and this note is cheaper than four of those.
