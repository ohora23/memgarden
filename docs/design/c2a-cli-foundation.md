# C2a / HK-1b — the `memgarden-cli` crate, transport, state, and the measuring instrument

The `memgarden` binary's skeleton, its hand-rolled loopback HTTP client, the
per-session state cache, bank derivation, the `[hooks]` config section, and the
interleaved-paired benchmark. Second PR of Phase C.

**No user-facing hook subcommand ships here — `hook noop` only.** That is
deliberate sequencing, the same shape as landing AX-2 before CE-11: the
measuring instrument lands first, so every later Phase C PR reports a delta
against an established baseline instead of against a number from yesterday, and
arm B's fixed cost is pinned before anything can quietly inflate it.

## The property this crate exists to guarantee: never exit **2**

On `UserPromptSubmit`, exit 2 "Blocks prompt processing and **erases the
prompt**". On `Stop` it prevents the turn from ending. `recall.py:287-291`
exits 2 when `debug` is set, so any unhandled exception in the legacy recall
hook deletes what the user typed.

**This is history, with a date, not a hypothetical.** The user's real
`~/.hindsight/claude-code.json` carried `debug: true` — that is what the Phase C
plan recorded and what motivated this crate — until **2026-08-03**, when it was
set to `false` in response to the hazard being found. Legacy's own default is
`false` and the `coding` preset does not set it; the flag remains one env var
(`HINDSIGHT_DEBUG`) from erasing prompts again. A configuration that had to be
changed by hand to stop deleting user input is the single strongest argument for
the cutover, and MemGarden makes the failure structurally impossible rather than
carefully avoided:

| mechanism | where |
|---|---|
| `main() -> ExitCode::SUCCESS` on every path, no `?` out of `main` | `src/main.rs` |
| panic hook prints one stderr line and `process::exit(0)` | `src/main.rs` |
| **no `clap`** — its usage errors exit 2 | `Cargo.toml`, with the reason in a comment |
| unknown subcommand, no subcommand, `--help` -> silent 0 | `dispatch`'s `_` arm |
| empty / malformed stdin -> `None` -> silent 0 | `hookio::parse` |

The guarantee is stated as **never 2**, not "never non-zero". `SIGSEGV`/`abort`
give 139/134 and a missing binary makes a shell launcher return 127; none of
those is 2 and none is preventable from inside the process. Claiming the
stronger thing would be claiming something the code cannot deliver.

One consequence worth naming, because it looks like a bug: `process::exit(0)`
from the panic hook **skips Rust's end-of-main stdout flush**. That is
intentional. A subcommand that panics half way through writing
`additionalContext` emits *nothing* rather than a truncated JSON line that
Claude Code would hand to the model. Empty is the correct partial result for
every one of our events. `an_injected_panic_exits_zero_with_empty_stdout`
asserts both halves.

## Transport: ~200 lines of `std::net`, and the four "no"s that make it safe

`reqwest` is already in the workspace and is still refused: it pulls `tokio`,
and spinning a multi-threaded async runtime inside a process whose entire
budget is sub-millisecond *is* the cost. The client is one connection, one
request, one response — no keep-alive, no redirects, no TLS, no chunked
decoding.

That is only safe because of what is on the other end. Three of the four "no"s
are load-bearing:

* **No TLS.** The daemon binds `127.0.0.1` only. `daemon_url` is validated to
  `http://` in `memgarden-core`, and `Target::parse` *additionally* refuses any
  host that is not `127.0.0.1`/`localhost`/`::1`. Two checks rather than one so
  that a caller building a URL by hand cannot leave the loopback either.
* **`Host` is mandatory and loopback.** `check_host` 403s anything else
  (`middleware.rs:34-46`). `a_post_round_trips_and_carries_a_loopback_host_header`
  asserts the bytes that reach the socket, not the field that produced them.
* **Chunked transfer encoding is a failure, not a format.** axum serializes
  `Json` to bytes and sets `Content-Length`; it never chunks a response we ask
  for. So a chunked reply means we are not talking to the daemon we think we
  are, and mis-parsing it would put a chunk-size line into the model's context.
  `parse_head` rejects **any** `Transfer-Encoding` outright, and the
  integration test sends a chunked body containing the word `hijack` to prove
  none of it surfaces.
