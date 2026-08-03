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
prompt**". On `Stop` it prevents the turn from ending. Legacy has this live:
`recall.py:287-291` exits 2 when `debug` is set, and `debug: true` is in the
user's real `~/.hindsight/claude-code.json` — so any unhandled exception in the
legacy recall hook deletes what the user typed. It is the single strongest
argument for the cutover, and MemGarden makes it structurally impossible rather
than carefully avoided:

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
| A `hook noop` | 0.243 | 0.285 | 0.317 | 0.227 |
| B `hook noop` (baseline) | 0.243 | 0.296 | 0.316 | 0.226 |
| paired A-B | -0.000 | 0.032 | 0.059 | -0.105 |

**Arm B p50 0.243 ms against the 1.5 ms gate — PASS, with 6x headroom.** That
is *below* the plan's 0.34 ms prototype, because this binary is smaller (462 KB
vs 606 KB: no `toml` in the CLI's own link path) and `noop` loads no config.

C2a ships only `noop`, so the default run is a **null experiment**: A = B, and
the paired delta must sit at ~0. It does (-0.000 ms). A harness that cannot
measure "no difference" cannot be trusted to measure one, so that is the right
first result for a measuring instrument, and
`identical_arms_produce_a_paired_delta_near_zero` keeps it that way.

`scripts/hook-budget.sh`, on the same build:

```
1. size    462264 bytes (0.44 MB)              <= 8 MB budget   ok   [human check]
2. ldd     linux-vdso, libgcc_s, libc, ld-linux-x86-64          ok   [human check]
           no libssl / libcrypto / libonnxruntime / libsqlite3 / libstdc++
3. tree    memgarden-core, serde, serde_json, thiserror, toml, winnow, itoa,
           memchr, zmij, serde_spanned, toml_datetime/parser/writer      ok   [CI-WIRED]
4. LD_DEBUG  165 relocations, 7 from cache, 61743 cycles total loader time
             (diagnostic only, never a gate)
```

**Only #3 is a CI gate.** A green CI does not prove #1, #2 or #4 — they are
human PR-body checks, and saying so plainly is part of the deliverable.

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
| `set_read_timeout(timeouts.io)` -> a hardcoded 5 s | `the_read_timeout_that_arrives_is_the_one_the_caller_passed` (asserts elapsed wall time, which is the only way to see the value that *arrives*) |
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

**Two mutations survived, and both are reported rather than papered over:**

1. **The containment re-check in `path_for` (`if path.parent() != Some(dir)`)
   is pinned by nothing.** Deleting it fails no test — because the sanitizer
   above it already makes the branch unreachable, which is exactly what
   defense-in-depth means. It stays (three lines, and it is what survives
   someone later "improving" the sanitizer), but it is honest to record that
   only the *sanitizer* is tested. Making it testable would mean shipping a
   deliberately weakened sanitizer behind `cfg(test)`, which is worse.
2. **Dropping `f.sync_all()` from `store()` fails no test.** Also expected:
   durability across a power cut is not observable from inside a test process.
   It stays because it is what makes the rename publish complete bytes rather
   than a filename pointing at an empty inode. The containing *directory* is
   deliberately **not** fsynced — this file is a cache, so surviving a power cut
   is not worth a second sync on the per-turn path.

## Diverged from legacy

* **Never exit 2.** Legacy exits 2 under `debug` (`recall.py:287-291`,
  `retain.py:283-287`) and `debug` is true in the live config. On
  `UserPromptSubmit` that erases the prompt. Ours cannot: see the table at the
  top.
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
* **The bench's stub daemon serves one connection at a time.**
  `// ponytail:` the driver is single-threaded and the hook opens exactly one
  connection per invocation; spawn per accept if a concurrent arm is ever added.
* **`retain_api::wall_timeout_fails_the_job_and_keeps_partial_progress` flaked
  once** during a mutation run and passes on the clean tree. It is the known
  `retain_jobs` lock flake C1's note already records, not something C2a
  introduced.

## Manual verification

```
$ cargo build --release -p memgarden-cli --bins
$ ./scripts/hook-budget.sh target/release/memgarden
   (output quoted in §Measurement — 462264 bytes, glibc-only ldd, clean tree)

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
