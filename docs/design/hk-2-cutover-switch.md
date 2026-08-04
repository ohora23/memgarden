# HK-2 (C5) — the cutover switch

`memgarden hooks install | uninstall | status`. The last Phase C PR: the four
hook subcommands built over C1–C4b become a thing a user can turn on, and — the
part this PR is actually about — a thing they can turn back off without losing
a byte of a file they share with four other tools.

Branch `feat/hk-2-cutover-switch`. **706 tests in the workspace**, 237 of them
in `memgarden-cli` — this PR adds 44: 25 `settings` unit tests, 3 `cmd::hooks`
unit tests, and 16 in `tests/hooks_install.rs` — plus a 1,000-case property
check run against the built binary.

---

## The one decision everything else follows from

`~/.claude/settings.json` is **edited by textual line splice and never
reserialized**.

`serde_json` in this workspace has no `preserve_order` — `Cargo.lock`'s
`serde_json` dependency list is exactly `itoa, memchr, serde, serde_core,
zmij`, no `indexmap`. Its `Map` is therefore a `BTreeMap`, and **any** `Value`
round-trip re-emits the whole document with its keys sorted. The user's real
file is not sorted:

```
hooks, statusLine, enabledPlugins, extraKnownMarketplaces, tui, voice,
skipDangerousModePermissionPrompt, skipWorkflowUsageWarning, voiceEnabled
```

and it is shared: Orca's eleven hooks, the statusline command, the plugin
registry, and the marketplace list all live in it. Rewriting every byte of that
file is the highest-damage operation this phase performs, and it would happen
on the **install** path — the first command a user runs and the one they trust
least.

Turning the feature on is worse, not better: Cargo feature unification would
apply `preserve_order` to `memgardend` in any workspace build, changing key
order in every API response and every stored JSON blob.

So `serde_json` here **validates and locates**, and never produces output
bytes:

* `from_str` proves the file parses before we touch it, and proves the spliced
  result still parses before we write it;
* a ~120-line string-aware forward scan finds the byte offset to insert at;
* install inserts exactly one line; uninstall deletes exactly that span.

That narrowing is what makes *"uninstall restores the file to its pre-install
bytes"* a test that can actually pass. General textual JSON editing would not
be less code than `Value` surgery. Insert-one-line / delete-one-line is.

### The three insertion shapes

| the file has | inserted after | chunk |
|---|---|---|
| `hooks.<Event>` array | its `[` | `{"hooks":[…]},` |
| `hooks`, no event key | the `hooks` `{` | `"<Event>": [{…}],` |
| neither | the document's `{` | `"hooks": {"<Event>": [{…}]},` |

Inserting immediately after an opening bracket means there is no matching-`]`
to search for, and ordering relative to other tools' entries within the same
event array is not a semantic anyone depends on.

Uninstall does not delete *a line*. It deletes **the newline and indent we
added plus the chunk**, found by a balanced scan from the chunk's first byte.
The difference is load-bearing: the user's real `Stop` array could be written
on one line, in which case our chunk and their entry share a line and a
line-wise delete takes their hook with it.

### The duplicate key — found by property-testing the built binary

A settings.json with **two `"hooks"` keys** installed "successfully" and wired
**zero hooks**. Duplicate keys are last-wins for every parser involved, but a
forward scan finds the *first* member, so the splice landed in a member Claude
Code discards. That is the one outcome worse than an error: `install` reported
four entries written and `hooks status` agreed that none were wired. Neither
was lying; they were describing different halves of the same file.

`find_member` now scans the whole object even after a match and refuses on a
second occurrence, at **both** levels — `"hooks"` and the event — naming the
key and saying why an edit to the first one would be ignored.

Found by a property check over **1,000 generated settings.json files**: nested
containers, escaped quotes and backslashes, unicode escapes, Korean, `{`/`[`/
`"hooks"` inside string *values*, single-line arrays, empty arrays, absent
keys, tabs, CRLF and four indentation styles. Each is installed, re-installed
and uninstalled through the real binary, asserting that the result still
parses, that every pre-existing entry survives untouched, that a second install
writes nothing, and that uninstall restores the bytes exactly. **1,000/1,000
after the fix.**

### What review found, and what it changed

Three-way review (functional / security / code) plus the property check above.
Nine findings changed code; every one of them is in the tests.

