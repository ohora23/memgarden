# Runbook — the Claude Code hooks

How to wire MemGarden into Claude Code, verify it, collect the AC-1 shadow
evidence, and get back out. Everything here is `memgarden hooks …`; nothing
else in MemGarden ever touches `settings.json` — not the build, not
`cargo install`, not the daemon.

**Installing does not turn anything on.** Wiring and injection are two
independent layers, and the default install wires all four events while
injecting nothing into your conversations. See [Two layers](#two-layers).

---

## Two layers

| layer | where | what it controls | how to change it |
|---|---|---|---|
| **wiring** | `~/.claude/settings.json` | whether Claude Code runs the hook at all | `memgarden hooks install` / `uninstall` |
| **runtime** | `~/.config/memgarden/config.toml` | `enabled`, and `mode = shadow \| full` | `$EDITOR`, or `MEMGARDEN_HOOKS_DISABLE=1` |

`hooks install --mode full` does **not** write `config.toml`. It selects the
double-injection check (below) and then tells you the one line to change. The
switch must not flip anything by existing.

**The rollback that needs no file surgery** is the runtime layer:

```bash
export MEMGARDEN_HOOKS_DISABLE=1     # or: [hooks] enabled = false
```

A wired-but-disabled hook exits in ~0.3 ms without reading its config. That is
what you reach for at 3 a.m.; `uninstall` is for daylight.

---

## Install

Always dry-run first. It prints the exact lines it would insert and writes
nothing:

```bash
memgarden hooks install --dry-run
memgarden hooks install                  # shadow mode, the default
memgarden hooks status
```

What it writes — one line per event, in the **exec form** (no `/bin/sh -c`, so
no quoting hazard and no shell hop):

| event | args | timeout | async |
|---|---|---|---|
| `SessionStart` | `hook session-start` | 5 s | — |
| `UserPromptSubmit` | `hook recall` | 10 s | — |
| `Stop` | `hook retain` | 30 s | **true** |
| `SessionEnd` | `hook session-end` | 5 s | — |

**This takes effect immediately in every running Claude Code instance.** The
settings file watcher picks the edit up mid-session; there is no restart.

Every write takes a timestamped backup first and prints the path
(`<data>/hooks/settings-backup-<unix-ms>.json`).

### Options

| flag | effect |
|---|---|
| `--dry-run` | print the diff, write nothing |
| `--mode shadow` (default) | wire everything; `recall` calls the daemon and prints nothing |
| `--mode full` | declares intent to inject; **refuses while the legacy hooks are wired** |
| `--allow-double-injection` | override that refusal, deliberately |
| `--settings <path>` | operate on a different settings.json |

---

## Running alongside the legacy (hindsight) hooks

Shadow + legacy is the **supported** configuration, and it is the AC-1
instrument. Both systems are wired, legacy still drives the conversation, and
MemGarden fills its own bank and records what it *would* have injected to
`<data>/hooks/shadow-recall.jsonl`.

Full + legacy is refused, and the reason is asymmetric rather than tidy:
MemGarden's retain strips `<hindsight_memories>` before ingesting, so legacy's
injections never re-enter our bank. Legacy's strip list has never heard of
`<memgarden_memories>`, so it would retain our block into *its* bank. We cannot
fix legacy.

While both are wired, `hooks status` prints two warnings:

* **GPU contention** — both retain pipelines extract with Ollama on the same
  box, and legacy already holds `qwen3-14b-nothink`. Watch the `retain_jobs`
  backlog; chunk failures get more likely under contention, which is exactly
  what the cursor's `pending` reconciliation exists for.
* **Double injection**, only when `mode = full`.

Nothing is shared between the two systems except the transcript files (both
read-only) and `settings.json`. Different stores, different state dirs, no
corruption path, and switching back and forth loses nothing.

---

## Verify

```bash
memgarden hooks status
```

reports, in order: the resolved config and runtime mode; which of the two
systems is wired per event; `memgardend`'s `/livez` + `/healthz`; whether
legacy's daemon is listening on 9077; the local session-state files — count,
oldest, and any poisoned sessions; and **`unconfirmed`** bytes.

`status` always exits 0. It is a diagnostic, not a gate.

### `unconfirmed` — the number to watch during a shadow run

```
  <sid> pending job=<id> bytes=8000 (posted, unconfirmed)
  <sid> inflight=3000 B (byte_offset 5000 - confirmed 2000)
unconfirmed 3000 B across 4 of 4 sessions — a LOWER BOUND …
```

Bytes this machine sent that nothing has confirmed ingested. A steady non-zero
value that never drains means retains are being queued and not completing —
check the `retain_jobs` backlog and Ollama.

**It is a lower bound, not a measurement.** While the `chunks_failed > 0`
cursor gap is open (`docs/design/c4b-hook-retain.md` §Known limits), a job can
fail a chunk and still have its cursor confirmed, which shrinks this number.
Non-zero is real; zero is not proof that nothing was lost. The probe is capped
at 10 sessions and the output says when it truncated.

Then confirm the hooks actually run: open a Claude Code session, type
anything, and look for the `memgarden: recalling` status message. After that:

```bash
tail -1 ~/.local/share/memgarden/hooks/shadow-recall.jsonl   # what it would have injected
ls ~/.local/share/memgarden/hooks/                           # one state file per session
curl -s localhost:9100/metrics.json | jq .recall_latency      # the daemon's side
```

`memgardend` must be running for any of this to do work — **the hooks never
start it**. A hook that spawns a model-loading daemon is the restart race this
rebuild exists to remove. `hooks status` prints the command when it is down.

---

## Collecting the AC-1 shadow evidence

1. Install in shadow mode alongside legacy, and leave it.
2. Work normally for a few days. Every prompt appends one line to
   `shadow-recall.jsonl`; every `Stop` fills the bank.
3. Compare against the gold set (`gold/`, AX-2) and against what legacy
   injected on the same prompts.
4. **Do not enable `full` before the pending-reconcile fix for
   `chunks_failed > 0` has landed** — see `docs/design/c4b-hook-retain.md`
   §Known limits. A shadow run is the first time two retain pipelines contend
   for one GPU, which is the condition that makes that gap produce cursor gaps.

---

## Roll back

In increasing order of effort — stop at the first one that is enough:

```bash
MEMGARDEN_HOOKS_DISABLE=1                       # instant, per shell
# [hooks] enabled = false in config.toml        # instant, everywhere
memgarden hooks uninstall --dry-run             # see what would go
memgarden hooks uninstall                       # remove the four lines
cp <data>/hooks/settings-backup-<ms>.json ~/.claude/settings.json   # last resort
```

`uninstall` removes only the lines it inserted, so the file returns to its
pre-install bytes. Your SQLite bank and every session cursor survive:
re-installing resumes from the recorded offsets, the intervening turns arrive
as one larger delta, and the daemon's per-`doc_key` hash dedup absorbs any
exact resend.

---

## Poisoned sessions

A session the daemon has **durably** rejected (10 consecutive 4xx) is
*poisoned*: it retries once an hour instead of every turn. It is a slow-retry
state, not a latch, and any success clears it.

```bash
memgarden hooks status                                  # lists them
memgarden hooks status --clear-poison <session-id>      # clears the stamp and the counter
```

---

## When something looks wrong

| symptom | first thing to check |
|---|---|
| nothing appears in the bank | `hooks status` — is `memgardend` up, and is `enabled` true? |
| the status message never shows | is the binary still at the path in `settings.json`? Re-run `install` after moving it |
| a hook seems to do nothing | `[hooks] debug = true` puts one line per invocation on **stderr**. It can never change an exit code |
| Claude Code feels slower | `hook recall` fails open at 400 ms and the breaker opens after 3 transport failures. `hooks status` will say if the daemon is down |
| the settings file looks wrong | the backup is in `<data>/hooks/`, timestamped |
| a mental model asserts something that is no longer true | turn its schedule off before investigating — see below |
| a recalled memory is stale rather than wrong | retire it: `POST .../nodes/{id}/supersede` |

Two things this binary will never do, by construction: exit 2 (on
`UserPromptSubmit` that erases your prompt), and write anything to stdout on an
event where stdout is not model context.

### Turning one mental model off

A mental model refreshes on a schedule and reads as authoritative, so a wrong
one keeps asserting itself until someone notices. Stop the schedule first;
decide about the content after.

```bash
curl -X DELETE http://127.0.0.1:9100/v1/banks/{bank}/mental-models/{mm_id}/trigger
```

The model, its content and its citations stay — only the schedule goes. `DELETE
/v1/banks/{bank}/mental-models/{mm_id}` (no `/trigger`) removes it entirely.

A `PATCH` cannot do this: its fields are `Option`, `None` means "leave alone",
and a JSON `null` therefore says nothing. That gap is why this was once done in
SQL — [the day it mattered](evidence/mental-model-supersession.md).

### Retiring a fact that has been superseded

```bash
curl -X POST http://127.0.0.1:9100/v1/banks/{bank}/nodes/{id}/supersede \
     -H 'content-type: application/json' -d '{"by": <newer node id>}'
curl -X DELETE http://127.0.0.1:9100/v1/banks/{bank}/nodes/{id}/supersede
```

The fact stays in the database and in the graph; it stops being served by every
recall path, `/reflect` included. A 409 means the pair failed a guard — wrong
bank, already retired, or a replacement that is not strictly newer.

**Why by hand:** detecting this during extraction was built and measured, and it
named 22 things that were not retractions, so it
[ships off](evidence/supersession-detection.md).

### Keeping a turn out of the bank

Wrap it: `<private>…</private>`. Retain drops it before extraction reads it.

Three limits worth knowing, because a redaction control that overstates itself
is worse than none: the marker is **exact and lower-case** (`<PRIVATE>` and
`<private reason="...">` do not match); it covers **message text**, not tool
inputs or tool results; and it works at **retain time only** — it cannot unwrite
what an earlier retain already stored. An *unclosed* `<private>` drops the rest
of that message rather than storing it.

### Editing MemGarden's lines by hand

Don't. `uninstall` deletes the line it wrote, so anything you add to that
line — a second hook inside an event array MemGarden created — goes with it.
Add your own entries as their own group; the backup is the recovery if you
already did.
