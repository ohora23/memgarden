# C3 / HK-1d — `hook recall`, the circuit breaker, and the bounded injection

The first hook on the **per-prompt** path, and the only one whose stdout is
supposed to be read. Fourth PR of Phase C. `src/cmd/recall.rs`, one new
`[hooks]` key, and `state::ensure_dir` made public.

`noop` never fires in production and `session-start` fires twice a session.
This one runs before every single thing the user types, which changes what each
of the crate's existing properties is worth:

| property | why it is different here |
|---|---|
| **never exit 2** | this is the event where 2 *erases the prompt*, and the hook whose legacy counterpart actually does it (`recall.py:287-291`) |
| **empty stdout on failure** | `session-start`'s stdout is unused; here stdout is the deliverable, so "empty on every path but one" has to be a rule rather than a side effect |
| **the whole-request deadline** | C2a's `SO_RCVTIMEO` defect was measured *against `recall_timeout_ms`* — 30.007 s on a 400 ms budget, returning `Ok`. Recall is the caller that bug was about |
| **cost of the failure path** | paid once per session for `session-start`, once per prompt here |

## What it does, in order

1. **Parse stdin.** Unusable → return.
2. **Gate on the prompt** — trimmed, `< 5` **characters** → return. See below;
   this is deliberately *before* the config read.
3. **Load config; honour `[hooks] enabled`.** Bound `session_id` at 200 bytes.
4. **Load the session's state file** (unlocked — it is a cache, and `store`
   publishes by `rename`).
5. **Resolve the bank**: the state file's `bank_id` if there is one, else
   `bank::derive`.
6. **Check the circuit breaker.** Open → return, *before a socket exists*.
7. **Truncate the query** to `recall_max_query_chars` characters and
   `POST /v1/banks/{bank}/recall`.
8. **Apply the outcome to the counters**, under the lock, writing only if
   something changed.
9. **Emit**: `full` prints the envelope, `shadow` appends one JSONL line, both
   write `last_recall.json`.

## The three things that go out, and the one bound that governs all of them

`injected_text` arrives daemon-built and already defanged by tag-name prefix
(Phase B `defang`), so the hook's remaining question is not *what is in it* but
*how much of it there is*. `max_inject_bytes` (64 KB) is checked **once**,
before the mode branch, and a payload over it is refused rather than truncated:

* nothing on stdout — half an injection is a worse thing to hand a model than
  none, and a truncated `<memgarden_memories>` block has no closing tag;
* nothing in the shadow log either, so a runaway daemon cannot fill the disk at
  the same rate it would have filled the context;
* `last_recall.json` records `status: "oversize"` with the byte count, because
  a silent refusal is the same failure shape as a silent injection.

`an_injection_over_max_inject_bytes_is_refused_rather_than_truncated` pins both
sides: 100 bytes against a 64-byte ceiling emits nothing, and the same 100 bytes
against a 100-byte ceiling emits. The test values are 64 and 100 rather than the
shipped 65536 **because a bound test whose numbers are the defaults cannot fail
on a hardcoded default** — the mistake C2a's review found in its own timeout
test.

### `full` stdout

One line of compact JSON:

```json
{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"<memgarden_memories>…"}}
```

Written with `writeln!` and not `println!`. `println!` **panics** when the write
fails; that panic would reach the hook in `main` and exit 0 having flushed
nothing, which is the right outcome reached by accident. Stdout is a
`LineWriter`, so the `\n` is also the flush — the line is either fully handed
over or not written at all, and the panic hook's `exit(0)` cannot truncate one
that was.

### `shadow` stdout is empty, and that is the whole feature

