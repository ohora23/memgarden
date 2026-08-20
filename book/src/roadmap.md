# Roadmap

> 🇰🇷 [한국어](ko/roadmap.md)

Where the rebuild is, what remains, and the specific facts that decide when it
is allowed to replace the system it is copying.

---

## Phases

| Phase | Scope | State |
|---|---|---|
| **A — Foundation** | workspace + CI, SQLite schema (vec + FTS5 + graph + temporal), REST skeleton, metrics plumbing | ✅ merged |
| **B — Core pipeline** | embeddings · Ollama extraction · retain ingest · hybrid recall · entities/graph · temporal · consolidation · reflect · reranker, plus vector-space tagging and the recall-quality harness | ✅ merged |
| **C — Hooks** | session/turn state · CLI foundation + latency harness · session-start · recall · transcript delta reader · retain · the cutover switch | ✅ code-complete |
| **D — Migration** | read-only legacy snapshot (MG-1a) ✅ · archive → SQLite importer (MG-1b) ✅ · the AC-3 verifier (MG-2) ✅ | ✅ code-complete |
| **E — UI & metrics** | dashboard, graph API, WebGL viewer (pan/zoom/drag, live SSE), ledger views, the bank survey | ✅ merged |
| **F — Cutover** | run the AC-1..3 gates → shut the legacy system down → final record in the legacy repo | ⏳ AC-1 run, awaiting the user's signature |

Dependencies are `A → B → (C, D, E in parallel) → F`. The graph viewer needs
the link data from Phase B and nothing from C or D.

---

## The cutover gates

The old system is shut down when **all three** are met, and not before.

### AC-1 — quality parity — *run, awaiting the user's signature*

Recall quality on a fixed query set (8 existing A/B log entries + 12 new) must
be at least equal to the current system, judged by the user.

Run 2026-08-12, 20 queries to both live systems under the same knobs, against
criteria committed **before** the first query was sent
(`docs/evidence/ac-1-criteria.md`).

| | |
|---|---|
| better | **6** |
| equivalent | 2 |
| worse | **5** |
| unjudgeable (both scored 0 hits) | 7 |

**The gate condition (`worse ≤ better`) holds, 6 to 5** — by one query, on 13
judgeable of 20. Latency on the same set: p50 **11.5 ms** against legacy's
**51.0 ms**. Full result and the five losses quoted:
`docs/evidence/ac-1-memcompare.md`.

The PRD assigns this judgement to the user, so what is recorded here is a
recommendation and the evidence under it; the gate is not met until the user
signs it.

Three findings from the run outlive it:

* **shadow prompts are not a query set.** Five of six real prompts replayed
  standalone were unjudgeable — "이게 맞나?" has no referent without the
  conversation around it. The instrument works; prompts lifted from a live
  session are the wrong thing to feed it.
* **conclusion-type questions are answered in neither corpus.** They live in
  the curated `MEMORY.md`, which neither system captures. Either AC-1 gains a
  second corpus or it is explicitly scoped to auto-captured recall — an open
  decision, recorded in `docs/evidence/ac-1-shadow.md`.
* **the diagnosis offered for the five losses did not survive measurement.**
  See the ranking attempt below.

### AC-2 — performance — ✅ **met**

| | requirement | measured |
|---|---|---|
| recall p50 | ≤ 35 ms | ✅ |
| recall p95 | ≤ 60 ms | ✅ |
| **hook overhead** | **< 10 ms** | **0.845 ms per turn** |
| retain cap savings | −55…−87 % held | ✅ |

The hook figure is `recall` + `retain` on one turn, interleaved-paired against
the same binary doing nothing. For context, the legacy Python hooks cost
**33 ms on their disabled path** — more to do nothing than these cost to work.

### AC-3 — lossless migration — ✅ **met on a rehearsal, by the instrument**

Node, link and document counts must match across the existing banks, plus a
50-sample content diff.

Two of Phase D's three PRs have landed the moving parts. `mg-migrate snapshot`
freezes legacy over read-only GETs and refuses on fifteen integrity properties;
`mg-migrate import` carries the archive into SQLite. Measured against legacy's
own frozen `/stats`, **four banks**: 25 == 25 documents, **5,288 == 5,288
nodes**, 200 == 200 authored causal links, 1,747 observations with 2,114
provenance edges.

