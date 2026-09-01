# Architecture and the case for the rebuild

Split out of the README on 2026-08-27 to keep the front page to overview,
headline numbers and usage. Nothing here is abridged.

## Architecture

```
Claude Code hooks (Rust subcommands, measured 0.85ms per turn — budget 10ms)
        │ raw transcript / query
        ▼
memgardend (axum, :9100) ──────────── web UI (dashboard + graph viewer, Phase E)
 ├─ retain: caps → <private> stripped → chunk → Ollama extraction → facts + links
 ├─ recall: FTS5 BM25 + sqlite-vec KNN → RRF fusion → token budget
 │          (retracted / expired facts dropped once, in `hydrate`)
 ├─ consolidate / reflect / reranker (off by default — see below)
 ├─ in-binary embeddings (fastembed, bge-small 384-dim, CPU)
 └─ metrics: lock-free atomics (74ns/op) + benefit ledger
        ▼
 single SQLite file (WAL, STRICT) — vec0 vectors + FTS5 + graph tables
```

| Decision | Choice | Why |
|---|---|---|
| Storage | SQLite + sqlite-vec (`=0.1.9`) + FTS5, external processes: **zero** | No restart races, one-file backup, and brute-force KNN keeps whole-recall p95 under 10ms at 3k nodes |
| Extraction LLM | Ollama HTTP (default `qwen3-14b-nothink`), swappable | GPU belongs to the big model; the daemon never competes for it |
| Embeddings / rerank | In-binary, CPU-forced | Measured 2.4ms embed p50 / 10.4ms per rerank call on CPU; VRAM contention was the #1 legacy latency bug |
| Korean search | FTS5 `unicode61` + `prefix='2 3 4'` + `*`-suffixed query terms | CJK has no word boundaries; guard-tested so recall can't silently degrade |
| Concurrency | One Ollama permit, background/interactive acquire split, hard deadlines | A 14B model on one GPU must be queued for, never raced for |
| Metrics | Static atomics, no LLM calls, `/metrics.json` + `benefit_ledger` table | Zero added latency on hot paths is an acceptance gate, not a hope |
| Fact lifecycle | `superseded_by` / `expires_at` as **columns**, filtered once in `search::hydrate` | Every reader has to know whether a fact is live, so the answer travels with the row recall already selects. As a graph edge it would be a join on the hot path, and any caller that forgot the join would serve retracted facts. Automatic detection was built and **measured off** — `docs/evidence/supersession-detection.md` |
| Migration | Legacy's own transfer archive, frozen to disk, re-embedded and re-linked here | Legacy exports facts without vectors, ids or derived links **by design** — carrying them and re-deriving is the supported path, not a compromise. The frozen archive is what makes the AC-3 evidence outlive the daemon it came from |

## What makes it different

- **One binary, one file, zero external processes.** SQLite with sqlite-vec and FTS5 does vectors, keywords, and the graph in-process. No database server to start, no restart race to lose data to, and a backup is `cp memgarden.db`.
- **Fast enough to be invisible.** Recall measures **p50 7.1ms / p95 7.8ms** at 3,000 memories with all four retrieval arms live, against a 35ms budget — so the memory layer never becomes the reason a prompt feels slow. Under concurrent ingest that pushes the bank past 35,000 nodes it still clears the 60ms ceiling.
- **Local by construction, not by policy.** Extraction runs on your own Ollama; embeddings and reranking are compiled into the binary and run on CPU so they never fight the LLM for VRAM. There is no cloud path to accidentally enable.
- **Working state is a separate tier from facts, and it is deliberately write-only.** Everything else stores what *was* true; `task_ledger` (schema v12) stores what is being worked on — goal, done, open, next action — one row per bank rather than per session, because the live `sessions` table says a session lasts days and spans a dozen tasks. Nothing reads it: the rows accumulate so their content can be judged before any of it is injected, which is the order this project's own substitution result (memory 11–7 *worse*, +5% tokens) argues for. The design was set by replaying the live transcripts, not by analogy — `scripts/boundary-replay.py`.
- **A retracted fact stops being served, everywhere, from one place.** The store records that fact B replaces fact A (`superseded_by`, schema v11), and `search::hydrate` — the choke point every retrieval arm and every caller above them passes through — drops it. That is why `/reflect` and the mental-model refresh needed no change: the synthesiser stopped being handed the dead version because nothing hands it out. Recognising a retraction *automatically* is a different problem, and an unsolved one: the detector ships off, with all three measured arms in [`docs/evidence/supersession-detection.md`](evidence/supersession-detection.md).

- **Honest about its own value.** Every retain records what the input caps saved (**−75.3%** on a live extraction, **−86.9%** over a whole 5.8MB transcript) into a benefit ledger. Metrics collection costs **88ns per request** — 0.00025% of the latency budget — so measuring the system can never distort what it measures.
- **Korean works properly.** CJK has no word boundaries, so a naive full-text index silently returns nothing — measured: a 5-token Korean prompt retrieved **0** hits before the fix and 100 after. The FTS layer is built for it (unicode61 + prefix indexing + prefix-suffixed terms) and a guard test fails the build if that ever regresses.
- **A turn can opt out.** `<private>…</private>` is dropped before extraction reads it, and an *unclosed* marker drops the rest of the message rather than storing it — the opposite failure direction from the memory-wrapper stripper next to it, deliberately, because the conservative choice differs when the text is a secret rather than an artefact. Retain-time only: it cannot unwrite what an earlier retain stored. Borrowed from [`claude-mem`](https://github.com/thedotmack/claude-mem), which is the one idea in it this project did not already have.

- **Bounded everywhere it touches untrusted input.** Transcripts, tool payloads, entity names, prompt sizes, and queue depth all have caps with tests — because in the previous system each of those, uncapped, produced a real incident.
- **Recall quality is measured, not asserted.** A frozen 2,718-fact corpus and 20 graded gold queries produce recall@k / MRR / nDCG, so a ranking change reports a delta instead of a feeling. The harness ships; that corpus does not, because it is a real memory bank — [`gold/README.md`](../gold/README.md) is how you export your own. It has already paid for itself: the embedded reranker looked like a +0.077 nDCG win until a temporal bug was fixed, after which the same measurement showed **+0.013 nDCG@10 with recall@10 going 0.044 negative** — which is why it ships off.
- **Reviewed like it matters.** Every change lands as a PR through a three-way review (functional / security / code), and fixes are confirmed by **mutation-testing them** — reverting the fix has to fail a test, or the test does not count. That discipline is also how the suite's own blind spot was found: a lock regression whose mutation survived every test, because every test ran one hook at a time.

## Why rebuild

The legacy stack (hindsight daemon + embedded Postgres + Python hooks) works, but every operational lesson it taught pointed the same direction:

- **Latency came from process sprawl** — 830ms recalls traced to GPU contention and a per-request `gc.collect` in a sidecar; fixed by config, but the lesson is architectural: fewer moving parts, no per-request penalties by construction.
- **Restart races** (embedded Postgres vs daemon) disappear when storage is an in-process SQLite file.
- **Unbounded inputs break things** — a 102MB transcript blew a retain wall-clock; an unbounded consolidation prompt outgrew the model context. Every MemGarden ingest path is capped by design, server-side.
- **Benefits must be measured, not vibed** — the [3-layer measurement framework](https://github.com/ohora23/memgarden-legacy/blob/master/docs/measurement.md) needs zero-latency counters built in, not bolted on.
