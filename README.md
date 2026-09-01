<p align="center"><img src="assets/hero.png" alt="MemGarden — an agent's memory system as a tended garden: inputs flow in, facts take root, connections form, and knowledge is harvested over moments, days, weeks, months, years, a lifetime" width="100%"></p>

# MemGarden

A ground-up **Rust rebuild** of my personal long-term memory system for Claude Code — local-LLM fact extraction, hybrid recall, an interactive memory graph, and overhead-free savings metrics, all in one self-contained daemon.

MIT licensed · runs entirely on the machine it grows on · no network at runtime

**v1 is complete and in daily use.** Every acceptance criterion is met, the legacy Python system was shut down on 2026-08-21, and this is the only memory system wired to Claude Code on my machine. [Status, phases and cutover gates →](docs/status.md)

## What it's for

An AI coding assistant starts every session amnesiac. It re-reads the same files, re-derives the same conclusions, and asks you things you settled last week. MemGarden is the layer that stops that: conversations are captured automatically, distilled into facts by a local LLM, linked to each other, and served back — in about seven milliseconds — as the handful of memories that matter for the prompt you just typed.

The name is the design. Memory here is not a bucket you throw things into; it is **tended**. Sessions rain down through hooks. Facts take root in per-project banks. Consolidation prunes duplicates and grafts observations into knowledge. What a later fact contradicts is pulled rather than left to compete. A ledger keeps honest books on what the garden actually yields. Nothing leaves the machine it grows on.

```
Claude Code hooks (Rust subcommands, 0.85ms per turn — budget 10ms)
        │ raw transcript / query
        ▼
memgardend (axum, :9100) ──────── web UI (dashboard + graph viewer)
 ├─ retain:  caps → <private> stripped → chunk → Ollama extraction → facts + links
 │           └─ task ledger: the bank's working state, written but not yet read
 ├─ recall:  FTS5 BM25 + sqlite-vec KNN + graph + temporal → RRF → token budget
 │           (retracted and expired facts filtered out in one place)
 ├─ embeddings in-binary (bge-small, 384-dim, CPU) · consolidate · reflect
 └─ metrics: lock-free atomics (88ns/request) + benefit ledger
        ▼
 single SQLite file (WAL, STRICT) — vec0 vectors + FTS5 + graph tables
```

**One binary, one file, zero external processes.** A backup is `cp memgarden.db`. Extraction runs on your own Ollama; embeddings and reranking are compiled in and run on CPU so they never fight the LLM for VRAM. There is no cloud path to accidentally enable.

[Design decisions, and why this was rebuilt rather than patched →](docs/architecture.md)

## Headline numbers

Measured on one machine (Ryzen 7 9800X3D, release build) and traceable to a design note. Absolute latencies drift ±1.5ms between runs of identical bits, so paired deltas are the trustworthy figures.

| | measured | budget |
|---|---|---|
| **recall, idle** | **7.1ms p50 / 7.8ms p95** — 3,000 nodes, all four arms | ≤35 / ≤60ms |
| recall, under concurrent ingest | 19.6ms p50 / 48.8ms p95 while ~35,700 nodes load | ≤35 / ≤60ms |
| **hooks, whole turn** | **0.845ms p50** (`recall` + `retain`) | <10ms |
| cost of measuring | **88ns per request** — 0.00025% of the SLO | zero added latency |
| input-cap savings | **−75.3%** live, **−86.9%** over a 5.8MB transcript | −55…−87% |
| embedding | 2.41ms p50 single · 26.2s for a 2,718-node corpus | — |

The legacy Python hooks cost **33ms on their disabled path** — more to do nothing than these cost to work.

Recall quality is measured against a frozen 2,718-fact corpus with 20 graded queries, so a ranking change reports a delta instead of a feeling. It has already paid for itself: the embedded reranker looked like a win until a temporal bug was fixed, after which it showed **+0.013 nDCG@10 with recall@10 going 0.044 negative** — which is why it ships off.

[Every number, per-arm, with conditions and caveats →](docs/performance.md)

## What it actually buys — including where it lost

