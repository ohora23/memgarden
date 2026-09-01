# Usage

> 🇰🇷 [한국어](ko/usage.md)

Once installed, MemGarden is meant to be invisible. This page is what to do
when you want to look at it anyway.

---

## The two switches

They are independent on purpose, and knowing which one you are touching is most
of operating this system.

| | where | what it decides |
|---|---|---|
| **wiring** | `~/.claude/settings.json` | whether Claude Code runs the hooks at all |
| **runtime** | `~/.config/memgarden/config.toml` | `enabled`, and `mode = shadow \| full` |

Installing sets the first. It never sets the second — `hooks install --mode
full` prints the line to change and does not write it, so that installing the
switch can never throw it.

### shadow vs full

| | `shadow` (default) | `full` |
|---|---|---|
| session-start, retain, session-end | live — the bank fills | live |
| recall | calls the daemon, real latency, logs to `shadow-recall.jsonl` | prints `additionalContext` |
| what the model sees | **nothing** | MemGarden's memories |

Shadow is not a dry run. It exercises every code path and produces the A/B
evidence for the cutover; the only thing it withholds is the injection itself.

To go live, one line:

```toml
[hooks]
mode = "full"
```

No restart. The next prompt reads it.

---

## Day to day

```bash
memgarden hooks status
```

is the one command worth remembering. It reports, in order:

- the resolved config, runtime mode, and whether `MEMGARDEN_HOOKS_DISABLE` is
  overriding it;
- which memory system is wired **per event** — MemGarden, legacy, or neither;
- `memgardend`'s `/livez` and `/healthz`, and whether legacy's daemon is up;
- session state: how many, the oldest, any poisoned ones;
- **`unconfirmed`** bytes — what this machine has sent that nothing has
  confirmed ingested;
- warnings that only apply while both systems are wired (GPU contention, and
  double injection if you are in `full`).

It always exits 0. It is a diagnostic, not a gate.

### What the hooks actually do

| event | what happens | cost |
|---|---|---|
| `SessionStart` | upserts the bank and the session row; spawns a detached child that catches up on stale sessions and prunes state files | 0.55 ms |
| `UserPromptSubmit` | one recall against the bank, bounded at 400 ms, circuit-broken after 3 transport failures | 0.47 ms |
| `Stop` | every 10th turn: reads the transcript delta by byte offset and POSTs it | 0.38 ms (gated) |
| `SessionEnd` | spawns a detached final retain and exits | 0.36 ms |

Retain fires every 10 `Stop`s rather than every turn (`retain_every_n_turns`),
and the first retain of a session sends the whole transcript — the one place
the budget is knowingly exceeded, which is why the `Stop` entry is `async`.

---

## Reading the system

```bash
curl -s localhost:9100/healthz | jq .              # HEALTHY / DEGRADED / UNHEALTHY
curl -s localhost:9100/metrics.json | jq .         # counters, latency histograms, ledger
curl -s localhost:9100/v1/banks | jq .             # what exists
curl -s "localhost:9100/v1/banks/<bank>/sessions" | jq .
```

Ask the bank something directly:

```bash
curl -s -X POST localhost:9100/v1/banks/<bank>/recall \
  -H 'content-type: application/json' \
  -d '{"query": "what did we decide about the cursor protocol?", "limit": 8}' | jq .
```

Bank ids are `claude-code::<project>` and contain `::`, so **percent-encode
them in a URL path**: `claude-code%3A%3Amemgarden`.

Two things about `/metrics.json` percentiles: they are **linear interpolations
inside 20 fixed buckets**, so they must never be compared against the hook
benchmark's exact order statistics. The `under_35ms` / `under_60ms` counts
*are* exact, because those bounds are the SLO boundaries themselves.

### Reading the task ledger

There is no endpoint, because nothing serves it yet — read it from the file:

```bash
sqlite3 ~/.local/share/memgarden/memgarden.db \
  "SELECT bank_id, goal, next_action FROM task_ledger;"
```

The `sqlite3` CLI is a separate package and MemGarden does not depend on it. If
it is not installed, any SQLite client will do — this one needs nothing beyond
the Python that is already there, and opening read-only keeps it out of the
daemon's way:

```bash
python3 -c "import sqlite3;print(*sqlite3.connect('file:$HOME/.local/share/memgarden/memgarden.db?mode=ro',uri=True).execute('select bank_id,goal,next_action from task_ledger'),sep=chr(10))"
```

One row per bank, replaced on every retain job that finds a goal. **Nothing
injects it into a prompt.** It is written so its content can be judged before
any of it is, which is the whole point of the current stage — see
[How it works](design.md#two-tiers-what-was-true-and-what-is-being-worked-on).

It costs one extra Ollama call per retain job. If that is not worth it:

```toml
[retain]
write_task_ledger = false
```

---

## When something looks wrong

| symptom | first thing to check |
|---|---|
| nothing appears in the bank | `hooks status` — is the daemon up, is `enabled` true? |
| the status message never shows | is the binary still at the path in `settings.json`? Re-run `install` after moving it |
| a hook seems to do nothing | `[hooks] debug = true` puts one line per invocation on **stderr**. It can never change an exit code |
| prompts feel slower | recall fails open at 400 ms and the breaker opens after 3 transport failures — `hooks status` says if the daemon is down |
| `unconfirmed` keeps growing | retains are queuing and not completing. Check the `retain_jobs` backlog and whether Ollama is contended |
| a session stopped retaining | it may be poisoned: `hooks status` lists them, `--clear-poison <sid>` clears it |
| settings.json looks wrong | the timestamped backup is in `~/.local/share/memgarden/hooks/` |

Two failure modes worth naming because they are *designed*, not bugs:

- **Recall fails open.** No daemon, a timeout, an open breaker — the turn
  proceeds with no memories. A memory layer must never be the reason a prompt
  fails.
- **Retain fails closed.** A failed retain does not drop the delta; the
  transcript *is* the spool, and the next `Stop`, the `SessionEnd` child, or
  the next session's catch-up re-sends it from the same file.

---

## Poisoned sessions

A session the daemon **durably** rejects (10 consecutive 4xx) is poisoned: it
retries once an hour instead of every turn. It is a slow-retry state, not a
latch, and any success clears it.

```bash
memgarden hooks status                              # lists them
memgarden hooks status --clear-poison <session-id>  # clears the stamp and the counter
```

---

## Backup

```bash
cp ~/.local/share/memgarden/memgarden.db /somewhere/safe/
```

That is the whole backup. One file, WAL-mode SQLite. There is no second store,
no external process holding state, and no export step.
