# MemGarden: what a local memory layer for a coding agent costs, and what it buys

*Measured 2026-11-13/14 on build `e2cf68f` (v0.1.0, schema v15). Every number
below is from a command run on that build or from a dated record — in
[`docs/evidence/`](evidence/), or where marked, the operator's own notes; the
section it comes from says which. Corpus sizes are as of the measurement.*

## Abstract

MemGarden is a single-binary, local memory system for Claude Code. Four hooks
record every session and put a short block of relevant memories in front of the
model on every prompt. After three months of daily use on one workstation —
25,321 memories across 22 project banks — the measurements say:

- **It is cheap to run.** A hook costs **0.20 ms** of its own work per prompt
  and **0.12 ms** per turn end; recall through the hook against the live daemon
  is **10.3 ms p50 / 12.1 ms p95**. A prompt is never blocked on the network or
  on a model.
- **It got much cheaper to read.** Each prompt now carries **214 tokens and 2.4
  memories** on average, down from **929 tokens and 13.1 memories** before the
  abstain gate and **~1,800 tokens / 29.5 memories** in the first month (−88%
  overall).
- **Ingest is reliable.** Over the last 30 days **2,056 of 2,058** transcript
  chunks were extracted (0.1% failed), against 4.1% over the system's lifetime.
- **It retrieved better than the system it replaced** — 13 better / 5 worse / 1
  equivalent on a blind three-judge panel (2026-08, before the embedder
  change) — and a whole turn of its hooks (~0.95 ms of process time, both hooks
  together) costs ~35× less than that
  system's hooks did *with nothing enabled*.
- **It has not been shown to make the agent better at the job.** Three
  task-level experiments, on this operator, came out negative or inside noise.
  This is stated in §6, not buried.

The headline value is narrow and real: the right paragraph out of tens of
thousands, in front of the model, before the user finishes typing, for about
10 ms and a couple of hundred tokens per prompt.

## 1. System in one paragraph

A Rust daemon (`memgardend`, SQLite + FTS5 + `sqlite-vec`, in-process ONNX
embeddings) and a 1.8 MB hook binary (`memgarden`, glibc only — no async
runtime, no SQLite, no ONNX). `UserPromptSubmit` runs hybrid recall — BM25,
vector (multilingual-e5-small, 384-d), entity graph and temporal arms fused by
RRF — then drops anything already injected this session and anything whose
cosine is under a floor of 0.83, and fits the rest to a token budget. `Stop`
ships the new transcript tail; a background worker extracts facts with a local
LLM, links them, and a consolidation pass distils them into observations.
Design detail: [`architecture.md`](architecture.md).

## 2. Method and environment

| | |
|---|---|
| CPU / RAM / GPU | AMD Ryzen 7 9800X3D (8C/16T) · 89 GiB · RTX 5080 16 GB |
| OS | Ubuntu, Linux 6.8.0-146 |
| build | `e2cf68f`, `cargo build --release` |
| live corpus | 25,321 nodes, 22 banks, 286 MB database |
| benches | pinned to 4 cores (`taskset -c 4-7`), embedder at its default 4 threads |

Three kinds of measurement, labelled throughout:

- **Bench** — a harness in this repository, run during this write-up. Hook
  numbers use the interleaved-paired design of `hook_bench`: arm A (the hook)
  and arm B (`memgarden hook noop`, the same binary doing nothing) alternate in
  one driver, and the reported cost is A−B. Absolute latencies on this machine
  drift ±1.5 ms between runs of identical bits, so paired deltas are the
  trustworthy figures.
- **Live** — the daemon's own counters, snapshotted every minute into
  `metric_snapshots` since 2026-08-04 (74,348 snapshots, 190 daemon runs). Its
  latency percentiles are interpolated inside 20 fixed histogram buckets, so
  they are coarse; read them as a trend.
- **Recorded** — an earlier dated experiment in `docs/evidence/`, quoted with
  its date because the system has changed since.

Live runs with ≥1,000 recalls are excluded from the live aggregates: they are
offline replays used to calibrate the gate, not use.

## 3. Latency

### 3.1 Hook overhead — bench, N=300 per arm

| hook | arm A p50 | arm B p50 | **own work, paired p50** | paired p95 |
|---|---|---|---|---|
| `recall` (`UserPromptSubmit`) | 0.519 ms | 0.312 ms | **0.204 ms** | 0.276 ms |
| `retain` (`Stop`, gated turn) | 0.427 ms | 0.303 ms | **0.120 ms** | 0.185 ms |

A whole turn's hook work is **~0.32 ms** paired, or **~0.95 ms** of absolute
process time for both hooks (arm A). The binary's fixed cost (arm B,
0.31 ms) is inside its 1.5 ms budget. For comparison, the Python hooks this
system replaced cost **33 ms on their disabled path** (recorded, 2026-08,
[`performance.md`](performance.md)), and an equivalent Python hook measured
24 ms cold — the language's interpreter start alone exceeds this whole budget.

### 3.2 Recall end to end — bench against the live daemon

`hook_bench --real`: the real hook, the real daemon, the real 4,700-node project
bank, a Korean-and-English prompt, N=300.