**Two HIGH, and they were the same asymmetry.** Removal was driven by a literal
byte search over a file four other tools rewrite, while everything else in the
command reasons about the *parsed* document. That made `uninstall`
simultaneously too weak and too strong:

* **Too weak.** `MARKER` is `"statusMessage":"memgarden: ` with no space, and
  every JSON serializer emits `": "`. One reformat by Claude Code's own
  `/hooks` editor, `jq`, prettier or a format-on-save and all four markers were
  gone — so `uninstall` printed *"nothing to remove"*, exited 0, and the hooks
  kept firing while `status` reported all four events wired. The same binary
  contradicted itself in two adjacent commands. Now the scan is
  whitespace-tolerant across the colon, **and** the two views are reconciled: a
  parse that still shows entries after the removal pass is
  `SettingsError::Residual`, an error, not a quiet success.
* **Too strong.** `chunk_span` accepted any line starting with `{` or `"`, so
  an event member holding `[{another tool}, {ours}]` on one line was deleted
  **whole** — someone else's hook gone, reported as `removed 1 entries`, exit
  0. `validate` cannot see it; the result is valid JSON. Two guards now: the
  line's key must be one we could have written (`hooks` or one of the four
  events), and the span must contain exactly one `"command":` member. Anything
  else is refused, and the residual check turns the refusal into a message.

The rest, each with its test:

| finding | change |
|---|---|
| duplicate `"hooks"` key → install wired zero and said four | `find_member` scans the whole object and refuses on a second occurrence, at both levels |
| the temp file was created at `0666 & ~umask` and the mode copied *after* the content | `0600` at `create_new`, the owner's mode applied **before** the write; a file we create stays 0600 rather than taking the umask |
| `rename` replaced a **symlinked** settings.json with a regular file, orphaning a dotfiles copy | the final component is canonicalized first, so the write lands on the real file and the link survives |
| `backup` used `fs::copy`, which follows a symlink at the destination and truncates it | `create_new` at 0600 with a counter for same-millisecond collisions — the convention `state.rs` already documents being bitten by |
| backups lived in the state dir, which `state::gc` prunes by `*.json` | they moved to `<state_dir>/backups/`; `gc` does not descend |
| `state_dir()` fell back to `"."`, dropping a copy of settings.json into the cwd **and chmod 0700'ing it** | it returns `Option` and the write is refused instead |
| `unconfirmed` probed 10 sessions in `read_dir` order while claiming "most recent" | sorted by state-file mtime; the cap is stated in the output |
| a moved binary read as healthy — `wired_events` matches the marker, not the path | `status` stats the wired `command` and prints `STALE` with the repair |
| `hooks <anything>` was exempt from `MEMGARDEN_HOOKS_DISABLE` and could exit 1 | only `install|uninstall|status` route here; everything else keeps the silent-zero arm |
| a missing flag value silently defaulted — `--settings` fell back to the **real** `~/.claude/settings.json` | a flag whose value is missing is a usage error |
| an unresolvable entry aborted the removal loop, hiding every later one | the scan steps past it and the residual check reports what is left |
| LF inserted into a CRLF file left mixed endings, which invites the reformat that triggered the first HIGH | the file's own line ending is reused, and the deletion takes the `\r` with the `\n` |

Two findings were **declined**, with reasons rather than silence: warning when
the binary lives somewhere group-writable (advisory, and false-positive prone),
and rejecting `--clear-poison` on `install`/`uninstall` (cosmetic).

### Three guards that are not the happy path

**The empty-container comma.** `[x,]` is not JSON. `trailing_comma` peeks past
the bracket and omits it when the container is empty — which the fixture
exercises through an empty `"PreCompact": []` and through a bare `{}`.

**`serde_json` found it, the scanner did not.** Only possible for an escaped
key spelling (`"hooks"`). The scanner returns `Unlocatable` and the
command refuses, because the alternative is inserting a **second** `"hooks"`
member — and last-wins duplicate keys would silently disable every hook in the
first one.

**The key is there and holds the wrong thing.** A `"hooks"` that is not an
object, or an event that is not an array, is `WrongType` — split out from
`Unlocatable` because the shared message was actively misleading: it blamed an
escaped key name for a file whose key is spelled perfectly and simply holds a
string.

