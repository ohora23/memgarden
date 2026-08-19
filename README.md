<p align="center"><img src="assets/hero.png" alt="MemGarden — an agent's memory system as a tended garden: inputs flow in, facts take root, connections form, and knowledge is harvested over moments, days, weeks, months, years, a lifetime" width="100%"></p>

# MemGarden

A ground-up **Rust rebuild** of my personal long-term memory system for Claude Code — local-LLM fact extraction, hybrid recall, an interactive memory graph, and overhead-free savings metrics, all in one self-contained daemon.

## What it's for

An AI coding assistant starts every session amnesiac. It re-reads the same files, re-derives the same conclusions, and asks you things you settled last week. MemGarden is the layer that stops that: conversations are captured automatically, distilled into facts by a local LLM, linked to each other, and served back — in about seven milliseconds — as the handful of memories that matter for the prompt you just typed.

The name is the design. Memory here is not a bucket you throw things into; it is **tended**. Sessions rain down through hooks. Facts take root in per-project banks. Consolidation prunes duplicates and grafts observations into knowledge. A ledger keeps honest books on what the garden actually yields. Nothing leaves the machine it grows on.

## What makes it different

- **One binary, one file, zero external processes.** SQLite with sqlite-vec and FTS5 does vectors, keywords, and the graph in-process. No database server to start, no restart race to lose data to, and a backup is `cp memgarden.db`.
- **Fast enough to be invisible.** Recall measures **p50 7.1ms / p95 7.8ms** at 3,000 memories with all four retrieval arms live, against a 35ms budget — so the memory layer never becomes the reason a prompt feels slow. Under concurrent ingest that pushes the bank past 35,000 nodes it still clears the 60ms ceiling.
- **Local by construction, not by policy.** Extraction runs on your own Ollama; embeddings and reranking are compiled into the binary and run on CPU so they never fight the LLM for VRAM. There is no cloud path to accidentally enable.
- **Honest about its own value.** Every retain records what the input caps saved (**−75.3%** on a live extraction, **−86.9%** over a whole 5.8MB transcript) into a benefit ledger. Metrics collection costs **88ns per request** — 0.00025% of the latency budget — so measuring the system can never distort what it measures.
- **Korean works properly.** CJK has no word boundaries, so a naive full-text index silently returns nothing — measured: a 5-token Korean prompt retrieved **0** hits before the fix and 100 after. The FTS layer is built for it (unicode61 + prefix indexing + prefix-suffixed terms) and a guard test fails the build if that ever regresses.
- **Bounded everywhere it touches untrusted input.** Transcripts, tool payloads, entity names, prompt sizes, and queue depth all have caps with tests — because in the previous system each of those, uncapped, produced a real incident.
- **Recall quality is measured, not asserted.** A frozen 2,718-fact corpus and 20 graded gold queries produce recall@k / MRR / nDCG, so a ranking change reports a delta instead of a feeling. The harness ships; that corpus does not, because it is a real memory bank — [`gold/README.md`](gold/README.md) is how you export your own. It has already paid for itself: the embedded reranker looked like a +0.077 nDCG win until a temporal bug was fixed, after which the same measurement showed **+0.013 nDCG@10 with recall@10 going 0.044 negative** — which is why it ships off.
- **Reviewed like it matters.** Every change lands as a PR through a three-way review (functional / security / code), and fixes are confirmed by **mutation-testing them** — reverting the fix has to fail a test, or the test does not count. That discipline is also how the suite's own blind spot was found: a lock regression whose mutation survived every test, because every test ran one hook at a time.

## Why rebuild

The legacy stack (hindsight daemon + embedded Postgres + Python hooks) works, but every operational lesson it taught pointed the same direction:

- **Latency came from process sprawl** — 830ms recalls traced to GPU contention and a per-request `gc.collect` in a sidecar; fixed by config, but the lesson is architectural: fewer moving parts, no per-request penalties by construction.
- **Restart races** (embedded Postgres vs daemon) disappear when storage is an in-process SQLite file.
- **Unbounded inputs break things** — a 102MB transcript blew a retain wall-clock; an unbounded consolidation prompt outgrew the model context. Every MemGarden ingest path is capped by design, server-side.
- **Benefits must be measured, not vibed** — the [3-layer measurement framework](https://github.com/ohora23/memgarden-legacy/blob/master/docs/measurement.md) needs zero-latency counters built in, not bolted on.

## Architecture

```
Claude Code hooks (Rust subcommands, measured 0.85ms per turn — budget 10ms)
        │ raw transcript / query
        ▼
memgardend (axum, :9100) ──────────── web UI (dashboard + graph viewer, Phase E)
 ├─ retain: caps → chunk → Ollama extraction → facts + entities + links
 ├─ recall: FTS5 BM25 + sqlite-vec KNN → RRF fusion → token budget
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
| Migration | Legacy's own transfer archive, frozen to disk, re-embedded and re-linked here | Legacy exports facts without vectors, ids or derived links **by design** — carrying them and re-deriving is the supported path, not a compromise. The frozen archive is what makes the AC-3 evidence outlive the daemon it came from |

## Status

Work lands as PRD-tracked pull requests (template in `.github/`), each 3-way reviewed (functional / security / code) before merge.

| Phase | Scope | State |
|---|---|---|
| A — Foundation | workspace/CI, SQLite schema, REST skeleton, metrics plumbing (CE-1..3, MX-1) | ✅ merged |
| B — Core pipeline | embeddings CE-4 · Ollama extraction CE-5a · retain ingest CE-5b · hybrid recall CE-6 · entities/graph CE-7 · temporal CE-8 · consolidation CE-9 · reflect CE-10 · reranker CE-11, plus vector-space tagging AX-1 and the recall-quality harness AX-2 | ✅ merged |
| C — Hooks | session/turn state ✅ · CLI foundation + hook-latency harness ✅ · session-start ✅ · recall ✅ · transcript delta ✅ · retain ✅ · install & cutover switch ✅ | ✅ code-complete |
| D — Migration | read-only legacy snapshot MG-1a ✅ · archive → SQLite importer MG-1b ✅ · AC-3 verifier MG-2 ✅ | ✅ **code-complete** |
| E — UI & metrics | dashboard, graph API, WebGL viewer (pan/zoom/drag, live SSE), ledger views, the bank survey | ✅ merged |
| F — Cutover | quality-parity A/B + performance gates + lossless migration → legacy shutdown | ⏳ AC-1 run, awaiting signature |

**Where the cutover gates stand.** All three must pass before the old system is shut down:

| | requirement | state |
|---|---|---|
| **AC-1 quality** | recall quality ≥ legacy on a fixed query set, human-judged | 🔄 **run, awaiting the user's signature** — 20 queries to both live systems on 2026-08-12, against criteria committed before the first query: **6 better / 2 equivalent / 5 worse / 7 unjudgeable**, so the gate condition holds by one query on 13 judgeable. Latency p50 **11.5 ms** to legacy's **51.0 ms**. The PRD assigns the judgement to the user, so this is a recommendation with its evidence under it. [Criteria](docs/evidence/ac-1-criteria.md) · [Result](docs/evidence/ac-1-memcompare.md) · [The fix that was measured and not shipped](docs/evidence/ac-1-ranking-attempt.md) |
| **AC-2 performance** | recall p50 ≤35ms / p95 ≤60ms, hook overhead <10ms, retain cap savings held | ✅ **met** — 7.1/7.8ms recall, **0.85ms of hook per turn**, −75…−87% savings |
| **AC-3 lossless migration** | node/link/document counts match across the legacy banks + 50-sample content diff | ✅ **met on the live database**, by the instrument rather than the importer — `mg_migrate verify` exits 0 against the cutover import of 2026-08-08: every Tier-1 equality green (28 documents, 5,311 nodes, 201 causal, 2,125 provenance edges, 3,945 entities), temporal self-consistency exact at 105,199 in both directions, and **no content difference in the 50-sample diff**. Report: [`docs/evidence/ac-3.json`](docs/evidence/ac-3.json) |

### Migrating off the old system

Only if you have one. This phase reads a legacy hindsight daemon on `:9077` through its own
transfer-archive API; a fresh install skips it entirely and just lets the bank fill.

Three subcommands of one binary, and only the middle one writes a database row — `snapshot`
issues nothing but `GET` to the old daemon, and `verify` issues no `INSERT`, `UPDATE` or
`DELETE` at all:

```bash
mg_migrate snapshot --out migration/<date>/            # read-only GETs, ~2s, refuses on 15 identities
mg_migrate import   --snapshot <dir> --db <path>       # archive → SQLite, ~167s for the whole corpus
mg_migrate verify   --snapshot <dir> --db <path>       # the AC-3 report; exit 0 / 1 / 2
```

Five non-empty legacy banks — **5,311 nodes, 28 documents, 1,757 consolidated observations, 201
authored causal links** — carried through legacy's own supported transfer format, re-embedded here, with
temporal and semantic adjacency **rebuilt by our rules rather than copied**. Of the four link
types only the authored one (`caused_by`) can be equal, and the report says so rather than
averaging it away: a semantic edge is a function of an embedding space that is ours and not
legacy's, legacy's temporal neighbour query applies no 24-hour window where ours does, and
`entity` links exist in neither system's storage.

The snapshot is the migration source, not the live daemon — legacy is still being written, and
a re-fetch between importing and verifying would surface a fact written in between as a count
mismatch that is not a defect. Rehearsals cost zero downtime: `--db /tmp/…` builds a complete
migrated database beside the live one with both daemons untouched.

[Runbook](docs/runbook-migration.md) · [design](docs/design/mg-1-migration.md) ·
[verification](docs/design/mg-2-verification.md)

**One open defect, and nobody has pinned it down yet.** `cargo test --workspace` intermittently
dies with a SIGSEGV inside SQLite (FTS5 index merge, and the allocator) under concurrent load —
**2 of 8 runs**, measured again on 2026-08-09 and unchanged. What did change is the story around
it: the 25-line `memgarden-store` reproducer that once crashed 6 times in 32 processes now runs
**0 of 32**, as does the heavier variant it was cut down from, as does `memgardend`'s lib suite
on its own. The smallest thing that still reproduces is the whole workspace run, so "it is the
store's, not the importer's" has lost the evidence it stood on.

**ASAN does not reproduce it either** — 24 workspace runs, including a build with the bundled
SQLite C instrumented, produced zero reports and zero segfaults, which points at a timing- or
layout-dependent defect that the sanitizer designs away. **The daemon's shape is still not
implicated** (one database, 16 threads, 6,400 inserts: 10/10 clean), though one of the two
deaths was a value read back as malformed JSON rather than a crash, which is a symptom "corrupts
memory under test load" does not cover. Until this closes, a PR's test tally carries the caveat.
[Details, the numbers, and what to try next](book/src/roadmap.md).

**Next up, in order:** the user's signature on AC-1, then the switch to `mode = full` and the
legacy shutdown — Phase F's import leg is already done. Two things stand outside that path and
should be fixed before any further ranking work: the gold harness no longer reproduces its own
ratified baseline, and AC-1's unjudgeable third is partly a question about what the corpus is
supposed to contain.

## Claude Code hooks

Four hook subcommands of one small binary (496 KB–1.6 MB, glibc only, no
tokio/SQLite/ONNX in its dependency closure — CI-enforced), wired into
`~/.claude/settings.json` by the binary itself:

```bash
memgarden hooks install --dry-run    # print the exact lines, write nothing
memgarden hooks install              # shadow mode — wires everything, injects nothing
memgarden hooks status               # which system is wired per event, daemon health, poisoned sessions
memgarden hooks uninstall            # restores the file to its pre-install bytes
```

Measured per invocation, interleaved-paired against the same binary doing
nothing, N=300 — the whole per-turn cost is `recall` + `retain` = **0.85 ms**
against the 10 ms budget:

| | `session-start` | `recall` | `retain` (gated turn) | `session-end` |
|---|---|---|---|---|
| p50 | 0.549 ms | 0.465 ms | 0.380 ms | 0.361 ms |
| p95 | 0.624 ms | 0.526 ms | 0.435 ms | 0.416 ms |

Three properties the design is built around, all tested rather than intended:

- **It can never exit 2.** On `UserPromptSubmit` exit 2 *erases what you typed*.
  There is no `clap` (its usage errors exit 2), no `?` out of `main`, and a
  panic hook that exits 0 with empty stdout.
- **Installing turns nothing on.** Wiring lives in `settings.json`, injection
  in `config.toml`, and the default install wires all four events in *shadow*
  mode — real daemon calls, real latency, and nothing reaches the model.
- **`settings.json` is edited by line splice, never reserialized.** Uninstall
  restores the file byte for byte; a `serde_json` round-trip would silently
  re-sort every key in a file shared with other tools.

Full runbook — install, verify, shadow-evidence collection, rollback,
coexistence with the legacy hooks: [`docs/runbook-hooks.md`](docs/runbook-hooks.md).

## Performance

Every number here is measured on this machine (Ryzen 7 9800X3D, 16 threads, release build) and traceable to a design note in `docs/design/`. Two caveats travel with them, because the notes insist on it: **absolute latencies on this box drift ±1.5ms between runs of identical bits**, so paired deltas are the trustworthy figures; and the recall-quality labels are **provisional on 19 of 20 gold queries**.

### Recall — the budget that matters

| | p50 | p95 | p99 | conditions |
|---|---|---|---|---|
| **idle** | **7.1ms** | **7.8ms** | 8.7ms | 3,000 nodes, 2,000 requests, all four arms (BM25 + vector + graph + temporal) |
| **under concurrent ingest** | 19.6ms | 48.8ms | — | same, while a background loader writes ~35,700 nodes into the same bank |
| budget (AC-2) | ≤35ms | ≤60ms | | 1,605/2,000 under 35ms and 1,997/2,000 under 60ms in the loaded run |

Per-arm, isolated: graph 0.29ms p50, temporal 0.13ms p50 (0.54ms worst case), mental-model KNN 0.20ms p50 at 1,000 models. The scaling ceiling is the brute-force vector scan — whole-recall p95 is 9.7ms at 3k nodes and 40.7ms at ~32k, which puts the upgrade point somewhere near 50k.

Through the hook, against a live daemon and the 2,718-fact gold bank: **p50 7.96ms / p95 8.91ms / p99 9.49ms** end to end, on a Korean query — 7× inside the 70ms gate.

### Hooks — 0.85ms of the 10ms allowance

Interleaved-paired against the same binary doing nothing, N=300 per arm:

| | `session-start` | `recall` | `retain` (gated turn) | `session-end` |
|---|---|---|---|---|
| p50 | 0.549ms | 0.465ms | 0.380ms | 0.361ms |
| p95 | 0.624ms | 0.526ms | 0.435ms | 0.416ms |
| paired p50 (own work) | 0.255ms | 0.183ms | 0.102ms | 0.084ms |

**A whole turn is `recall` + `retain` = 0.845ms p50 / 0.961ms p95.** The comparison that matters is not the budget but the system this replaces: the legacy Python hooks cost **33ms on their disabled path** — more to do nothing than these cost to work. An equivalent Python hook measured 24ms cold, so AC-2's <10ms was never reachable in that language.

Two declared exceptions: the **first retain of a session** sends the whole transcript (68.6ms on a 21.9MB file, once per session, which is why the `Stop` entry is `async`), and a **hung daemon** costs 1.5s on the first prompt before the circuit breaker takes over.

The binary is 1.58MB, links only glibc/libgcc/vdso, and its dependency closure is CI-enforced — no tokio, no SQLite, no ONNX in a process that runs thousands of times per session.

### Ingest and extraction

| | measured | conditions |
|---|---|---|
| embedding, single | 2.41ms p50 / 3.39ms p95 | bge-small, 384-dim, fp32 ONNX, CPU |
| embedding, corpus | 26.2s for 2,718 nodes | real drain worker including its KNN pass |
| legacy migration, whole corpus | 207s for 5,288 nodes | dev build; snapshot 1.6s, then documents, facts, entities, observations, links and re-embedding, four banks |
| transcript delta read | 0.46ms for a 200KB tail; 64.3ms to parse 106.9MB | byte-offset resume, so the common case never re-reads |
| input-cap savings | **−75.3%** live, **−86.9%** over a 5.8MB transcript | written to the benefit ledger on every retain |
| consolidation round | 151s for 50 facts | real Ollama qwen3-14b-nothink, ~50% duty cycle against a 300s interval |
| reflect | 1.70s warm, 6.21s cold | 3 memories in the payload |

Extraction wall time is **deliberately not quoted**: the one live measurement (564s for three chunks) ran on a GPU shared with the legacy daemon and against a pathological fixture. It needs re-measuring on an idle card before it means anything.

### Cost of measuring

One `record` call is **74.3ns**; the full set a `POST /recall` touches — four counters and two histograms — is **87.7ns per request**. That is 0.00025% of the 35ms SLO, which is what makes "zero added latency on the hot path" an acceptance criterion rather than a hope.

### Recall quality

Against a frozen 2,718-fact corpus with 20 graded queries and 331 judgments, macro-averaged, Burges/TREC nDCG. That corpus is a real memory bank and is not in this repository, so these are **our** before/after record rather than a benchmark to compare against — recall@k is a property of the corpus and its labels ([`gold/README.md`](gold/README.md)):

| | recall@1 | recall@5 | recall@10 | MRR | nDCG@10 |
|---|---|---|---|---|---|
| shipped (RRF, no reranker) | 0.038 | 0.243 | **0.403** | 0.551 | 0.340 |
| with the reranker, top_k=10 | 0.047 | 0.258 | 0.358 | **0.701** | 0.352 |

The reranker wins ordering (+0.150 MRR) and loses coverage (−0.044 recall@10) for +13.7ms p50 / +31.8ms p95, and it drops background ingest to 89.9% of offered load. That is why it ships **off** — with a written re-entry criterion rather than a verdict.

**A caveat these numbers inherited, found during Phase D and since fixed.** Semantic links only
ever formed between nodes embedded in the *same* batch of 8: `embed_task.rs` built its
`fact_type` lookup from the just-embedded batch, so `semantic_links` dropped every neighbour
outside it and the KNN's other 99 candidates were discarded. The filter meant to select on
fact type was selecting on batch membership.

Fixed on 2026-08-09 — the lookup now covers the batch **and** its neighbours. Re-importing the
same corpus moves semantic edges **6,918 → 62,199** (0.11× → 0.96× of legacy's 65,149) and
out-degree from max 7, which was `batch_size - 1`, to max 20, which is `SEMANTIC_LINK_TOP_K`.

**Re-measured on the fixed graph, and the density bought nothing.** The gold corpus was rebuilt
through the same worker: **681 semantic edges → 43,830**, a 64× change, with out-degree going
from mean 1.24 / max 3 to mean 16.6 / max 20. Recall moved the wrong way:

| | recall@10 | MRR | nDCG@10 |
|---|---|---|---|
| thin graph (ledger line 8) | 0.3881 | 0.5221 | 0.3236 |
| 9× denser (line 11) | **0.3792** | **0.5162** | **0.3168** |
| relinked, +58% denser again (line 12) | 0.3792 | 0.5162 | 0.3168 |

Per stratum, nothing improved: `memcompare` recall@10 −0.025, `graph` nDCG −0.025, and
`identifier`, `conclusion` and `temporal` unmoved to three decimals. The ceiling is unchanged at
0.8588, as it must be — the labels never moved.

**Line 12 is not a rounding coincidence.** The fix only reaches nodes embedded after it, so
[`POST /v1/banks/{id}/relink`](docs/design/ce-7-entity-graph.md) re-runs the pass over a settled
bank; on line 11's own database it added 25,250 edges in 2.4s (43,830 → 69,080, out-degree mean
16.61 → 25.53, max 20 → 40) and every aggregate came back **identical to the last floating-point
digit**. The only field that moved anywhere in the record is q05's retrieved list, which reordered
without changing a metric. Of 400 pooled candidates, 8 were replaced — and not because the new
edges are weak, since their mean weight is 0.781 against the existing 0.7669. They never reach the
fused top-20: the graph arm is already saturated against its 200-node expansion cap, so a denser
graph feeds it more of what it was already discarding.

The honest reading is *no measurable gain*, not *a regression*: −0.9 points of recall@10 over 14
scored queries is inside what a set this size can resolve, and the second experiment moved nothing
at all. Two independent density changes now agree, so the assumption that these numbers were held
down by the thin graph is retired. The fix and the repair stand on the code having done something
other than what it said, not on a recall win neither delivered.

## Documentation

- **[Migration runbook](docs/runbook-migration.md)** — snapshot, rehearse, cut over, verify, with the four steps that are not optional marked as such
- **[Wiki](book/src/introduction.md)** — install, usage, how it works, extending it, roadmap. English canonical, Korean alongside. Build it locally with `mdbook serve book`.
- `docs/design/` — one design note per merged PR, each standing alone without the diff
- `docs/parity-gaps.md` — legacy behaviour deliberately not ported, each row with the fact that would reopen it
- `docs/runbook-hooks.md` — install, verify, collect shadow evidence, roll back
- `docs/PRD.md` — the deep-interview spec this repo executes

## Development

```bash
cargo test --workspace                       # full suite (unit + API + schema)
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p memgardend                      # daemon on 127.0.0.1:9100
cargo test -p memgardend -- --ignored        # live tests (need Ollama running)
cargo run -p memgarden-cli --bin hook_bench  # hook latency, interleaved-paired
cargo run -p memgardend --bin mg_migrate -- snapshot --out <dir>   # freeze legacy (GETs only)
cargo run -p memgardend --bin mg_migrate -- import --snapshot <dir> --db <path>
cargo run -p memgardend --bin mg_migrate -- verify --snapshot <dir> --db <path> --sample 50
./scripts/hook-budget.sh                     # binary size, ldd set, dependency closure
mdbook serve book                            # the wiki, at http://localhost:3000
```

Configuration: `config.example.toml` → `~/.config/memgarden/config.toml`, env overrides `MEMGARDEN_*`. The daemon creates its data dir `0700` and speaks plain HTTP on loopback with a Host-header guard — it is a single-user local tool by design.

## Repo map

- `crates/memgarden-core` — types, config, lock-free metrics
- `crates/memgarden-store` — SQLite layer (migrations, vec/FTS, banks/nodes/search/ledger)
- `crates/memgardend` — the daemon (routes, Ollama client, extraction, retain pipeline, embed worker) and `mg_migrate`, the one-way legacy migration tool
- `crates/memgarden-cli` — the `memgarden` hook binary (loopback HTTP, session state, bank derivation, transcript delta reader, and the `settings.json` installer); dependency-closure-checked in CI, because a hook pays for its binary on every invocation
- `gold/` — the frozen recall-quality corpus, graded queries, and the results ledger
- `book/` — the wiki: install, usage, design, extending, roadmap (English + Korean), built with mdBook
- `docs/PRD.md` — the deep-interview product spec this repo executes
- `docs/design/` — one design note per merged PR (mirrored to my Obsidian vault)

## Lineage

- [memgarden-legacy](https://github.com/ohora23/memgarden-legacy) — Python-era system: role-split architecture, ops runbook, fork patches, memdash/memcompare tooling, and the measurement framework whose numbers set this rebuild's acceptance bars
- [ohora23/hindsight `claude-code-integration`](https://github.com/ohora23/hindsight) — the fork whose caps, tags, and profile presets are ported server-side here

## License

MIT — see [`LICENSE`](LICENSE). Vendored browser libraries keep their own
notices in `crates/memgardend/ui/vendor/`.