| p50 | p95 | p99 |
|---|---|---|
| **10.3 ms** | **12.1 ms** | 13.5 ms |

The gate this was designed against is p95 ≤ 70 ms.

### 3.3 How recall scales with bank size — bench

`hybrid_recall_bench`, one synthetic bank, all four arms, 1,000 requests:

| nodes in the bank | p50 | p95 | p99 |
|---|---|---|---|
| 3,000 | 8.1 ms | 10.1 ms | 11.8 ms |
| 10,000 | 16.0 ms | 19.3 ms | 22.8 ms |
| 25,000 | 34.8 ms | 41.2 ms | 44.4 ms |

Cost is linear in bank size, ~1.3 µs per node, because the vector arm is a
brute-force scan. Banks are per project and the largest live one holds 6,782
nodes, so today's per-bank cost is ~10–15 ms. **A single bank past ~40k nodes
would cross the 60 ms p95 budget** (p95 grows ~1.5 µs per node); the upgrade is an ANN index, and it is not
built because no bank is near it.

### 3.4 What the live histogram shows — live

| period | recalls | p50 | p95 |
|---|---|---|---|
| 2026-09-26 → 10-15 | 2,065 | ~12.6 ms | ~28.4 ms |
| 2026-10-16 → 11-13 | 1,000 | ~26.3 ms | ~55.5 ms |

Live latency doubled while the bench above stayed at 10 ms. The cause is not measured.
Two candidates landed in the second period and both add work per prompt: banks
grew (~16k → 25k nodes in total), and the cross-bank fallback (#76, live from
2026-11-02, so only the last part of the period) runs a second recall over up
to five other banks when the project's own bank returns nothing. Live numbers
also include cold page cache and recalls that race background ingest (which
the recorded loaded-run measurement put at 19.6 ms p50 / 48.8 ms p95). It stays
inside budget; it is the number to watch.

## 4. What it costs the model: injected tokens — live

