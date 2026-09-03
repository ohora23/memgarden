# How it works

> 🇰🇷 [한국어](ko/design.md)

The parts of the design that are decisions rather than implementation — the
ones worth knowing before you change anything.

---

## Shape

```
Claude Code hooks (Rust subcommands, 0.85 ms per turn — budget 10 ms)
        │ raw transcript / query
        ▼
memgardend (axum, :9100) ──────────── web UI (dashboard + graph, Phase E)
 ├─ retain: caps → chunk → Ollama extraction → facts + entities + links
 │          └─ task ledger: working state, written but not yet read
 ├─ recall: FTS5 BM25 + sqlite-vec KNN → RRF fusion → token budget
 ├─ consolidate / reflect / reranker (off by default)
 ├─ in-binary embeddings (fastembed, bge-small, 384-dim, CPU)
 └─ metrics: lock-free atomics + benefit ledger
        ▼
 single SQLite file (WAL, STRICT) — vec0 vectors + FTS5 + graph tables
```

Four crates: `memgarden-core` (types, config, metrics), `memgarden-store` (all
SQL), `memgardend` (the daemon), `memgarden-cli` (the hook binary).

---

## Two tiers: what was true, and what is being worked on

Every table up to schema v11 stores things that **were** true — a fact, when it
was said, what retracted it. None of them holds what is being worked on *now*:
the open goal, what is blocked, the next action. So a session
that resumes after a break gets semantically similar facts and no answer to
"where was I".

Schema v12 adds that as a separate tier, the **task ledger**. The separation is
not an invention here — every system that solved cross-session continuity keeps
two: LangGraph splits thread-scoped checkpointers from its cross-thread Store,
CoALA splits working memory (which explicitly holds active goals) from
long-term episodic memory, Letta splits in-context memory blocks from archival
memory. A 2026 survey of 435 works names the missing piece "workflow and
task-ledger state" and calls the split its main taxonomic correction, because
live control state is invisible to a taxonomy of *remembered information*.

### One row per bank, and that is a measurement

The obvious key is the session. It is the wrong one, and the live data says so
without ambiguity. All 15 rows of the `sessions` table, 2026-09-01 — sessions
keep accruing, so read this as a measurement with a date on it, not a live
count:

```
source     : startup 11 · resume 4     <- 'compact', 'clear', 'fork': never
end_reason : prompt_input_exit 8 · NULL 6 · other 1
lifetime   : 116.7h · 116.0h · 95.7h · 70.2h · 49.7h ...
```

A 116-hour session is not one task. You switch banks without ending it, and you
switch tasks a dozen times inside it. A session-keyed ledger would hold a dozen
unrelated goals in one row and fire about fifteen times a month. Keyed by bank,
"what is going on in this project" has exactly one answer — which is the
question a resuming agent actually asks.

The boundaries that matter are therefore *inside* a session, and the harness
that establishes this is `scripts/boundary-replay.py`. Its header carries the
full census; run it yourself against your own transcripts.

### Nothing reads it yet, on purpose

The write path runs and the read path does not exist. That ordering is
deliberate: the rows have to be judged before any of them reach a prompt, and
this project's own substitution measurement (memory 11–7 *worse* at +5% tokens)
is the reason to check rather than assume. Look at what accumulates first.

```sql
SELECT bank_id, goal, next_action, datetime(updated_at/1000,'unixepoch')
FROM task_ledger;
```

Turn the write off with one line if the extra call is not worth it:

```toml
[retain]
write_task_ledger = false
```

### Two design choices that will look odd

**`anchors` is never asked of the model.** It holds the working directory and
the touched paths, assembled from values the daemon already has. The field
exists to be re-checked against the filesystem before the ledger is ever
believed — so a model asked for a branch name it cannot see would produce a
plausible one and convert a safety mechanism into false confidence.

That check is also the answer to the failure mode this tier could otherwise
*cause*: resuming a task that was finished or cancelled. Classifying the
transcript for completion is the wall CE-12 hit (22 false positives, 0
detections). Re-statting a path is cheaper and it is a question with an answer.