A hand-written edge-case pass confirmed every refusal is **safe**: a BOM,
JSONC-style comments, an escaped `hooks` key, wrong-typed values, a root array
and an empty file are each declined with nothing written and the file
unchanged.

---

## Diverged from legacy

| Divergence | Why |
|---|---|
| **`settings.json` edited by line splice, never reserialized** | Above. Legacy has no installer at all — its entries were placed by hand, which is also why the file is unsorted |
| **Exec form** (`"command": "<bin>", "args": ["hook", "recall"]`) | No `/bin/sh -c` hop (measured 0.28 ms) and no quoting hazard. Legacy's entries are `ENV=… python3 /path/script.py` strings, which need a shell by construction |
| **`UserPromptSubmit` timeout 10 s, not legacy's 45** | The hook's own client timeout is 400 ms and C3's breaker bounds the rest. A 45 s ceiling cannot *help* — it can only hide a wedged daemon for 45 seconds per prompt |
| **No `PostCompact` entry** | Legacy names one in a comment but never wired it (`recall.py:41` vs `hooks.json`). Nothing to port |
| **Shadow is the default install mode** | Legacy has no equivalent: installing it *is* enabling it. Here the install is inert by construction (§Two layers) |

`Stop` keeps `async: true` and `SessionStart` keeps 5 s: both match legacy, and
both are load-bearing — `async` is what makes the once-per-session initial
retain (68 ms) invisible.

---

## Two layers, and why `--mode` does not write config

Wiring and enabling are independent:

* **wiring** — the four entries in `settings.json`, written here;
* **runtime** — `[hooks] mode` in `config.toml`, read by `hook recall` on every
  prompt, plus `enabled` / `MEMGARDEN_HOOKS_DISABLE`.

`install --mode full` is therefore a **declaration of intent**, not a setting.
It selects the double-injection gate, and then prints the one line that would
actually change the runtime. It deliberately does not write `config.toml`:
that file is TOML with the user's comments in it, so writing it needs a second
comment-preserving splice engine — for a value the user can set with one line
of `$EDITOR`. Plan §Binding decisions #13 wants "the switch must not flip
anything by existing" to be true by construction; a `--mode` flag that cannot
reach the runtime is exactly that.

The cost is honest and stated in the output: `--mode full` prints *"does NOT
change this"* with the config path, and `full_mode_refuses_while_legacy_is_wired`
asserts that sentence.

---

## The refusal

`--mode full` exits **1** while legacy is wired, listing the entries it found.

The hazard is asymmetric, which is why the refusal is one-directional.
MemGarden's retain strips `<hindsight_memories>` and `<relevant_memories>` as
well as its own tag (CE-5b, ported from `lib/content.py:47-48`), so legacy's
injections cannot re-enter our bank. Legacy's strip list has never heard of
`<memgarden_memories>`, so in full mode it retains our block into its own bank
— and we cannot fix legacy.

Detection matches `hindsight` case-insensitively anywhere in `command`. A
tighter match on the script filename would miss a shell-wrapped invocation, and
the two error directions are not symmetric: a false positive costs one
`--allow-double-injection` flag, a false negative ships the corruption this
check exists to prevent.

---

## Exit codes, and the guarantee that did not move

This is the only subcommand family that is **not** a hook. It is typed by a
human, once, and a refusal that exits 0 is a refusal no script can see. So
`dispatch` now returns `ExitCode` instead of `()`.

The never-2 guarantee is untouched, and it is now structural in a second way:
`ExitCode` gives this code no way to *say* 2. The only two values constructed
are `SUCCESS` and `FAILURE`, and `a_bad_invocation_exits_one_and_never_two`
pins it against the real binary for four bad invocations.

One related re-ordering: `MEMGARDEN_HOOKS_DISABLE` is checked **after** the
`hooks` family is dispatched, not before. A tool that reports whether the hooks
are wired has to work precisely when they are switched off — that is the state
the user is asking about. `status_still_answers_when_the_hooks_are_disabled`
covers it, and the disable check still precedes every real hook.

---

## Writing

`backup` → `write_atomic`, in that order, and a failed backup **aborts the
write**. The backup is the recovery path for the one race the atomic write
cannot close, so proceeding without it would be proceeding without the escape
hatch.