* `localhost` is resolved **by table, not by `getaddrinfo`** — the resolver
  reads `/etc/nsswitch.conf` and can consult a network service, which is not a
  dependency a sub-millisecond process should acquire for a name whose answer
  is fixed.

`read_response` reads until the header terminator and then **exactly**
`Content-Length` more bytes. `read_to_end` would have been shorter and is
wrong: it waits for the peer to close even after the whole body has arrived, so
a daemon that ignored `Connection: close` would cost every hook its full io
timeout while looking perfectly healthy. Missing `Content-Length` is a failure
for the same reason chunked is — "read until close" is exactly the ambiguity
this client refuses to have.

### `SO_RCVTIMEO` is not a request budget (review HIGH)

The first version of this PR set `set_read_timeout(timeouts.io)` and stopped
there. That is `SO_RCVTIMEO`: it bounds **one `read()`** and re-arms on every
byte that arrives. Review measured the consequence against the shipped
`recall_timeout_ms = 400` — a server sending the head promptly and then one
body byte per 300 ms:

```
http::get(&target, "/v1/recall", &Timeouts::from_ms(50, 400))
  -> returned after 30.007 s, and returned Ok
```

Thirty seconds on `UserPromptSubmit`, **reported as success**, so the circuit
breaker never sees it, `transport_failures` never moves, and the daemon reads
healthy. It contradicted this file's own "recall fails open", and it happened in
the PR whose entire premise is a bounded wall clock — the one place the
instrument could not see itself.

`MAX_HEAD_BYTES` was no help and the old comment overclaimed: it bounds
**bytes, not time.** 16 KB at one byte per read is 16,384 reads, and the body
loop allowed 8 M.

The fix is a whole-request deadline (`Instant::now() + timeouts.io`) re-armed
onto the socket before **both** loops' every read, so the remaining budget
shrinks monotonically and the socket option only ever bounds a single blocked
syscall. The early return is preserved — still exactly `Content-Length`, still
no wait for FIN. Measured after: **408 ms** against a 400 ms budget, `Timeout`.
Removing the two re-arms puts it back to 29.71 s returning `Ok`, which is what
`a_trickling_server_is_bounded_by_the_whole_request_deadline` now pins.

### The path guard is at the choke point

`request()` refuses any path that is empty, not absolute, or contains a byte
`<= 0x20` or `0x7f`. Not reachable in C2a, but C2b puts `session_id` — which
arrives on untrusted stdin — into `GET /v1/banks/{bank}/sessions/{session_id}`,
and a raw CR there is request splitting, not a 400. `encode_path_segment`
existed and was correct; nothing *enforced* it. One guard where all six future
subcommands route through beats six guards they each have to remember.

A 4xx is a **response**, not a transport failure. The client returns it with
its body rather than erroring, because `transport_failures` and
`reject_failures` are counted separately precisely so a down daemon can never
poison a session (plan §Failure posture), and collapsing them in the client
would make that split unimplementable.

## State: one file per session, and it is a cache

`<state_dir>/<session_id>.json`, replacing legacy's three global files
(`turns.json`, `retention_tracking.json`, `bank_missions.json`) and their global
flocks (`state.py:95-210`). No global read-modify-write, no cross-session
contention, no 10,000-entry truncation hack.

**The authoritative copy is the daemon's `sessions` row (C1).** This file exists
so the fast path never makes a network call to find out where it is; losing it
costs one recovery round trip (C2b), not a session's memory. Everything else
follows from that: a corrupt file, an unreadable file and an unrecognised
`schema` all read as *absent*, and none of them is an error.

### Recovery seeds from `confirmed_offset`, enforced by the signature

C1's design note is explicit: C2b must rebuild a wiped state file from
`GET …/sessions/{sid}`'s **`confirmed_offset`, never its `byte_offset`**.
`byte_offset` is what some hook *POSTed*, so it is already ahead of reality
after a failed job or the byte-budget 429 — seeding from it skips exactly the
bytes the dual cursor exists to protect.

A doc comment would not have survived C2b. So the only constructor that builds
state from the mirror is

```rust
SessionState::recovered(session_id, bank_id, confirmed_offset, chunk)
```

whose parameter is named for the column it takes. There is no constructor that
accepts `byte_offset`, so the wrong one cannot be reached for without
deliberately renaming an argument.

### No `fsync`, and saying so out loud