**The write is not gated on a clean run.** A job that lost a chunk still knows
what is being worked on, and the directions are asymmetric: a missing fact is
recovered by re-POSTing the transcript, a missing ledger is simply gone.

---

## One file, zero external processes

SQLite with `sqlite-vec` and FTS5 does vectors, keywords and the graph
in-process. There is no database server, no vector store, and no sidecar.

This is the load-bearing choice, and it came from operating the previous
system rather than from taste:

- **restart races vanish.** The old stack ran an embedded Postgres beside the
  daemon; a stop/start while a consolidation was mid-flight produced a
  "migration failed" that was really `connection refused :5432`. In-process
  storage cannot have that bug.
- **backup is `cp memgarden.db`.** No export step, no dump format, no
  second store to keep consistent.
- **brute-force KNN is fast enough** at this scale, so there is no index to
  build, tune, or invalidate.

The cost is honest: one writer at a time, and no horizontal anything. Both are
correct for a single-user local tool and would be wrong for a service.

---

## Local by construction

Extraction runs on your own Ollama over HTTP. Embeddings and reranking are
compiled into the binary and **forced onto CPU**.

CPU is not a compromise here, it is the fix. The single worst latency bug in
the legacy system was VRAM contention between the embedding model and the
14-billion-parameter extraction model on one card; moving embeddings to CPU
was most of an 830 ms → 20 ms recall. The GPU belongs to the big model, and the
daemon never competes for it.

There is no cloud provider to configure, which means there is no cloud path to
enable by accident.

---

## Korean actually works

CJK has no word boundaries, so a naive full-text index silently returns nothing
for Korean queries — it does not error, it just quietly retrieves less. The FTS
layer uses `unicode61` with `prefix='2 3 4'` and suffixes query terms with `*`,
and a guard test fails the build if that regresses.

The same care shows up in unglamorous places, because this is a bilingual
corpus:

- a 200-**character** filename cap is a bug when ported verbatim: 200 Korean
  characters is 600 bytes against ext4's 255-byte limit, so the sanitizer fails
  on exactly the input it exists to make safe. Cap in **bytes**, on a character
  boundary.
- `&s[..800]` panics on Korean. Every truncation in the codebase is
  character-wise or byte-wise on a boundary, deliberately.
- date parsing diverges from legacy on purpose: `8월 2일` resolves day-precise
  here, where legacy's dateparser takes the day and year from the reference
  date and returns a confidently wrong window.

---

## The hook binary

Four subcommands of one small binary that Claude Code spawns thousands of times
per session. Three guarantees hold it together.

### It can never exit 2

On `UserPromptSubmit`, exit 2 does not fail the hook — it **erases what the
user typed**. On `Stop` it prevents the turn from ending.

This is not hypothetical: the legacy Python recall hook exits 2 whenever its
`debug` flag is set, and that flag was `true` in the live config until it was
found and turned off. So here it is structural rather than careful:

- `main` returns `ExitCode::SUCCESS` on every hook path and no `?` escapes it;
- a panic hook prints one line to stderr and exits 0 — skipping Rust's
  end-of-main stdout flush, so a hook that panicked half way through writing
  the model's context emits **nothing** rather than a truncated JSON line;
- there is no `clap`, because its usage errors exit 2 and that alone
  disqualifies it;
- an unknown subcommand, empty stdin and malformed stdin are all silent
  successes.

The guarantee is stated as *never 2*, not *never non-zero* — a `SIGSEGV` gives
139 and a missing binary makes a launcher return 127, and no code inside the
process can prevent either.

### Recall fails open, retain fails closed

A recall that cannot answer lets the turn proceed with no memories: a memory
layer must never be why a prompt fails. It is bounded at 400 ms and a circuit
breaker opens after three transport failures, so a wedged daemon costs three
timeouts per cooldown instead of one per prompt.

A retain that fails does **not** drop the delta. The hook never spools a
payload, because the transcript already is one: the same bytes are re-sent on
the next `Stop`, then by the `SessionEnd` child, and if the whole session
failed, by the next session's catch-up.