Against the Python system it replaced: **13 better / 5 worse / 1 equivalent** on a blind recall-quality panel, 7.1ms instead of ~51ms, and 0.845ms of hook per turn against a disabled path that cost 33ms.

On its own terms the record is mixed, and the losses are in the repository next to the wins:

- **Few raw facts are unique to it.** A census of 60 sampled memories found **0** stating anything not already on disk. That census can only judge *raw facts* — it asks whether a fact's terms are on disk, which an observation synthesised from forty sessions passes trivially while its synthesis exists nowhere.
- **Substitution did not show up.** On questions whose answers were already in the repo, the memory arm was **11–7 worse** on a blind panel and spent **+5% tokens** — while finishing **25% faster**.
- **Injection is a cost**: 1,325 tokens and 18 memories per turn. The ingest caps save 57.4% of extraction input, but that is the local LLM's input, not yours.
- **A synthesis can be confidently wrong.** One of the first four mental models cited 17 nodes to assert a gate was awaiting a signature it had received ten days earlier. Every sentence in it had been true when written.

Those four agree on one thing: **the value measured so far is retrieval, not storage.** It surfaces the paragraph of yours that answers the question — including, once, a line in `book/src/roadmap.md` that reopened a month-old crash investigation because nobody was going to grep for it.

The tier above raw facts has since been measured and repaired: distillation is worth **+27% recall@10**, and mental models — which had never once run, because every call failed on a grammar limit and nothing called them anyway — now run on four. The store also learned to **retire** a fact, so a superseded memory stops reaching the synthesiser at all. Noticing a retraction *automatically* is the part that does not work: the detector found 2 of 3 targets and 22 things that were not retractions, and [ships off](docs/evidence/supersession-detection.md).

Every one of those stores what **was** true. What none of them holds is what is being worked on right now — the open goal, what is done, what is blocked, the next action — so a session resuming after a break gets similar facts and no answer to "where was I". Schema v12 adds that tier as a **task ledger**, one row per bank, and **nothing reads it yet**: the rows accumulate so their content can be judged before any of it is injected. Given that memory lost 11–7 on substitution, "the extractor writes something worth injecting" is a claim to check, not to assume.

[The full analysis, with what is still unmeasured →](docs/benefits.md)

## Getting started