The first version fsynced the temp file and justified it as "what makes the
rename publish complete bytes". That reasoning is a *crash* property, and
within a live system it is not needed: the page cache is coherent, so
`rename(2)` publishes full contents with or without the sync and no reader can
observe the empty inode. What the fsync actually bought was power-cut survival
— which the very next sentence of the same comment explicitly declined to pay
for. A real disk sync on the per-turn hot path for a property the code
disclaims two lines later is not a trade-off, it is a contradiction, and arm B
cannot see the cost because `noop` never touches state.

Deleted. **This cache is not crash-durable, deliberately.** Deleting the code
is also how the unpinnable "dropping `sync_all` fails no test" mutation gets
closed: there is nothing left to drop.

### `File::lock()` is advisory, and the comment says only what it does

Review measured this on C1: with the lock held, a second handle that never
calls `lock()` writes straight through it. It serializes **MemGarden against
MemGarden** and nothing else. That is exactly enough for the one race we have —
an `async: true` `Stop` still running when the next fires, both of them our own
processes — and it is not protection against anything we do not control.

`concurrent_locked_writers_serialize_but_an_unlocked_one_does_not` asserts both
halves in one test: four locked threads increment a counter to 4 through a
5 ms read-modify-write window, and then an unlocked writer clobbers the file
while the lock is held. The negative half is there so nobody upgrades the
comment into a promise.

A lock we cannot acquire does not fail the hook — `f` runs anyway, unlocked,
because dropping a turn's state to protect against a race with ourselves is the
worse trade.

## Bank derivation without a subprocess

Legacy runs `git -C <cwd> rev-parse --path-format=absolute --git-common-dir` on
every hook invocation: **0.435 ms p50 measured**, more than the entire rest of
the hook. `repo_root` walks up for `.git` instead — a handful of `stat`s — and
gives the same answer for the two shapes that occur:

| `.git` | resolution |
|---|---|
| a directory | that directory's parent is the repo root |
| a file `gitdir: <common>/worktrees/<name>` | strip `worktrees/<name>`, take the common dir's parent — so every worktree of a repo shares one bank |

The default is `claude-code::<project>`, byte-identical to the live legacy bank
ids, which is what lets AC-1 compare the two systems on the same bank. Bank ids
are percent-encoded into the URL path; both live shapes need it —
`claude-code::bank-b` (a `::`) and
`claude-code::bank e` (a `::` **and** a space). The integration
test asserts the resulting request line splits into exactly three
space-separated tokens, because an unescaped space would make four and the
daemon would answer 400.

## `[hooks]`, and three keys the plan did not list

`[hooks]` lives in `memgarden-core` because **two** binaries read it: the hook
CLI on every invocation, and `memgardend` for `session_retention_days`.

The plan's §C2a key list names only the transport and failure knobs, but the
same section requires `directoryBankMap` -> static -> `agent::project`
resolution, which cannot be expressed without config. Three keys were added:

| key | legacy | note |
|---|---|---|
| `bank_id` | `bank.py:103-106` | non-empty pins every session to it |
| `agent_name` | `bank.py:124` | the `agent` segment |
| `directory_bank_map` | `bank.py:87-101` | exact directory overrides, canonicalized |

`dynamicBankId` is **not** ported as a separate knob. Legacy's two-knob form has
an unreachable combination (`dynamicBankId = false` with no `bankId` is just the
default), so the two collapse into one: a non-empty `bank_id` means static.

`session_retention_days` is now a **parameter** to `metrics_task::tick` rather
than the constant C1 shipped, which C1's note anticipated. Two sources of truth
for 90 days — one documented in `config.example.toml` and one compiled into the
daemon — is the drift this repo keeps catching in review, and the test now
passes a non-default 7 so a `tick` that ignores its argument fails.

## Measurement

Absolute cross-session comparison is invalid on this machine: re-benching an
identical commit returned **+1.5 ms on identical bits**. So `hook_bench`
alternates `A,B,A,B…` inside one driver process, with **B = `memgarden hook
noop`** — the same binary, the same dynamic-link and page-cache state. Arm B is
the binary's fixed cost, arm A is the subcommand, and `A_i - B_i` survives a
noisy box because noise moves both arms.

A hook pays for its binary on **every** `execve`, so size and dynamic-link cost
are inside the budget in a way they never were for the daemon.

Measured on this box (release, N=300, 20 discarded warm-ups, embedded stub
daemon):