| period | what changed | recalls | **tokens / prompt** | memories / prompt |
|---|---|---|---|---|
| 2026-08-10 → 09-26 | first month | 554 | ~1,800 | 29.5 |
| 2026-09-26 → 10-15 | per-session dedup (#64), terser lines (#67) | 2,065 | 929 | 13.1 |
| 2026-10-16 → 11-13 | multilingual embeddings (#73) + abstain gate (#74) | 1,000 | **214** | **2.4** |

The first-month figure includes a few runs from before an over-injection bug
was fixed and is the least reliable row; the documented August steady state
was 1,325 tokens / 18.1 memories. Against either, the current cost is a
fraction.

**Why the gate could cut this much, and what it gave up.** The old
English-only embedder scored Korean prompts at AUC 0.507 — a coin flip — so a
cosine floor would have been noise. After switching to multilingual-e5-small,
precision of what was injected rose from 25.6% to 32.2% before any gate, and a
per-item floor of 0.83 kept 72% of the useful memories while dropping 38% of all
memories (precision 37.0%) — so it does lose 28% of the useful ones, the price
of the cut. Calibration and the full threshold table:
[`evidence/multilingual-embeddings.md`](evidence/multilingual-embeddings.md).

**What that cost is in context.** A recorded audit (2026-10-17, 76 turns after
the gate went live) put the injected text, including the cache re-reads it
causes on every later call, at **1.9%** of the session's price-weighted input —
an upper bound, because it ignores compaction.

## 5. Ingest

### 5.1 Reliability — live, `retain_jobs`

| window | jobs | chunks | failed chunks | jobs `failed` | facts written |
|---|---|---|---|---|---|
| last 30 days | 264 | 2,058 | **2 (0.1%)** | 0 | 6,617 |
| lifetime | 466 | 3,893 | 160 (4.1%) | 42 | 12,630 |

The lifetime failures are concentrated before two fixes: schema-bounded string
fields, so a looping model reply costs a field instead of the chunk (#68), and
resuming a job after its finished chunks when the daemon restarts mid-job
(#69, schema v14). Over the daemon's current 128-hour run: 553 HTTP requests,
**0 errors**, 0 failed retain jobs.

### 5.2 Throughput — live

Extraction is a local LLM on the same workstation, off the prompt path:
**9.6 s per chunk p50**, a whole job (one session's new tail) **47 s p50 /
330 s p95**. Because the `Stop` hook only ships bytes and returns, none of this
is seen by the user.

The ingest caps cut what the extractor reads by **35.1%** in the current run
(533k → 346k tokens) — a saving to the local model, not to the user's context.
It is large when a session's first retain backfills a long history and close to
zero otherwise; see [`performance.md`](performance.md).

### 5.3 Footprint — live

| | |
|---|---|
| processes | 1 daemon (the system it replaced ran 3: daemon, Postgres, Python hooks) |
| daemon RSS | 0.9–1.3 GB across two `ps` readings, including the embedding model |
| database | 286 MB for 25,321 nodes; backup is copying one file |
| hook binary | 1.8 MB |

## 6. Retrieval quality and task outcome

### 6.1 Against the system it replaced — recorded, 2026-08-20

A blind three-judge panel compared the two systems' recall on real prompts,
anonymised and frozen: **13 better / 5 worse / 1 equivalent**, 0 unjudgeable.
Method and criteria committed before the run:
[`evidence/ac-1-blind-panel.md`](evidence/ac-1-blind-panel.md).

### 6.2 The frozen gold set — bench, with a caveat that decides how to read it

20 graded queries (14 answerable) over a frozen 2,718-fact corpus, production
weights (`semantic=0.1`). Labels on 19 of the 20 queries are still provisional:

| embedder | recall@10 | MRR | nDCG@10 | top-10 results with no label |
|---|---|---|---|---|
| bge-small-en (recorded) | 0.403 | 0.710 | 0.375 | 18 / 140 (13%) |
| multilingual-e5-small (now) | 0.371 | 0.583 | **0.395** | **54 / 140 (39%)** |

**This table cannot rank the two embedders.** The labels were pooled from the
old embedder's results, so an unlabelled result scores as irrelevant; the new
embedder returns three times as many results no one has judged. nDCG rises
while recall and MRR fall, which is what that bias looks like. Settling it means
grading the 54 new results, which has not been done. The change was made on the
live, mostly-Korean prompt stream, where the old embedder was at chance (§4).

### 6.3 Does it make the agent better at the job? — recorded, not shown

| experiment | date | question | result |
|---|---|---|---|
| MX-3 | 2026-08-25 | does injected memory substitute for reading the repo? | **11 worse / 7 better**, +5% tokens, −25% wall clock — 24 of 27 tasks had their answer in the repo ([result](evidence/mx-3-result.md)) |
| benefit audit | 2026-10-14 | what did injections do in 60 real turns? | **2 clear contributions, 0 misleading**, ~70% off-topic (one non-blind judge) |
| counterfactual | 2026-10-17 | after the gate, 47 injected turns | **0 used, 0 misleading**; of 214 memories, 160 irrelevant, 26 already in context, 28 relevant but unused |
| MX-4 | 2026-10-18 | does it help a user who keeps no notes? | **not shown** — 8 vs 6, memory-only 1 vs 2; 22 tasks × 1 run cannot resolve an effect this size (operator's notes; the PR carrying it, #75, was closed unmerged) |

Read together: **no measured case of memory misleading the agent, and no
measured task-level gain.** Both contributions that were seen came from a
*different* session than the one asking — the cold-start case a memory layer
exists for — and with a contribution rate around 3%, 0 of 47 is not evidence of
zero (0.97⁴⁷ ≈ 0.24).

**The caveat that matters most for anyone else.** The operator these numbers
come from writes almost everything down: a notes vault, dense commit messages,
a design note per PR, a handoff document at the end of every session. A corpus
census found 0 of 60 sampled memories said anything not already on disk
([result](evidence/bank-uniqueness-result.md)). On that user, memory is mostly
redundant with notes the agent can already see. A user who decides things in
chat and writes little down is the experiment that would move §6.3, and it has
not been run.

## 7. Summary

| claim | status | evidence |
|---|---|---|
| hook overhead ≈ 0.3 ms per turn | **measured** | §3.1 |
| recall ≈ 10 ms end to end on a project bank | **measured** | §3.2 |
| recall scales linearly; one bank's ceiling is ~40k nodes | **measured** | §3.3 |
| injected context down ~77–88% at 214 tokens/prompt | **measured, live** | §4 |
| ingest failure rate 0.1% over 30 days, 0 daemon errors | **measured, live** | §5.1 |
| better recall than the system it replaced | **measured, 2026-08** | §6.1 |
| new embedder better on the gold set | **not determinable** with current labels | §6.2 |
| better task outcomes | **not shown** | §6.3 |

## 8. Reproducing this

```bash
cargo build --release -p memgardend --bin recall_bench -p memgarden-cli --bins

# §3.1 hook overhead (stub daemon) and §3.2 end to end (live daemon)
./target/release/hook_bench --n 300 --warmup 20 --arm-a "hook recall" --stdin-a recall.json
./target/release/hook_bench --n 300 --warmup 20 --arm-a "hook retain" --stdin-a stop.json
./target/release/hook_bench --n 300 --warmup 20 --arm-a "hook recall" --stdin-a recall.json \
    --real http://127.0.0.1:9100

# §3.3 scale curve
MEMGARDEN_BENCH_NODES=25000 MEMGARDEN_BENCH_REQUESTS=1000 \
  cargo test --release -p memgardend --test recall_api -- --ignored --nocapture hybrid_recall_bench

# §6.2 gold set (corpus and labels are private; see gold/README.md to build your own)
./target/release/recall_bench import corpus.jsonl gold.db
./target/release/recall_bench bench gold.db queries.jsonl corpus.jsonl results.jsonl semantic=0.1 proof=0.0

# §4–§5 live figures
curl -s localhost:9100/metrics.json     # current run
# history: SELECT created_at, payload FROM metric_snapshots — one row per minute
```

`recall.json` and `stop.json` are ordinary hook inputs
(`{"session_id", "transcript_path", "cwd", "hook_event_name", "prompt"}`).