**Atomic because of the file watcher, not because of crashes.** The hook docs
say direct edits to settings files "are normally picked up automatically by the
file watcher": the write reconfigures every running Claude Code instance the
moment it lands, and a watcher that reads a half-written file gets a
settings.json that does not parse. Hence tmp-in-the-same-directory (so `rename`
is single-filesystem and therefore atomic), `fsync`, `rename`.

Two deviations from the plan's sentence, both narrowing:

* **A full byte comparison replaces the specified SHA-256.** It needs no hash
  implementation in a crate whose dependency closure is CI-enforced, and on a
  7 KB file it is strictly stronger — a hash can collide, a comparison cannot.
* **The file's own permissions are preserved.** `settings.json` is not a
  MemGarden file; a hook installer has no business tightening or loosening its
  mode. (Every other file this crate writes is forced to 0600 for the opposite
  reason: they are ours and they carry cursors and paths.)

The residual race is unchanged and accepted: someone writing between the
re-read and the `rename` still loses their edit, and the timestamped backup is
its recovery.

---

## The marker, and where the plan was wrong

The plan specifies the marker as "the installed binary's absolute path followed
by `" hook "`". **That string cannot exist in what we emit**, because the same
plan pins the exec form — the path and the word `hook` land in two different
JSON values (`"command"` and `"args"[0]`). A path-derived marker would also
break `uninstall` for anyone who moved or rebuilt the binary between the two
commands, which is exactly when they need it.

The marker is `"statusMessage":"memgarden: ` — a field Claude Code already
renders, so it is visible in the UI, it is ours by construction, and it
survives the binary moving. `statusMessage` was going to be on every entry
anyway: during a shadow run two memory systems are wired at once and the user
needs to see which one spoke.

---

## `status` reports `unconfirmed` bytes, and that was a requirement, not a nicety

The open daemon defect — a `done` job with `chunks_failed > 0` opens a cursor
gap, and the worker's unconditional `confirmed_offset` write then erases the
evidence through the `MAX` merge — is mitigated today by a **`debug`-gated
stderr line**, and `[hooks] debug` defaults to false. A default installation
would therefore show nothing at all about the one number the shadow run's own
re-entry criterion is written in. Shipping C5 without this would have meant
shipping a shadow run that cannot evaluate itself.

So `status` prints two things, neither of them behind `debug`:

* **locally recorded `pending`** — bytes this hook POSTed and has not seen
  settled. No daemon needed, and it is the plainest statement of the fact.
* **`byte_offset − confirmed_offset` from the daemon's row**, per session, with
  a total.

Two properties are stated in the output rather than left to a reader:

**It is a lower bound.** The same defect that opens the gap shrinks this
number. A non-zero value is real; a zero is *not* proof of convergence.

**The probe is capped at 10 sessions and says so.** One GET per session on a
command a human types is fine; unbounded is not. A silent truncation would read
as "all sessions are clean".

Reading `byte_offset` here is deliberate and is the only place it happens.
`cmd::Mirror` omits the field by construction, because seeding a *cursor* from
the optimistic value skips exactly the bytes the dual cursor protects — that
property is worth keeping exactly as it is. `MirrorStatus` is a separate struct
in `cmd::hooks` for the one job where the **difference** between the two
cursors is the answer, and it writes nothing.

---

## Known limits

**Uninstall is prefix-matched, not provenance-tracked.** When install *creates*
a wrapper (`"SessionEnd": [ours],` or the whole `"hooks": {…}` member), that
wrapper is one line and uninstall deletes the line. If you hand-add a second
entry inside an event array MemGarden created, uninstall takes it with ours.
The chunk must start with one of the three prefixes we emit, so a wrapper we
did not write is never touched — but within one we did, the granularity is the
line. The runbook says so, and the timestamped backup is the recovery. A real
fix needs a sidecar recording what we wrote, i.e. a second file to keep in sync
for a case that has not happened.

**`status` reports four events, not all of Claude Code's.** A MemGarden entry
hand-copied into `PreToolUse` would not show up. Nothing installs one there.

**The daemon probe is one `/livez` plus one `/healthz`.** It does not
distinguish "starting up" from "wedged"; C3's breaker is what protects the
prompt path, and `status` is a diagnostic.