**And MG-2 now says so, which is the part that counts.** `mg-migrate verify`
reads three oracles — legacy's frozen `/stats`, the frozen archive, and the
database — and exits **0** on the four-bank rehearsal: every Tier-1 equality
green (25 documents, 5,288 nodes, 200 authored causal edges, 2,114 provenance
edges, 3,917 entities, 10,379 mentions), temporal self-consistency exact at
105,016 with zero edges in either direction of disagreement, and **no content
difference in the 50-sample diff**.

Phase F re-runs it against the cutover import, which is the run that decides.
Counts printed by the thing that wrote the rows are not evidence that the rows
are right — which is the whole reason the verifier is a separate program with a
separate oracle.

Two things the migration establishes that Phase F will need:

* **the numbers cannot all be equal, and the honest ones say so.** Authored
  `caused_by` edges transfer exactly. `temporal` and `semantic` are *rebuilt*,
  because a semantic edge is a function of an embedding space that is ours and
  not legacy's, and legacy's temporal neighbour query applies no 24-hour
  window where ours does. Legacy's four banks are also the last four: `entity`
  links exist in neither system's storage;
* **the migration is rehearsable at zero cost.** `--db <scratch>` builds a
  complete migrated database beside the live one with both daemons untouched,
  which is how the numbers above were taken;
* **a Tier-2 review stop has a way out that is not "ignore it".**
  `verify --accept-tier2 <hash>` records a human acknowledgement of one
  specific result. A phase that always exits 2 teaches the reader to ignore
  exit 1 within two runs.

Four further criteria (AC-4 graph viewer, AC-5 dashboard, AC-6 metrics, AC-7
PR discipline) are tracked but do not gate the shutdown.

---

## Known limits and open defects

Ordered by what blocks what.

### ~~The gold harness no longer reproduces its own ratified baseline~~ — retracted 2026-08-19, one day after it was filed

It reproduces exactly. The report compared a fresh run against
`gold/results.jsonl` **line 8** (0.3881 / 0.5221 / 0.3236) while lines 11 and
12 — the two newest runs at the same corpus digest and configuration — both
hold **0.3792 / 0.5162 / 0.3168**, which is what a fresh import benches today,
to all sixteen digits. Line 8 is the *thin-graph* number, superseded when
CE-7's fix took the corpus from 681 semantic edges to 43,830; the drop is
tabulated in `README.md` and explained in `docs/design/mg-1-migration.md`.
Determinism was measured on the way to retracting it: two imports of the
frozen corpus produce hash-identical nodes, links, entities and vectors.

**What was actually missing:** the run never read the ledger. The comparison
happened against a figure copied into a document months earlier, so a
correctly-superseded baseline read as drift. `recall_bench bench` now prints
the newest ledger row matching the corpus digest and `rerank_top_k`, the delta
to it, and `reproduces line N to the digit` when there is none — including on
runs that write nothing, which is precisely when a stale document is the only
thing left to compare against.

### The ranking fix for AC-1's losses was measured and not shipped — 2026-08-12

The memcompare write-up proposed that four of the five losses came from
action records ("X was executed") outranking substance. Two candidate fixes
were built and measured; **neither shipped**, and the diagnosis is downgraded
to an unsupported hypothesis. Full numbers:
`docs/evidence/ac-1-ranking-attempt.md`.

* **Narrowing the recency window** (365 d → 30/7/3 d) buys MRR up to +0.133
  and costs recall@5, 0.218 → 0.192 — the same trade the CE-11 reranker was
  turned off for. The best window also happens to match the gold corpus's own
  span, which is a fit to the corpus, not a fix.
* **Penalising action records** scores 20/20 on inspection and moves the
  benchmark +0.003–0.006 **at every strength, including 0.0** — noise.
  It also matches 5 of the 276 nodes the gold set labels *relevant*, one of
  them the `doctor --dry-run` record the AC-1 judgement called noise. The
  labeller and the judge disagree, and the judge also wrote the system.