Shadow is the default install mode (§Binding decisions #13) and the AC-1
instrument: the daemon is really called, with real latency and real
`/metrics.json` counters, and the model sees nothing.
`shadow_mode_emits_nothing_and_appends_exactly_one_jsonl_line` asserts the stub
recorded an accept, so "shadow" cannot quietly become "skip the daemon".

`<data>/hooks/shadow-recall.jsonl`, one object per injected recall:

```json
{"ts":1785778630488,"session_id":"…","bank_id":"gold","prompt_chars":26,
 "returned":11,"tokens":915,"injected_text":"<memgarden_memories>…"}
```

`ts` is epoch **milliseconds**, matching `now_ms()` and every timestamp already
in the state file, rather than legacy's `time.strftime` ISO string
(`recall.py:263`) — the consumer is an A/B analysis, and one unit across the two
files it joins beats matching a format nothing else here uses.

Rotation is one generation at `shadow_log_max_bytes`: at or over the ceiling the
file is renamed to `shadow-recall.jsonl.1`, replacing any previous one. Retained
history is therefore between one and two times the ceiling. `// ponytail:` a
numbered ladder if an AC-1 run ever needs more than 64 MB ≈ 40k prompts.

### `last_recall.json`, written on **every** path that reached the daemon

Legacy's `LAST_RECALL_STATE` (`recall.py:261-269`) is written only on the inject
path, so it can answer *why did it inject that* and never *why did it inject
nothing* — which is the question people actually ask. Ours is written on the
rejected, transport-failure and bad-url paths too, at no extra branch (it is one
call at the end of `emit`), and it carries the two numbers that explain a silent
hook:

```json
{"ts":…,"mode":"full","status":"transport_failure","http_status":0,
 "session_id":"…","bank_id":"gold","prompt_chars":26,"returned":0,"tokens":0,
 "injected_bytes":0,"injected_text":null,
 "transport_failures":0,"breaker_open_until_ms":0}
```

`http_status` is separate from `status` because `404` (no bank yet) and `400`
(a query the daemon refused) are different answers, and collapsing them into
`rejected` throws away the half that says which.

The one path that writes **nothing** is the gated one: an open breaker returns
before any file is touched, which is what keeps that path at the measured
0.303 ms. The consequence is that `last_recall.json` is then stale — mitigated
by `breaker_open_until_ms` being *in* it, so the stale record says why it is
stale.

## The circuit breaker

Three consecutive `transport_failures` (connect refused, timeout, unparseable
2xx) set `breaker_open_until_ms = now + breaker_cooldown_secs`. While open the
hook returns at step 6, **before `TcpStream::connect`**. Any success clears both
the counter and the window. Nothing else moves either: not a 4xx, not a 503, not
a bad `daemon_url`.

`three_failures_open_the_breaker_and_the_fourth_prompt_opens_no_socket` asserts
this with a stub that counts **accepts, not requests**, and it points the gated
invocation at a *healthy* stub — so what is asserted is "the socket was never
opened", not "no work was done", and the breaker is shown to be a property of
the session rather than of the address. A hook that connects and then decides
not to ask has already paid the connect; a log line saying it skipped is
evidence of nothing.

### Both edges of the window are guarded

```rust
now_ms < until && until <= now_ms.saturating_add(cooldown_ms(cfg))
```

The second conjunct is the one that matters. `breaker_open_until_ms` is read
from a file, and a value far enough in the future turns "skip for 60 s" into
"never recall again for this session" — silently, on the hook whose failure is
invisible by design. **No attacker is required**: an NTP step, a VM resume or a
dual-boot RTC produces one, and C2b hit the identical shape on `poisoned_at`.
Anything more than one cooldown ahead cannot have been written by the arm above,
so it reads as closed. Pinned at the unit level at all four boundaries
(`now`, `now+1`, `now+cooldown`, `now+cooldown+1`) and end to end with
`i64::MAX` through the real binary.

### Recall never poisons, so the §Failure posture table has one arm here, not four

`reject_failures` exists to stop a *cursor* advancing past bytes the daemon
durably refused. Recall advances nothing, so 400, 404, 503 and 500 are one arm
in this file — they move no counter, emit nothing, and are distinguishable only
in `last_recall.json`. `a_rejected_recall_moves_no_counter_whatever_the_status`
still exercises all four separately, because "they are the same today" is a
property worth having a test for rather than an argument for not writing one.

## Diverged from **the plan**

### 1. The bank is the session's, not a fresh derivation

The plan does not say which, and deriving on every prompt is the obvious
reading. It is wrong for a case C2b already decided: `session-start`
deliberately does **not** refresh a session's stored `bank_id`, because a
cursor belongs to the bank its bytes were posted to — so after a `resume` from a
different cwd, or an edited `directory_bank_map`, `bank::derive` and the stored
id disagree. Deriving here would recall from a bank this session has never
written a byte to, and the failure is **silent**: an empty result set is
indistinguishable from "nothing matched".

So: stored `bank_id` when there is a state file, `bank::derive` when there is
not. `the_bank_comes_from_the_session_state_and_is_only_derived_when_absent`
pins both halves.

### 2. A successful recall writes no state file when there was none

The plan's failure posture has recall clearing `transport_failures` on success,
which reads as "write the state file". Taken literally on a session whose state
file is absent, that *creates* one — and a state file that exists is a state
file `session-start` will not rebuild from the daemon's mirror (C2b), so the
success path would quietly disable the wiped-state-dir recovery.

The write is therefore against a **baseline**: build the state that would exist,
apply the outcome, and store only if the two differ. Healthy steady state → no
state I/O at all, which is also the right answer for a per-prompt path. The
failure path still creates the file, because the breaker has nowhere else to
live. Both are tested (`a_successful_recall_writes_no_state_when_there_was_none`
and the daemon-down test's `transport_failures == 1`).

### 3. `recall_max_query_chars` is a new `[hooks]` key, and its upper bound is arithmetic

The plan names the value (800, `lib/config.py:19`) but the key is not in C2a's
`[hooks]` list. Added, in **characters** — legacy's unit and the daemon's unit
for `MIN_QUERY_CHARS`.

`validate` caps it at **2048**, and the number is derived rather than chosen: a
`char` is at most 4 UTF-8 bytes and the daemon refuses a query over
`MAX_QUERY_BYTES` (8 KB, `routes/recall.rs`), so 2048 is the largest character
budget that *cannot* produce a 400. Above it a hook would 400 on every prompt
from a config that reads as perfectly reasonable. Same shape as
`max_post_bytes ≤ DAEMON_MAX_BODY_BYTES`, with a unit conversion in the middle —
which is the part that would be easy to get wrong twice.

### 4. The prompt gate runs before the config read

`session-start` reads config first and then validates its payload. Recall
reverses it: the length gate is free, one-word turns (`ok`, `yes`, `네`) are
common, and a TOML parse to discover we are going to do nothing is pure cost on
the hottest path in the system. Correct in every case, because
`[hooks] enabled = false` and a 3-character prompt have the identical
observable — nothing at all.

## Diverged from legacy

* **Never exit 2.** `recall.py:287-291` exits 2 whenever `debug` is set, and on
  `UserPromptSubmit` that erases what the user typed. Inherited from C2a, and
  this is the hook it was written for: `[hooks] debug` here only ever adds a
  stderr line.
* **The gate counts characters, and so does the truncation.** Both are `len()`
  in Python, which is characters; both would be bytes in Rust. The two diverge
  violently on the Korean this system is measured against — `안녕하` is 3
  characters and 9 bytes, and `&s[..800]` on a Korean prompt is not merely short,
  it **panics** on a non-boundary index, inside the process whose entire contract
  is that it cannot fail loudly.
* **No client-side bank mission.** Legacy calls `ensure_bank_mission` on every
  recall (`recall.py:157`), memoized through `bank_missions.json` and its
  10,000-entry truncation hack. C2b already replaced that with one idempotent
  `POST /v1/banks` per session; recall does not create or touch banks at all, and
  a 404 is simply an empty turn.
* **No multi-turn query composition.** `recallContextTurns` > 1 makes legacy read
  the transcript and compose the query from the last N turns
  (`recall.py:160-170`). Not ported: the default is 1, the live coding preset
  does not raise it, and it would put a transcript read on the per-prompt path.
  Recorded as an open question, not a decision — if AC-1 shows recall quality
  needs it, C4a's reader is already the right instrument and the plumbing is one
  call.
* **`recallTags` / `tagGroups` / `recallAdditionalBankFilters` are not sent.**
  The daemon accepts `tags`/`tagsMatch`; the fork's config sets none of them, so
  sending empties would be three fields of noise in every request body.
* **The diagnostic is written on failures too.** See `last_recall.json` above.
* **Nothing is spooled, retried or backed off in-process.** A failed recall is
  a turn with no memories, and the next prompt is the retry.

## Measurement

Release build, N = 300, 20 discarded warm-ups, embedded stub daemon, hermetic
`MEMGARDEN_CONFIG` / `HOME` / `XDG_DATA_HOME`, arm B given the same stdin as arm
A. Every listener binds port 0 or 9111; 9077 (hindsight) and 9090 (memdash) were
live throughout and never touched.

### Gate A — hook overhead, against the stub

| arm | p50 ms | p95 ms | p99 ms | min ms |
|---|---|---|---|---|
| A `hook recall` | 0.439 | 0.504 | 0.550 | 0.401 |
| B `hook noop` (baseline) | 0.287 | 0.331 | 0.350 | 0.249 |
| paired A−B | **0.158** | 0.224 | 0.259 | 0.063 |

**0.158 ms of own work against the 10 ms per-hook budget**: stdin parse, config
load, bank derivation, a loopback POST, a 1.8 KB response parse, the stdout line
and the diagnostic write. Arm B 0.287 ms against its 1.5 ms gate: **PASS**.

### The broken paths, which are the ones that must cost nothing

| condition | arm A p50 | paired A−B | what it exercises |
|---|---|---|---|
| stub healthy | 0.439 | 0.158 | the injection path above |
| **daemon down** (ECONNREFUSED) | 0.438 | 0.160 | connect refused + a state write |
| **breaker open** | **0.303** | **0.040** | stdin, config, one state read, return |

Daemon-down is indistinguishable from healthy, which is not a coincidence: a
loopback `ECONNREFUSED` costs about what a stub round trip costs, and the down
path spends what it saves on the state write the healthy path skips. The gated
path is 0.040 ms of own work — the number the breaker exists to produce, and
the reason a wedged daemon costs `3 × 400 ms` per cooldown instead of 400 ms per
prompt.

The gated run made 320 invocations and left `transport_failures` at exactly 3:
the gated path writes nothing, asserted here and in the integration test.

### Gate C — against the live daemon, reported beside Gate A and not instead of it

`memgardend` release, embeddings **on** (bge-small, model cache pre-warmed),
Ollama up, `schema_version: 7`, on **127.0.0.1:9111**. Bank `gold` seeded from
`gold/corpus.jsonl` via `recall_bench import`: 2,718 nodes, 2,129 entity rows,
54,012 temporal links. Query: `JWT 로그인 세션 풀리는 버그 어떻게 고쳤지?`
(`gold/queries.jsonl` q01).

| arm | p50 ms | p95 ms | p99 ms | min ms |
|---|---|---|---|---|
| A `hook recall` (live daemon) | 8.048 | **9.858** | 11.674 | 7.644 |
| B `hook noop` | 0.362 | 0.452 | 0.640 | 0.288 |

**Gate C p95 = 9.86 ms against the ≤ 70 ms gate — PASS with 7× headroom.**

And the split, which is the reason this number is reported *beside* arm B rather
than instead of it. The daemon's own `/metrics.json` over the same 321 requests:

```
recall_latency: p50 7.588 ms  p95 9.904 ms  max 12.995 ms
                under_35ms 321/321   under_60ms 321/321
recall_errors: 0
```

So essentially all of Gate C is the callee. The hook's own contribution is the
0.158 ms in the table above, and a future Gate C regression should be read
against `recall_latency` first — conflating the two is exactly how a daemon
regression comes to read as a hook regression.

### Did arm B move? Measured, not inferred

C2b recorded arm B at 0.327 (0.311 on its null control); this run reads 0.287.
C2b's own note is the reason not to attribute that difference: cross-session
comparison on this box is invalid (+1.5 ms measured on identical bits), and its
first draft over-attributed a similar gap by **5×** until it ran the control.

So the control was run. C2b's binary was rebuilt from `df16a86` in this session —
**1,390,184 bytes, byte-identical to the size its note quotes** — and paired
against this one inside one driver, both arms `hook noop`, alternating:

| run | A (C3 binary) | B (C2b binary) | paired A−B |
|---|---|---|---|
| 1 | 0.266 | 0.267 | **−0.001** |
| 2 | 0.265 | 0.266 | **−0.001** |
| 3 | 0.263 | 0.263 | **0.001** |

**This binary's growth costs 0.000 ms**, stable across three runs of N = 300.
Every bit of the apparent −0.04 ms move in arm B is cross-session drift.

That is the *opposite* result to C2b's +0.015 ms, and the mechanism explains
both: C2b's growth was `Config::load` linking the TOML parser for the first
time — 496 KB → 1,390 KB and **165 → 220 relocations**. C3 adds 1,457,936 bytes
(+67,752) and **220 → 221 relocations**, because everything it adds is code
`hook noop` never reaches. Binary size is not the variable; the relocation count
is, and one relocation is not measurable.

`scripts/hook-budget.sh`:

```
1. size    1457936 bytes (1.39 MB)            <= 8 MB budget   ok   [human check]
2. ldd     linux-vdso, libgcc_s, libc, ld-linux-x86-64          ok   [human check]
           no libssl / libcrypto / libonnxruntime / libsqlite3 / libstdc++
3. tree    21 crates, unchanged from C2a/C2b, diffed against the allowlist [CI-WIRED]
4. LD_DEBUG  221 relocations, 7 from cache, ~65k cycles loader time  [diagnostic]
```

**Only #3 is a CI gate.** #1, #2 and #4 are human PR-body checks.

## Manual verification

Daemon and bank as in Gate C above.

```
$ memgarden hook recall < manual.json            # mode = "full", daemon up
exit 0, stdout 3118 bytes, exactly 1 line, valid JSON
  hookSpecificOutput.hookEventName = "UserPromptSubmit"
  additionalContext = "<memgarden_memories>\n\nCurrent time - 2026-08-03 17:35 UTC\n\n- JWT …"

$ memgarden hook recall < manual.json            # mode = "shadow", daemon up
exit 0, stdout 0 bytes
  shadow-recall.jsonl: 1 line
  {"ts":1785778630488,"session_id":"c3-manual-0001","bank_id":"gold",
   "prompt_chars":26,"returned":11,"tokens":915,"injected_text":<1812 bytes>}

$ kill <memgardend>; memgarden hook recall < manual.json   # the same command
exit 0, stdout 0 bytes, stderr 0 bytes
  state:        transport_failures 1, breaker_open_until_ms 0, bank_id "gold"
  last_recall:  status "transport_failure", http_status 0, injected_text null
```

The bank in all three is `gold` **from the state file**, not re-derived — the
divergence above, demonstrated: the state file was created by the daemon-down
run and the two later runs read their bank out of it.

`transport_failures` went 1 → 0 on the successful shadow run: any success clears
the breaker.

## Mutation evidence

**33 mutations, applied one at a time by a script and reverted after each
run** (`scripts/` has no home for it; it lived in the session scratchpad, and
the table below is its output). **30 caught, 3 survive and are named.** C2a ran
13 + 16, C2b ran 32 / caught 29.

Two of the three survivors were *predicted* — written to check the prediction
rather than to pass. **The third was not, and it is the useful one.**

| mutation | caught by |
|---|---|
| the `[hooks] enabled` check deleted | `the_config_switch_makes_no_request_and_writes_nothing` |
| the `MAX_SESSION_ID_BYTES` bound removed | `an_unusable_session_id_writes_nothing_and_makes_no_request` |
| `Target::parse`'s early return → `Transport` | `a_non_loopback_daemon_url_is_not_counted_as_a_transport_failure` |
| the `< MIN_PROMPT_CHARS` gate deleted | `a_prompt_under_five_characters_makes_no_request` |
| `MIN_PROMPT_CHARS` 5 → 4 | the same test's `"four"` row |
| the gate counts `.len()` instead of `.chars().count()` | `the_prompt_gate_counts_characters_not_bytes` |
| `usable_prompt` drops the `user_prompt` fallback | `the_user_prompt_spelling_is_accepted` |
| `usable_prompt` returns the untrimmed string | `either_prompt_spelling_is_accepted_and_short_ones_are_refused` |
| `truncate_chars` uses `&s[..max]` (bytes) | `the_query_is_truncated_to_the_configured_number_of_characters` — **by panicking** on a non-boundary index, which is what "bytes" means for Korean |
| truncation hardcoded to 800 | the same test (configured 7) |
| truncation removed entirely | the same test |
| `breaker_open`'s upper conjunct removed | `a_far_future_breaker_stamp_does_not_wedge_recall_off_forever`, `the_breaker_is_open_only_inside_a_window_it_could_have_written` |
| `now_ms < until` → `<=` | the unit test's `until == now` row |
| the breaker check moved *after* the request | `three_failures_open_the_breaker_and_the_fourth_prompt_opens_no_socket` (accepts 1, not 0) |
| the breaker never opens (`>=` → `>` on `breaker_failures`) | the same test |
| the success arm stops clearing `transport_failures` | the same test's recovery half, `an_empty_recall_clears_the_breaker_and_logs_nothing` |
| `Outcome::Rejected` folded into the `Transport` arm | `a_rejected_recall_moves_no_counter_whatever_the_status` (all four statuses) |
| an unparseable 2xx treated as `Rejected` | `an_unparseable_two_hundred_is_a_transport_failure` |
| the `max_inject_bytes` check removed | `an_injection_over_max_inject_bytes_is_refused_rather_than_truncated` |
| `>` → `>=` on that check | the same test's at-the-ceiling half |
| the oversize payload truncated instead of refused | the same test (`injected_bytes == 0`) |
| `open_regular`'s `symlink_metadata` guard removed | `a_planted_symlink_is_refused_by_both_open_modes` |
| `full` mode writes the shadow line too | `full_mode_emits_one_line_of_the_documented_envelope` |
| `shadow` mode writes stdout | `shadow_mode_emits_nothing_and_appends_exactly_one_jsonl_line` |
| `hookEventName` → `"UserPrompt"` | `full_mode_emits_one_line_of_the_documented_envelope` |
| the shadow log truncates instead of appending | the same test's two-prompt half |
| the rotation check removed | `the_shadow_log_rotates_at_its_configured_ceiling` |
| `maxTokens` sent as `budget` (the collapse CE-6 refused) | `the_request_carries_budget_and_max_tokens_as_separate_knobs` |
| the stored `bank_id` ignored, always derived | `the_bank_comes_from_the_session_state_and_is_only_derived_when_absent` |
| the `st != baseline` guard → always store | `a_successful_recall_writes_no_state_when_there_was_none` |

### The three survivors, one of which was not predicted

**1. `fetch`'s `Err(HttpError::Url(_))` arm — the unpredicted one.** Folding it
into `Transport` fails **nothing**, and
`a_non_loopback_daemon_url_is_not_counted_as_a_transport_failure` does *not*
cover it, which is the finding: there are **two** sites that produce
`Outcome::Config` and the test only reaches the first. A bad `daemon_url` is
caught by `Target::parse` at the top of `fetch` and returns before the POST, so
the arm below it can only fire on a bad **path** — and `encode_path_segment`
cannot emit a byte `http::request`'s guard rejects. The arm is therefore
unreachable today, exactly like C2a's `path_for` containment re-check, and it is
kept for the same reason: it is what fires if the path construction ever
changes, and a code bug opening the circuit breaker would look exactly like an
outage in `hooks status`. The reachable site **is** pinned — mutating
`Target::parse`'s early return to `Transport` is caught. Named here rather than
deleted or fake-tested.

**2. `open_regular`'s `create_new` on the absent-file branch** (predicted).
Replacing it with a plain `create(true)` fails no test, because the
`symlink_metadata` above it has already refused every link the tests can plant —
the `create_new` is only reachable in the window *between* the `lstat` and the
`open`. That is what defense-in-depth against a TOCTOU means, and pinning it
would require deliberately winning a race inside a test. The `lstat` itself
**is** pinned, in both open modes.

**3. `Outcome::http_status` returning `0` for `Recalled`** (predicted). Changing
it to `200` fails nothing, because no test asserts `http_status` on a success
path. It is a diagnostic field with no behavioural consumer; asserting it
everywhere would be pinning a log format.

## Known limits and accepted risks

### The gated path's diagnostic is stale by design

An open breaker returns before `last_recall.json` is written, so during a
cooldown the file describes the last invocation that *reached* the daemon. That
is what keeps the gated path at 0.040 ms of own work, and the file carries
`breaker_open_until_ms`, so the stale record explains its own staleness. The
alternative — a write per gated prompt — buys a fresher timestamp for a number
that has not changed.

### `shadow-recall.jsonl` records only the prompts that produced text

Per the plan: the line is appended on a 200 with a non-empty `injected_text`.
So the log answers "what would MemGarden have injected" and **not** "how often
would it have injected nothing", which an AC-1 analysis might also want. The
denominator is available from the daemon's `/metrics.json` `recall_requests`
over the same window, which is why this was left alone rather than doubling the
log's volume. Stated so it is not discovered during the analysis.

### A pinned `[hooks] bank_id` does not take effect until the next session

The bank comes from the state file when one exists (divergence 1), so an
operator who sets `bank_id` mid-session keeps recalling from the session's
original bank. That is deliberate: retain (C4b) will use the stored bank for the
same reason, and a recall/retain disagreement — recalling from a bank nothing is
writing to — is a worse failure than a knob that takes a session to apply.

### `with_lock` opens a path it did not create

Unchanged from C2b and now on the per-prompt path: `create_new` at 0600 falling
back to a **read-only** `File::open`, so a planted `sX.lock` symlink is flocked
but never written. `// ponytail:` `O_NOFOLLOW` needs `libc`, which the
CI-enforced dependency closure refuses.

### The two new files under `<data>/hooks` are opened by an `lstat`-then-open

`shadow-recall.jsonl` and `last_recall.json` are the first files this crate
writes that are neither the state file nor its lock. `File::create` was not an
option — it follows a symlink and truncates the target, which C2b **measured**
on the lock file — and `append(true)` is no better, since writing *through* a
link is the risk, not truncating. So `open_regular` lstats first, refuses
anything that is not a regular file, and `create_new`s when absent. The residual
TOCTOU window needs write access to a 0700 directory, at which point the state
files are already writable. Accepted, with the mutation survivor above.

### A `503` is indistinguishable from a `400` in this hook's behaviour

Both move nothing and emit nothing. That is correct for recall and **will not
be** for retain, where 503 is transport-class and other 4xx are poisoning.
`last_recall.json`'s `http_status` is what keeps the distinction observable
here; C4b needs the distinction to be *behavioural*.

### The daemon's short-query short-circuit is now double-guarded

`recall/mod.rs:189` returns an empty result under 5 characters and the hook
never sends one, so the daemon's guard is unreachable from this caller. It stays
on the daemon side because the hook is not the only possible client, and the two
use the same unit and the same number — a divergence between them would show up
as a wasted round trip, not as a wrong answer.

### `recallContextTurns` is not ported

See §Diverged from legacy. Recorded as an open question with its cost (a
transcript read on the per-prompt path), not as a decision that it is worthless.