**Legacy's daemon is probed with a bare TCP connect** to 127.0.0.1:9077 and
nothing else. Plan §Cross-PR rules 1 — legacy is untouchable, and that includes
not sending it requests we do not have to.

---

## Measurement

Interleaved-paired, one driver process, arm B = `memgarden hook noop`, N=300.
Absolute cross-run comparison is invalid on this box (+1.5 ms measured on
identical bits), so every number is `A_i − B_i` from the same run.

### The consolidated AC-2 table — all four hooks, one build

| hook | A p50 | A p95 | A p99 | B p50 | **paired p50** | paired p95 |
|---|---|---|---|---|---|---|
| `hook session-start` | 0.549 | 0.624 | 0.929 | 0.294 | **0.255** | 0.332 |
| `hook recall` | 0.465 | 0.526 | 0.551 | 0.284 | **0.183** | 0.241 |
| `hook retain`, gated turn (9 of 10 `Stop`s) | 0.380 | 0.435 | 0.464 | 0.281 | **0.102** | 0.156 |
| `hook session-end` | 0.361 | 0.416 | 0.442 | 0.276 | **0.084** | 0.127 |

**A whole turn costs `recall` + `retain` = 0.845 ms p50 / 0.961 ms p95**
against AC-2's 10 ms. The comparison that matters is not the budget, though —
it is the system this replaces: the **live legacy hooks cost 33 ms on their
DISABLED path**, measured, i.e. more than an order of magnitude more to do
nothing than these do to work. AC-2's `<10 ms` is not reachable in Python at
all (an equivalent Python hook was measured at 24 ms cold, against 0.34 ms
here).

Two declared exceptions, both unchanged by this PR and both recorded in
`c4b-hook-retain.md`: the **initial retain** (~68 ms on a 21.9 MB transcript,
once per session, made invisible by `async: true` on the `Stop` entry this PR
installs), and a **hung daemon** (1.5 s on the first prompt, then the breaker).

### Arm B did not move — the paired-binary control, not an inference

C5 adds `serde_json::Value` *reading* to a binary that previously only wrote
JSON, and +97,792 bytes with it. C2b's lesson is that attributing an arm B
change by elimination is not attribution, so this was measured directly:
`--bin-b` against master's binary (`841d47a`), both arms `hook noop`, N=200.

| run | A (C5 binary) | B (master binary) | paired A−B |
|---|---|---|---|
| 1 | 0.273 | 0.276 | **−0.000** |
| 2 | 0.264 | 0.267 | **−0.002** |
| 3 | 0.261 | 0.265 | **−0.003** |

Three of three at or below zero: **no measurable cost, ≤3 µs either way**, on a
1.5 ms budget. Consistent with C3's and C4b's finding that binary *size* is not
what moves arm B — relocations are, and they went 221 → **226**.

### `scripts/hook-budget.sh`

```
== 1. size (human check, budget 8 MB) ==   1,653,240 bytes (1.58 MB)   ok
== 2. ldd (human check) ==                 vdso, libgcc_s, libc, ld    ok
== 3. cargo tree containment (CI-wired) == 21 crates, allowlist match  ok
== 4. LD_DEBUG=statistics (diagnostic) ==  226 relocations
```

**Only #3 is a CI gate.** #1, #2 and #4 are human PR-body checks.

### The installer itself

Not a hook and not on any budget — typed by a human, once — but measured
because "it is fast" should be a number. N=50 against a copy of the real
`settings.json` (7,157 bytes):

| command | p50 | p95 |
|---|---|---|
| `hooks status` (daemon down, legacy up) | 0.495 ms | 0.662 ms |
| `hooks install --dry-run` | 0.507 ms | 0.637 ms |
| `hooks uninstall --dry-run` | 0.342 ms | 0.389 ms |

---

## Manual verification

`--dry-run` against the **real** `~/.claude/settings.json`. This PR installs
nothing; the file's md5 was `82ee834e278808b36fa13e518ba7e143` before and
after.