What the run did establish: the score spread is the problem. On the worst
loss all twelve items land within **0.042** of each other, recency is 0.97 on
every one (a two-week bank against a 365-day decay) and proof is neutral, so
semantic similarity alone orders the list — and it scores an action record at
0.750 against a genuinely relevant item at 0.752.


### `resolve_fact` runs against the whole migrated bank after cutover — CE-7, established by MG-1b and not fixed by it

MG-1b measured what `entities::resolve_fact`'s fuzzy pass does over a dense
candidate set: **77 of 3,917 distinct names dissolved into others, 33 of them
into names with no plausible similarity** — `ce-4` into `ce-1`, `ci.yml` into
`cli.mjs`, `shell` into `schedule`. It removed the pass from the *importer*,
which is a one-time bulk operation legacy had already canonicalized for.

It did not remove it from retain, and cannot: that is the path the resolver
was written for. But two facts make the same condition recur after cutover:

* `load_resolution_context` is `WHERE bank_id = ?1 ORDER BY last_seen DESC
  LIMIT 5000` (`graph.rs:54-72`) — **bank-wide**, not per chunk. The migrated
  `bank-b` holds 2,491 entities, under the cap, so every retain
  scores every mention against all of them;
* `resolution_score` is `ratio*0.5 + overlap*0.3 + temporal*0.2`
  (`entities.rs:160-176`), so a co-occurring same-day pair holds 0.5 of the
  0.6 gate before names are compared.

And a second-order effect specific to a migrated bank: `resolve_fact` takes the
argmax with **no exact-match short-circuit**, while a migrated entity's
`last_seen` is its *legacy* date. Months after cutover an exactly-matching
migrated candidate holds `1.0*0.5 + overlap*0.3 + 0`, and a fresher,
co-occurring near-match can outscore it — so a later `CE-4` can be routed into
`ce-1` rather than onto the migrated `ce-4` row.

**Shape of a fix:** one line in `resolve_fact` — an exact `canonical_name`
match is the best possible match by definition and can short-circuit the
argmax. **What would justify it:** an AX-2 run showing the recall effect, for
the same reason MG-1b did not change it on the way past. A migration does not
get to reshape CE-7 while nobody is measuring.

### Fixed 2026-08-09, and measured directly rather than through AX-2

The short-circuit landed as described. What justified it was not an AX-2 run:
AX-2 measures recall quality, and the question here is how often the resolver
hands a mention to the wrong entity — which can be counted.

Replaying the scoring over the migrated corpus, against the real co-mention
sets from `node_entities` and with CPython's `difflib` rather than the code
being measured: **1,124 of 10,415 mentions (10.8%)** would resolve to a
different entity *despite an exact match existing*. Both sides are scored with
a temporal term of zero, which is what cutover makes permanent for migrated
entities and is the setting kindest to the exact match — a rival written after
cutover collects up to +0.2 more and wins by a wider margin.

What the failures look like:

```
'memgardend'      -> 'memgarden'        rival 0.624 vs exact 0.5
'claude code'     -> 'claude-code'      rival 0.605
'/deep-interview' -> 'deep-interview'   rival 0.783
'architect(opus)' -> 'architect'        rival 0.675
'phase f'         -> 'phase 1'          rival 0.654
'phase 1'         -> 'phase f'          rival 0.654
```

The last pair is the shape worth noticing: each absorbs the other depending on
which is mentioned. That is not a merge, it is an oscillation — the graph has no
stable answer for either name.

**A first pass got this wrong and is recorded because the error is instructive.**
It gave every rival the full 0.3 co-occurrence term whenever the rival had
co-occurred with anything at all, rather than with the entities named alongside
*this* mention, and reported 2,771 of 3,945 names at risk. That number measured
the assumption, not the corpus.

**Still open:** the short-circuit only rescues mentions whose exact match
already exists as an entity. MG-1b's other observed merges — `ci.yml` into
`cli.mjs` — are names that were never entities, and no short-circuit reaches
them. That is a threshold-and-weights question, and a separate decision.

### The test suite corrupts memory under concurrent load — `memgarden-store`, not the migration