Requires Rust and a running [Ollama](https://ollama.com) with an extraction model (default `qwen3-14b-nothink`).

```bash
cargo install --path crates/memgardend --bin memgardend    # the daemon
cargo install --path crates/memgarden-cli --bin memgarden  # the hook binary

cp config.example.toml ~/.config/memgarden/config.toml     # env overrides: MEMGARDEN_*
memgardend                                                  # 127.0.0.1:9100
```

Wire it into Claude Code. **Installing turns nothing on** — the default install wires all four events in *shadow* mode: real daemon calls, real latency, nothing reaches the model. Flip `[hooks] mode = "full"` in `config.toml` when you want injection, and back to `"shadow"` to roll it back in one line.

```bash
memgarden hooks install --dry-run   # print the exact lines, write nothing
memgarden hooks install             # shadow mode
memgarden hooks status              # what is wired per event, daemon health, poisoned sessions
memgarden hooks uninstall           # restores settings.json to its pre-install bytes
```

`settings.json` is edited by line splice and never reserialized, so uninstall restores it byte for byte — a `serde_json` round-trip would silently re-sort every key in a file shared with other tools. And the hook **can never exit 2**, because on `UserPromptSubmit` exit 2 erases what you typed: no `clap`, no `?` out of `main`, and a panic hook that exits 0 with empty stdout.

The web UI is at `http://127.0.0.1:9100/ui/` — dashboard, benefit ledger, and the memory graph.

**Coming from the old Python system?** Only then: [migration runbook](docs/runbook-migration.md). A fresh install skips it and just lets the bank fill.

## Day to day

Nothing here is required — the bank fills on its own. These are the three things an operator actually reaches for.

**Keep a turn out of it.** Wrap it in `<private>…</private>` and retain drops it before extraction reads it. The marker is exact and lower-case, it covers message text rather than tool payloads, and an **unclosed** `<private>` discards the rest of that message rather than storing it. It works at retain time only: it cannot unwrite what an earlier retain already stored.

**Retire a fact that is no longer true.** A superseded memory is retired rather than edited, and every retrieval path — `/reflect` and the mental-model refresh included — then stops serving it. The row and its graph edges stay.

```bash
curl -X POST   .../v1/banks/{bank}/nodes/{id}/supersede -d '{"by": <newer node id>}'
curl -X DELETE .../v1/banks/{bank}/nodes/{id}/supersede    # put it back
```

Noticing this automatically, during extraction, was built and measured: it found 2 of 3 targets and 22 things that were not retractions, so it [ships off](docs/evidence/supersession-detection.md).

**Turn off a mental model that has gone wrong.** A synthesis refreshes on a schedule and reads as authoritative, so stop the schedule first and decide about the content after. The model, its content and its citations stay.

```bash
curl -X DELETE .../v1/banks/{bank}/mental-models/{mm_id}/trigger
```

[More, including what to check when something looks wrong →](docs/runbook-hooks.md)

## Development

```bash
cargo test --workspace                       # full suite (unit + API + schema)
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p memgardend                      # daemon on 127.0.0.1:9100
cargo test -p memgardend -- --ignored        # live tests (need Ollama running)
cargo run -p memgarden-cli --bin hook_bench  # hook latency, interleaved-paired
./scripts/hook-budget.sh                     # binary size, ldd set, dependency closure
mdbook serve book                            # the wiki, at http://localhost:3000
```

Work lands as PRD-tracked pull requests (template in `.github/`), each 3-way reviewed (functional / security / code). Fixes are confirmed by **mutation-testing them** — reverting the fix has to fail a test, or the test does not count.

The daemon creates its data dir `0700` and speaks plain HTTP on loopback with a Host-header guard. It is a single-user local tool by design.

## Documentation

- **[Wiki](book/src/introduction.md)** — install, usage, how it works, extending it, roadmap. English canonical, Korean alongside. `mdbook serve book`.
- **[Architecture](docs/architecture.md)** — the decision table and the case for rebuilding
- **[Benefits](docs/benefits.md)** — every measurement of what it buys, in one place, including the ones that came out against it
- **[Performance](docs/performance.md)** — every measured number, with conditions
- **[Status](docs/status.md)** — phases, cutover gates, and what is still open
- **[Migration runbook](docs/runbook-migration.md)** · **[hooks runbook](docs/runbook-hooks.md)**
- `docs/design/` — one design note per merged PR, each standing alone without the diff
- `docs/evidence/` — the measurements the acceptance criteria were signed on, including the ones that were retracted, and [why write-time supersession detection ships off](docs/evidence/supersession-detection.md)
- `docs/parity-gaps.md` — legacy behaviour deliberately not ported, each row with the fact that would reopen it
- `docs/PRD.md` — the deep-interview spec this repo executes

## Repo map

- `crates/memgarden-core` — types, config, lock-free metrics
- `crates/memgarden-store` — SQLite layer (migrations, vec/FTS, banks/nodes/search/ledger)
- `crates/memgardend` — the daemon (routes, Ollama client, extraction, retain pipeline, embed worker) and `mg_migrate`, the one-way legacy migration tool
- `crates/memgarden-cli` — the `memgarden` hook binary; dependency-closure-checked in CI, because a hook pays for its binary on every invocation
- `gold/` — the frozen recall-quality corpus, graded queries, and the results ledger
- `book/` — the wiki, built with mdBook

## Lineage

- [memgarden-legacy](https://github.com/ohora23/memgarden-legacy) — Python-era system: role-split architecture, ops runbook, fork patches, memdash/memcompare tooling, and the measurement framework whose numbers set this rebuild's acceptance bars
- [ohora23/hindsight `claude-code-integration`](https://github.com/ohora23/hindsight) — the fork whose caps, tags, and profile presets are ported server-side here

## License

MIT — see [`LICENSE`](LICENSE). Vendored browser libraries keep their own notices in `crates/memgardend/ui/vendor/`.