### The cursor is committed optimistically and confirmed later

The daemon answers a retain with `202 Accepted` — *queued*, not ingested, and a
worker can still fail a chunk afterwards. So the hook advances its offset and
records the job id; on the **next** retain it asks how that job ended:

| answer | what happens |
|---|---|
| `done` | clear the record and carry on |
| `failed` | roll the offset back and re-send |
| still running | leave it, skip this turn — never stack two unconfirmed jobs |

The daemon keeps two cursors for the same reason: `byte_offset` is what the
hook posted, `confirmed_offset` is what a clean job ingested, and the
difference is the in-flight-or-lost window. Recovery always seeds from the
durable one — seeding from the optimistic one skips exactly the bytes the split
exists to protect.

---

## `settings.json` is spliced, never rewritten

`~/.claude/settings.json` is a live, shared file: other tools' hooks, the
statusline, the plugin registry. `serde_json` here has no `preserve_order`, so
its map is a `BTreeMap` and **any** parse-and-re-emit sorts every key in that
file.

So the installer uses `serde_json` to *validate and locate* and never to
produce output bytes. A string-aware forward scan finds the byte offset; install
inserts exactly one line; uninstall deletes exactly that span.

Narrowing the operation to insert-one-line / delete-one-line is what makes
*"uninstall restores the pre-install bytes"* a property that can be tested at
all. The write is atomic (temp file in the same directory, `fsync`, `rename`,
with the source bytes re-checked immediately before) — not for crash safety, but
because Claude Code's file watcher picks the edit up mid-session and must never
see a half-written file.

---

## Everything that touches untrusted input has a cap

Each of these caps exists because its absence produced a real incident in the
previous system:

| input | cap |
|---|---|
| transcript, first retain | server-side message cap; a 102 MB transcript once blew a one-hour wall clock |
| tool payloads | truncated before extraction — `toolUseResult` never leaves the machine |
| consolidation prompt | bounded by construction, after one outgrew the model context and looped on timeouts |
| query | characters, not bytes, matching what the daemon counts |
| retain body | 32 MB, with an oversize fallback that counts **body** bytes rather than file bytes — the units differ by ~2× and getting it wrong drops ~1,200 messages that had room |
| queue depth, entity names, prompt size | all bounded, all tested |

Caps live **server-side**. The hook posts messages and the daemon decides what
to keep, so a stale hook binary cannot bypass a limit.

---

## Measurement is part of the design

Metrics are lock-free atomics with no LLM call anywhere near them, because
"zero added latency on the hot path" is an acceptance criterion, not an
aspiration. Every retain also writes what its caps saved into a benefit ledger,
so the system keeps honest books on its own value.

Two rules make the numbers mean something:

- **Paired, never across runs.** Re-benchmarking an identical commit has
  returned +1.5 ms on identical bits, so hook numbers come from one driver
  process alternating the subcommand with `hook noop` and reporting the
  difference.
- **Never mix percentile kinds.** `/metrics.json` interpolates inside 20 fixed
  buckets; the hook bench produces exact order statistics. Comparing them is
  invalid at any percentile. The `under_35ms` / `under_60ms` counts *are* exact,
  because those bounds are the SLO boundaries.

---

## The lock lesson

The sharpest bug of the hook phase is worth stating on its own, because it is
about the test suite rather than the code.

A retain held a per-session lock across the network — up to ~10 s — while
`recall` took the **same** lock on every prompt. Measured, a recall blocked
1.752 s; with a non-waiting acquire, 0.002 s.

Three things hid it:

1. the circuit breaker is checked **inside** the lock, so everything built to
   make a hung daemon cheap was bypassed by waiting for the lock;
2. the benchmark measures one hook in isolation and structurally cannot see it;
3. **the mutation that reintroduced the bug survived the entire test suite**,
   because every test runs one hook at a time.

The rule that came out of it: when a hook takes a lock another hook takes, the
test that would catch the regression must run **two processes**. One-at-a-time
is the default blind spot of this harness.