`cargo test --workspace` intermittently dies with SIGSEGV or abort in the
`memgardend` test binary. Measured, same harness: **0 of 8 runs at `bccaed9`
(before Phase D's importer tests), 2 of 8 at `9fd1930`.** Under a synthetic
load of 8 concurrent test processes at 32 threads each, the importer tests
crash **13 times in 32 processes**.

Three distinct backtraces, all heap corruption **inside SQLite**: FTS5's index
merge writing a varint through a stale buffer pointer
(`fts5IndexMergeLevel` → `sqlite3Fts5PutVarint`), an FTS5 varint read from
`0xec9117eb9cd4eabf`, and the allocator itself
(`sqlite3MemSize(pPrior=0x1008)` under `pcache1Alloc`). SQLite is
`THREADSAFE=1`, `MUTEX_PTHREADS`, 3.53.2.

**It is not the migration code, and the reproducer is what said so.** Twenty-five
lines in `memgarden-store` — open a file-backed `Db`, `nodes::insert_batch` of
150 long-text nodes, drop — with no `migrate` involvement, no links and no
reopen, crashed **6 times in 32 processes** under the same load on 2026-08-07.
Phase D's tests were simply the first suite to hold dozens of file-backed
databases with substantial FTS5 content open at once.

**That reproducer stopped reproducing, and the conclusion it carried has to go
with it.** Re-measured 2026-08-09, same machine, same harness:

| what was run | 2026-08-07 | 2026-08-09 |
|---|---|---|
| `cargo test --workspace`, 8 runs | 2 died | **2 died** |
| the committed probe, 8 proc x 32 threads | 6 of 32 | **0 of 32** |
| its pre-reduction variant (+3,000 links in a second transaction, + reopen) | — | **0 of 32** |
| `memgardend` lib alone, 4 proc x 16 threads | — | **0 of 16** |

The defect is alive — the workspace tally is unchanged. What is not established
any more is *where* it lives: the smallest unit that reproduces is the whole
workspace run, where cargo schedules a dozen test binaries concurrently, and no
single binary reproduces on its own. "It is the store's, not the migration's"
rested entirely on the probe reproducing alone, so that sentence is now an open
question rather than a result.

**A second symptom, which is not a crash.** One of the two 2026-08-09 deaths was
`migrate::verify`'s sample test failing with `Store { message: "malformed
JSON" }` — a value read back wrong, not a segfault. Two observations is not a
pattern, but "the test suite corrupts memory" is scoped to crashes and this one
is not a crash, so the scope is doing work the evidence does not support.

## ASAN does not reproduce it

Run on 2026-08-09 against the only unit that reproduces (`cargo test
--workspace`), on nightly with `-Zbuild-std`, in three configurations —
Rust-only instrumentation; Rust **and** the bundled SQLite C; and the same again
with `--no-fail-fast` so every target runs regardless. **24 workspace runs, zero
ASAN reports, zero SIGSEGVs.**

Instrumenting SQLite is what makes this conclusive rather than inconclusive. A
redzone absorbing an overrun would still be *reported* by instrumented code; no
report at all points at the corrupting access not happening, which makes this a
timing- or layout-dependent defect that ASAN's allocator and its 2-4x slowdown
design out of existence. ASAN is the wrong instrument here. The next ones are
valgrind — much slower, no rebuild, precise access tracking instead of redzones
— or TSAN, if the cause turns out to be a race.

`rust-toolchain.toml` stays pinned at stable; nightly is installed locally and
`cargo +nightly` overrides the file, so nothing about the project's toolchain
needs to change to re-run any of this. The `-fsanitize=address` for SQLite has
to arrive through a `CC` wrapper that matches only the SQLite sources: a global
`CFLAGS` also instruments `ring`, whose objects then link into `ort-sys`'s
*build script* — a host binary with no ASAN runtime — and the build fails on
`__asan_stack_free_6`.

**What ASAN did surface, unrelated to the corruption:** under its slowdown,
`retain_api.rs`'s `await_job` poll fails with `Storage("database table is locked:
retain_jobs")` in 5 of 8 runs, and one `memgarden-cli` recall test times out.
`SQLITE_LOCKED` on a *read* under WAL is not what WAL is supposed to give, so
this is worth its own look — particularly since production slows down the same
way under GPU contention.

> **Closed 2026-08-10, and it was never under WAL.** `Db::open_memory` went
> through `r2d2_sqlite::memory()`, which opens
> `file:{uuid}?mode=memory&cache=shared` — and SQLite refuses WAL for an
> in-memory database, answering `memory` instead. `init_pragmas` asks with
> `pragma_update`, which does not read the answer back, so the request was
> discarded in silence and all 828 tests ran on shared-cache table-level
> locking. There a read concurrent with a write fails `SQLITE_LOCKED`
> **immediately**, and `busy_timeout` does not cover that lock class; the same
> read/write pair on the file+WAL production path passes untouched. Both halves
> were measured directly against the system sqlite before anything was changed.
>
> `open_memory` now opens a throwaway file, removed on drop, and a guard test
> pins `PRAGMA journal_mode` to `wal`. Against `retain_api` at 4 concurrent
> processes × 32 threads, interleaved so both arms take the same machine load,
> 160 runs each: **5 `SQLITE_LOCKED` deaths on shared cache, 0 on file+WAL.**
>
> **The corruption is a separate defect and is still open.** Each arm also died
> once in a way that is not a lock — a SIGSEGV on shared cache, `error in table
> sessions after add column: near "\n  ": syntax error` on file+WAL — and that
> second shape, a value read back wrong, is the same one as the `Store {
> message: "malformed JSON" }` above. It survives the storage change. What did
> improve is that the probe and the suite now run the same storage model, so
> the probe going quiet while the suite kept dying is no longer an
> inconsistency to explain.

**No MemGarden code is required to reproduce it.** A sibling probe was written
that goes nowhere near `Db`: raw `rusqlite`, an r2d2 pool of 4, the same
pragmas, an FTS5 index over one text column, 150 long rows per database, and no
`sqlite-vec` registered anywhere in the process. It died 25 times in 480 runs
while the `Db`-based probe beside it died 11 times — so `sqlite-vec` is not in
it, and neither is anything this repository wrote. What is left in the picture
is SQLite, FTS5, and the shape of the load.

**Every "ruled out by measurement" line here is now suspect, and that is the
most important sentence on this page.** On 2026-08-10 the reproduction rate was
measured repeatedly across one day and moved between 0% and 30% *for the same
binary and the same command*, with hour-long stretches at zero. Numbers taken at
different times are not comparable, and most of the eliminations below were
taken that way.

Kept for the record with that caveat attached: thread stack size
(`RUST_MIN_STACK=32M` changes nothing), `mmap_size`, `sqlite-vec`, serializing
pool construction, and the `cache_size` observation (13/32 → 9/32 at 2 MiB).

Two later measurements show how little that record is worth. A same-session
interleaved A/B — both arms in the same rounds, same binary, same machine load,
480 runs each — put `mmap_size=256MB` at **25** deaths against `mmap_size=0` at
**13**, which contradicts the "0 changes nothing" line above. A run of the same
shape an hour later returned **0 and 0**, which contradicts both.

**ASAN and TSAN have now been run properly, and neither finds it.**
`memgarden-store` does not depend on `ort-sys`, so the `CC`-wrapper problem that
broke the earlier attempt does not arise here; `libsqlite3.a` and
`libsqlite_vec0.a` were verified instrumented rather than assumed. 480 runs of
the reproducing probe under ASAN: **zero reports, zero deaths**. TSAN (with
`-Z build-std`, so std and the C are instrumented together) fires on the first
run, but every race it names is one SQLite documents as intentional —
`unixOpen`'s `randomnessPid`, whose own comment says "multiple resets are
harmless", and `walIndexTryHdr` reading the wal-index header without a lock
before validating it by checksum.

**A hardware explanation was proposed and withdrawn the same day.** Every crash
in one boot that the kernel annotated with a CPU — 42 of them — named the same
logical CPU, and pinning the probe there produced 12 deaths in 40 runs against
0 in 40 on its SMT sibling. It did not replicate: 80 sequential runs across four
CPUs, and 80 more with both siblings of that physical core saturated at once,
returned **zero**. Three synthetic self-checks pinned to the same CPU — integer
arithmetic plus `memcpy`; 32 threads context-switching with atomics and FP
state; a 512 MB streaming working set — return zero mismatches. The
42-of-42 clustering is a real observation and remains unexplained. It is not,
on this evidence, a defective core.

**The daemon's shape is not implicated.** One file-backed database, 16 threads,
6,400 FTS5-bearing inserts: 10 runs, 0 failures. Thirty short-lived databases
with 250 small inserts each at 64 threads: 20 runs, 0 failures. It takes many
concurrent databases *and* substantial FTS5 index building together.

**What it costs today:** `cargo test --workspace` is not a trustworthy gate
under load, so a PR's test tally has to be read from consecutive runs rather
than one, and a death has to be recorded rather than re-run away.

**What would close it: a reproduction that stays reproducible.** Every
instrument aimed at this so far has been aimed at a failure that was not firing
while it was aimed — that is what the ASAN, TSAN and CPU-pinning results above
have in common, and it is why each of them looked conclusive for an afternoon.
Until the rate is steady enough that two arms measured an hour apart mean
something, bisecting configuration is measuring the clock. The leading untested
lead is thermal: the 12-of-40 came directly after two hours of continuous
8-process × 32-thread load, and every failed replication was run on a
comparatively cool machine.

### ~~The cursor gap~~ — fixed (HK-1g)

A retain job finishing `done` with a failed chunk used to open a gap between
the optimistic and durable cursors that the worker's unconditional confirm then
erased through a `MAX` merge. The fix landed with the shape predicted below —
`offset_from` on the request, persisted on the job row, and a third
range-guarded update channel. `docs/design/hk-1g-retain-cursor-gap.md`.

The paragraph that follows is kept for the record, because it is where the
anti-shape is written down.

### The original entry

A retain job that finishes `done` while a chunk failed opens a gap between the
optimistic and durable cursors, and the worker's unconditional confirm then
erases the evidence through a `MAX` merge. Under the GPU contention a shadow
run creates — two extraction pipelines on one card — chunk failures are exactly
what gets likely.

The fix has a known shape (`offset_from` on the retain request → persisted on
the job row → a third guarded update channel) and a known **anti**-shape:
`confirm_if_settled` would freeze the durable cursor at zero for every session
forever, because the request path has already written the optimistic one.

Until it lands, `hooks status` reports `unconfirmed` bytes as a **lower bound**
— non-zero is real, zero is not proof of convergence.

### Smaller, with re-entry criteria

| limit | what would change it |
|---|---|
| the reranker ships **off** | a caller whose budget absorbs +14 ms p50, or a rerank path that does not starve the ingest loop below 90 % of offered load |
| one in-flight retain job per session | retain firing per turn, or job durations regularly exceeding 10 turns |
| no `SubagentStop` retain | a transcript with sidechain entries *and* a measured recall win from having them |
| uninstall is prefix-matched, not provenance-tracked | a case where someone actually loses a hand-added hook |
| Linux only | a Windows user |

`docs/parity-gaps.md` is the full list of legacy behaviour deliberately not
ported, each row with the fact that would reopen it.

---

## After v1

Out of scope for v1, with the reason recorded rather than the intention:

- **Obsidian vault export** — memory nodes to markdown with backlinks, joining
  the existing Library vault. A companion to the graph viewer, not a
  replacement for it.
- **Consolidation sophistication** beyond the ported dedup + observation round.
- **LLM-judged hit-rate metrics** (the measurement framework's Layer 3). Layers
  1 and 2 are deterministic and cost nothing; Layer 3 needs a judge, and a
  judge needs its own evaluation.
- **Multi-user, remote deployment, cloud LLMs, mobile.** Not deferred —
  excluded. There is no cloud path to accidentally enable, and that is a
  property worth keeping.

---

## How to read progress

- `README.md` — the phase table and the headline numbers.
- `docs/design/*.md` — one note per merged PR, each standing alone without the
  diff, each with a `## Diverged from legacy` section.
- `docs/parity-gaps.md` — what is knowingly missing and what would close it.
- `docs/PRD.md` — the deep-interview spec this repo executes, including the
  work order the PR ids refer to.