| arm | p50 ms | p95 ms | p99 ms | min ms |
|---|---|---|---|---|
| A `hook noop` | 0.242 | 0.296 | 0.324 | 0.226 |
| B `hook noop` (baseline) | 0.243 | 0.305 | 0.323 | 0.225 |
| paired A-B | 0.000 | 0.036 | 0.065 | -0.150 |

**Arm B p50 0.243 ms against the 1.5 ms gate — PASS, with 6x headroom.** That
is *below* the plan's 0.34 ms prototype, because this binary is smaller (484 KB
vs 606 KB) and `noop` loads no config. Arm B did **not** move across the review
round (0.243 both before and after the deadline, path guard and `create_new`
retry landed); the binary grew 462264 -> 496160 bytes.

The null experiment is a stronger control than "the harness works": with
A = B, any within-pair position effect δ (arm A always running first) would
appear as `paired = δ`. Measuring 0.000 at N=300 bounds δ below a microsecond,
which retires the confounder by measurement instead of assuming it away.

C2a ships only `noop`, so the default run is a **null experiment**: A = B, and
the paired delta must sit at ~0. It does (-0.000 ms). A harness that cannot
measure "no difference" cannot be trusted to measure one, so that is the right
first result for a measuring instrument, and
`identical_arms_produce_a_paired_delta_near_zero` keeps it that way.

`scripts/hook-budget.sh`, on the same build:

```
1. size    496160 bytes (0.47 MB)              <= 8 MB budget   ok   [human check]
2. ldd     linux-vdso, libgcc_s, libc, ld-linux-x86-64          ok   [human check]
           no libssl / libcrypto / libonnxruntime / libsqlite3 / libstdc++
3. tree    21 crates, diffed against an explicit allowlist               ok   [CI-WIRED]
4. LD_DEBUG  165 relocations, 7 from cache, ~75k cycles total loader time
             (diagnostic only, never a gate)
```

**Only #3 is a CI gate.** A green CI does not prove #1, #2 or #4 — they are
human PR-body checks, and saying so plainly is part of the deliverable.

#3 is an **allowlist diff, not a denylist.** The first version listed 11
forbidden crate names, which is not containment: a future `ureq`, `hyper`,
`rustls` or `libc` would have passed it green while the PR body claimed the
closure was enforced. Diffing the whole closure fails on additions *and*
removals for the same line count, and turns the crate list into something a
reviewer reads once instead of a denylist someone must remember to extend.
`ci.yml` and `scripts/hook-budget.sh` carry the same list.

The in-process transport probe (`http::post` -> stub, N=190) reports p50
0.015 ms / p95 0.019 ms. That is **not** a hook-overhead number — there is no
`execve` in it. It is reported because it separates "the transport is slow"
from "the binary is slow" when arm A eventually moves.

`--real <url>` switches the harness to a live `memgardend` for **Gate C**, which
is an AC-2 *recall-clause* number (p95 <= 70 ms end to end), not a hook-overhead
one. Labelled separately in the output so a daemon regression never reads as a
hook regression.

## Mutation evidence

The convention this repo keeps getting wrong is a correct rule that no test
pins — C1's review found `last_seen_at = excluded.last_seen_at` surviving
deletion against all 453 tests. So the rules added here were checked by
breaking them. Each mutation was reverted after the run.