```
$ memgarden hooks install --dry-run --settings ~/.claude/settings.json
--- /home/user/.claude/settings.json
+      {"hooks":[{"type":"command","command":"…/memgarden","args":["hook","session-start"],"timeout":5,"statusMessage":"memgarden: session start"}]},
+      {"hooks":[{"type":"command","command":"…/memgarden","args":["hook","recall"],"timeout":10,"statusMessage":"memgarden: recalling"}]},
+      {"hooks":[{"type":"command","command":"…/memgarden","args":["hook","retain"],"timeout":30,"async":true,"statusMessage":"memgarden: retaining"}]},
+      {"hooks":[{"type":"command","command":"…/memgarden","args":["hook","session-end"],"timeout":5,"statusMessage":"memgarden: session end"}]},

--dry-run: nothing written.
```

Four lines, one per event, at the file's own indentation, each inserted into an
event array that already exists and already holds a legacy entry.

```
$ memgarden hooks status --settings ~/.claude/settings.json
config      /home/user/.config/memgarden/config.toml
hooks       enabled = true
mode        shadow
daemon url  http://127.0.0.1:9100
state dir   /home/user/.local/share/memgarden/hooks

settings    /home/user/.claude/settings.json
  SessionStart      -          hindsight
  UserPromptSubmit  -          hindsight
  Stop              -          hindsight
  SessionEnd        -          hindsight

memgardend  cannot build a request: daemon token unavailable: …/daemon.token: No such file or directory
hindsight   listening on 127.0.0.1:9077

sessions    0 state files
```

Read on the live machine: legacy is wired on all four events and its daemon is
up; MemGarden is wired on none; `memgardend` has never been started here, which
is why there is no token file. That is the correct pre-cutover state and the
one AC-1's shadow run starts from.

Every automated test uses `--settings <tempfile>` with `HOME` redirected as
well (plan §Cross-PR rules 1: **never write `~/.claude/settings.json` from a
test**).

---

## Tests worth naming

| test | what it would catch |
|---|---|
| `install_changes_no_byte_outside_the_lines_it_inserts` | the whole point. Removing exactly what `install` *reports* it added must give the input back byte for byte — asserted against the reported chunks, not via `uninstall`, so the two properties fail independently |
| `uninstall_restores_the_pre_install_bytes` | the removal finds what the splice inserted, across all three shapes |
| `the_fixtures_key_order_is_unsorted_and_survives` | a future switch back to `Value` surgery. The fixture's top-level keys are in the real file's order on purpose; sorted, it would prove nothing |
| `strings_that_contain_our_key_names_do_not_move_the_insertion_point` | the scanner being led by `"hooks"`/`"Stop"`/`[` inside a *string value* — the live file's SessionStart hook embeds a whole escaped JSON document |
| `unusual_formatting_survives_the_round_trip` | tabs, CRLF, and a one-line document |
| `a_hooks_key_of_the_wrong_type_is_refused_rather_than_shadowed` | a duplicate `"hooks"` key silently disabling every hook in the first one |
| `a_duplicate_key_is_refused_rather_than_spliced_into_the_dead_member` | **the silent failure**: install reporting four entries written while wiring none. Both levels, plus the negative case (the same key at two *different* levels is not a duplicate — the real file has that shape everywhere) |
| `a_file_that_changed_under_us_is_not_overwritten` | the read-modify-write window, and that no temp file is left behind when it fires |
| `full_mode_refuses_while_legacy_is_wired_and_writes_nothing` | the double-injection ship, plus that a refusal writes nothing |
| `a_bad_invocation_exits_one_and_never_two` | the crate guarantee, on the one family that can return a code |
| `clear_poison_clears_the_stamp_and_the_counter` | an operator's intervention lasting exactly one request |
| `status_reports_unconfirmed_bytes_without_debug` | the shadow run being unable to measure its own criterion on a default install. It also asserts the fixture does **not** set `debug`, or it would prove nothing |

---

## What Phase F reads from here

* **AC-2 is discharged** by the consolidated table above: 0.845 ms per turn
  against 10 ms, on one build, with the arm B control saying the number did not
  drift.
* **AC-1 is now collectable** — `hooks install` (shadow) is the instrument, and
  `docs/runbook-hooks.md` §Collecting the AC-1 shadow evidence is the
  procedure.
* **AC-3 is still not discharged.** `c4b-hook-retain.md` §AC-3 is the
  authority; nothing in this PR changes it.
* **Do not run `--mode full` before the `chunks_failed > 0` cursor fix lands.**
  That defect (`c4b-hook-retain.md` §Known limits) produces cursor gaps exactly
  under the GPU contention a shadow run creates.