| mutation | caught by |
|---|---|
| `parse_head` stops rejecting `Transfer-Encoding` | `a_chunked_reply_is_a_failure_and_not_a_mis_parse`, `head_parsing_refuses_chunked_and_lengthless_replies` |
| missing `Content-Length` defaults to `0` instead of erroring | `a_reply_without_content_length_is_a_failure` + the unit test |
| the io budget -> a hardcoded constant | `the_read_timeout_that_arrives_is_the_one_the_caller_passed`, which asserts elapsed wall time — the only way to see the value that *arrives*. **Corrected in review:** it originally made one call and asserted `[150 ms, 600 ms)`, claiming that caught a hardcoded 400 ms. It did not — 400 satisfies both bounds, and 400 is the *most likely* hardcode because it is the shipped `recall_timeout_ms`; only the 5 s mutant was caught. It now calls twice with 150 and 700 and asserts both, and the two windows are disjoint, so no constant survives |
| both `tick(rearm)?` deadline re-arms removed | `a_trickling_server_is_bounded_by_the_whole_request_deadline` — 29.71 s and `Ok`, exactly the pre-fix measurement |
| the deadline check alone (byte bounds kept) | `an_expired_deadline_short_circuits_before_the_byte_bound` |
| the `request()` path guard removed | `a_path_with_control_characters_never_reaches_the_socket` |
| `symlink_metadata` restored in `repo_root` | `a_symlinked_git_dir_still_resolves_to_the_repo` |
| the `gitdir:` `continue` back to `?` | `an_unreadable_git_entry_does_not_abandon_the_ancestor_walk` |
| `gc` filters `.json` only | `gc_collects_lock_and_temp_leftovers_too` |
| `load` drops the `session_id` conjunct | `a_filename_collision_reads_as_absent_rather_than_as_another_session` |
| `ensure_dir` back to bare `create_dir_all` | `the_state_dir_and_its_parent_are_created_0700` |
| `File::create_new` back to `File::create` | `a_planted_symlink_at_the_temp_path_is_not_followed` |
| the `session_retention_days` upper bound removed | `hooks_validation_rejects_unusable_values` |
| duplicate-`Content-Length` conflict check removed | `head_parsing_refuses_chunked_and_lengthless_replies` |
| the unbracketed-ipv6 refusal removed | `parses_loopback_urls_and_refuses_everything_else` |
| panic hook `exit(0)` -> `exit(1)` | `an_injected_panic_exits_zero_with_empty_stdout` |
| `MEMGARDEN_HOOKS_DISABLE` check deleted from `dispatch` | `the_disable_switch_exits_zero_and_stays_silent` (via the `__panic` half) |
| truthy set gains `"0"` | `the_disable_switch_matches_the_configs_truthy_set` |
| loopback host allowlist removed from `Target::parse` | `parses_loopback_urls_and_refuses_everything_else`, `the_client_refuses_to_leave_the_loopback` |
| `repo_root` stops stripping `worktrees/<name>` | `a_linked_worktree_resolves_to_the_main_repo` |
| `encode_path_segment` leaves `:` unescaped | 3 tests, incl. the request-line token count |
| `body.truncate(content_length)` removed | `body_is_bounded_by_content_length`, `a_post_round_trips_…` |
| `MAX_RESPONSE_BYTES` check removed | `head_parsing_refuses_chunked_and_lengthless_replies` |
| `MAX_HEAD_BYTES` guard removed | `an_endless_headerless_stream_does_not_grow_without_bound` — **by hanging**, which is what "no bound" means; it was killed at 60 s |
| `path_for` stops replacing `/` | `a_traversing_session_id_stays_inside_the_state_dir` |
| `SessionState::recovered` ignores `confirmed_offset` | `recovery_seeds_the_offset_from_the_confirmed_cursor` |
| `[hooks] max_post_bytes` ceiling removed | `hooks_validation_rejects_unusable_values` |
| `tick` ignores its `session_retention_days` argument | `the_metrics_tick_expires_stale_sessions` (fixture ages derive from a non-default 7) |
| `store()` writes in place instead of temp+rename | 6 state tests |

**One mutation still survives, and it is reported rather than papered over:**

**The containment re-check in `path_for` (`if path.parent() != Some(dir)`) is
pinned by nothing.** Deleting it fails no test — because the sanitizer above it
already makes the branch unreachable, which is exactly what defense-in-depth
means. It stays: it is a trust-boundary backstop, and if the sanitizer ever
stops replacing `/`, `dir.join("/etc/passwd")` returns `/etc/passwd` and this
check is what fires. Unreachable-today is not unreachable-tomorrow, and pinning
it would mean shipping a deliberately weakened sanitizer behind `cfg(test)`,
which is worse than an honest note. Only the *sanitizer* is tested, and the
mutation that removes `/` from it is caught.

The second survivor from the first round — dropping `f.sync_all()` — is gone
because the `sync_all()` is gone. See §No `fsync`: the justification was a
crash property the same comment declined to pay for, and deleting the code is a
valid way to close an unpinnable mutation.

## Diverged from legacy

* **Never exit 2.** Legacy exits 2 under `debug` (`recall.py:287-291`,
  `retain.py:283-287`), and the live config had `debug: true` until it was
  turned off on 2026-08-03. On `UserPromptSubmit` that erases the prompt. Ours
  cannot: see the table at the top.
* **No `git` subprocess for worktree resolution** (`bank.py:52-63`). Measured
  0.435 ms — more than the entire rest of the hook. A `.git` walk replaces it.
* **One state file per session**, replacing `turns.json` +
  `retention_tracking.json` + `bank_missions.json` and their global flocks
  (`state.py:95-210`). No global read-modify-write, no cross-session
  contention, no 10,000-entry truncation hack.
* **The filename cap is 200 *bytes*, not 200 characters** (`state.py:51`). The
  two agree for the ASCII uuids Claude Code actually sends and diverge
  violently otherwise: 200 Korean characters is 600 bytes, ext4's limit is 255,
  and the "capped" name is then rejected by `open` — legacy's sanitizer would
  raise on the very input it was written to make safe.
* **`dynamicBankId` is not a separate knob.** A non-empty `bank_id` means
  static mode. Legacy's two-knob form has an unreachable combination.
* **A `gitdir:` file that is not a worktree resolves to its own directory.**
  Legacy, via `--git-common-dir`, reports `modules` for a submodule (the
  basename of the parent of `<super>/.git/modules/<name>`). That is a bug, not
  a behaviour, and it is not ported. Submodules are not a shape any live bank
  has.
* **No `clap`, no `reqwest`.** Both are refused with a consequence rather than
  a preference, and the reasons are in `Cargo.toml` where the temptation is.

## Known limits and deferred items

* **`hook noop` is the only subcommand.** `session-start` (C2b), `recall` (C3),
  the transcript reader (C4a), `retain`/`session-end` (C4b) and
  `hooks install` (C5) all land later. `dispatch`'s `_` arm means an
  install that names one of them today is a silent no-op, not a failure — which
  is the correct behaviour for a binary whose contract is "never break the
  session".
* **Two session ids sharing a 200-byte prefix collide onto one state file.**
  Claude Code session ids are 36-character uuids, so this cannot fire; the cost
  if it ever did is a wrong cursor for one session, recoverable from the
  mirror.
* **`lto = "fat"` and `panic = "abort"` are out of scope** (plan §Deliberately
  out of scope). Arm B is at 0.243 ms against a 1.5 ms budget; the re-entry
  criterion is arm B's paired p50 exceeding it.
* **`SessionState`'s fields are all `pub`,** so "the wrong cursor cannot be
  reached for" is about ergonomics, not enforcement — C2b could still assign
  `offset` directly. The airtight version is to omit `byte_offset` from C2b's
  mirror-response struct entirely; a field that is never deserialized cannot be
  misused. Noted at `SessionState::recovered`.
* **`empty_and_malformed_stdin_exit_zero` does not cover `hookio`.** It drives
  `hook noop`, which never reads stdin, so it pins the exit code only. Repoint
  it at `hook session-start` in C2b.
* **The bench's stub daemon serves one connection at a time.**
  `// ponytail:` the driver is single-threaded and the hook opens exactly one
  connection per invocation; spawn per accept if a concurrent arm is ever added.
* **`retain_api::wall_timeout_fails_the_job_and_keeps_partial_progress` flakes
  under a full parallel workspace run** (twice observed) and passes 5/5 in
  isolation on the clean tree. It is the known `retain_jobs` lock flake C1's
  note already records, not something C2a introduced.

## Manual verification

```
$ cargo build --release -p memgarden-cli --bins
$ ./scripts/hook-budget.sh target/release/memgarden
   (output quoted in §Measurement — 496160 bytes, glibc-only ldd, 21-crate
    closure matching the allowlist exactly)

$ ./target/release/hook_bench --n 300 --warmup 20
   (table quoted in §Measurement — arm B p50 0.243 ms, PASS)

$ echo '{"session_id":"s1","hook_event_name":"Stop"}' \
    | ./target/release/memgarden hook noop; echo "exit=$?"
exit=0

$ ./target/release/memgarden hook __panic </dev/null; echo "exit=$?"
memgarden: panicked at crates/memgarden-cli/src/lib.rs:65:44:
injected panic (memgarden hook __panic)
exit=0

$ ./target/release/memgarden bogus </dev/null; echo "exit=$?"
exit=0
```

Nothing bound 9077 (legacy hindsight) or 9090 (memdash); every listener in the
tests and the bench binds port 0.
