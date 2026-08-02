<!-- Hero image: generated externally, drop the file at assets/hero.png and uncomment.
<p align="center"><img src="assets/hero.png" alt="MemGarden — a tended garden of machine memory, grown in Rust" width="100%"></p>
-->

# MemGarden

A ground-up **Rust rebuild** of my personal long-term memory system for Claude Code — local-LLM fact extraction, hybrid recall, an interactive memory graph, and overhead-free savings metrics, all in one self-contained daemon.

Like a garden, memory here is not just stored but **tended**: sessions rain down through hooks, facts take root in per-project banks, consolidation prunes and grafts them into knowledge, and a ledger keeps honest books on what the garden actually yields. The [Python-era system](https://github.com/ohora23/memgarden-legacy) proved the concept and produced the measurements; this repo replaces its entire automatic layer with a single Rust binary.

## Why rebuild

The legacy stack (hindsight daemon + embedded Postgres + Python hooks) works, but every operational lesson it taught pointed the same direction:

- **Latency came from process sprawl** — 830ms recalls traced to GPU contention and a per-request `gc.collect` in a sidecar; fixed by config, but the lesson is architectural: fewer moving parts, no per-request penalties by construction.
- **Restart races** (embedded Postgres vs daemon) disappear when storage is an in-process SQLite file.
- **Unbounded inputs break things** — a 102MB transcript blew a retain wall-clock; an unbounded consolidation prompt outgrew the model context. Every MemGarden ingest path is capped by design, server-side.
- **Benefits must be measured, not vibed** — the [3-layer measurement framework](https://github.com/ohora23/memgarden-legacy/blob/master/docs/measurement.md) needs zero-latency counters built in, not bolted on.

## Architecture

```
Claude Code hooks (Phase C: Rust subcommands, <10ms)
        │ raw transcript / query
        ▼
memgardend (axum, :9100) ──────────── web UI (dashboard + graph viewer, Phase E)
 ├─ retain: caps → chunk → Ollama extraction → facts + entities + links
 ├─ recall: FTS5 BM25 + sqlite-vec KNN → RRF fusion → token budget
 ├─ consolidate / reflect (Phase B tail)
 ├─ in-binary embeddings (fastembed, bge-small 384-dim, CPU)
 └─ metrics: lock-free atomics (19.4ns/op) + benefit ledger
        ▼
 single SQLite file (WAL, STRICT) — vec0 vectors + FTS5 + graph tables
```

| Decision | Choice | Why |
|---|---|---|
| Storage | SQLite + sqlite-vec (`=0.1.9`) + FTS5, external processes: **zero** | No restart races, one-file backup, brute-force KNN is 1–3ms at this scale |
| Extraction LLM | Ollama HTTP (default `qwen3-14b-nothink`), swappable | GPU belongs to the big model; the daemon never competes for it |
| Embeddings / rerank | In-binary, CPU-forced | Measured 4.5/12.8ms on CPU; VRAM contention was the #1 legacy latency bug |
| Korean search | FTS5 `unicode61` + `prefix='2 3 4'` + `*`-suffixed query terms | CJK has no word boundaries; guard-tested so recall can't silently degrade |
| Concurrency | One Ollama permit, background/interactive acquire split, hard deadlines | A 14B model on one GPU must be queued for, never raced for |
| Metrics | Static atomics, no LLM calls, `/metrics.json` + `benefit_ledger` table | Zero added latency on hot paths is an acceptance gate, not a hope |

## Status

Work lands as PRD-tracked pull requests (template in `.github/`), each 3-way reviewed (functional / security / code) before merge.

| Phase | Scope | State |
|---|---|---|
| A — Foundation | workspace/CI, SQLite schema, REST skeleton, metrics plumbing (CE-1..3, MX-1) | ✅ merged |
| B — Core pipeline | embeddings CE-4 ✅ · Ollama extraction CE-5a ✅ · retain ingest CE-5b ✅ · hybrid recall CE-6 🔄 · entities/graph CE-7 · temporal CE-8 · consolidation CE-9 · reflect CE-10 · reranker CE-11 | 🔄 in progress |
| C — Hooks | 4 Rust hook subcommands, global settings switch | ⏳ |
| D — Migration | Postgres → SQLite exporter, lossless-verification script | ⏳ |
| E — UI & metrics | dashboard, graph API, WebGL viewer (pan/zoom/drag, live SSE), ledger views | ⏳ |
| F — Cutover | quality-parity A/B + performance gates + lossless migration → legacy shutdown | ⏳ |

**Cutover gates (AC-1..3):** recall quality ≥ legacy on a fixed query set (human-judged), recall p50 ≤35ms / p95 ≤60ms with hook overhead <10ms, and node/link/document counts + 50-sample content diff proving lossless migration of the 3 existing banks.

## Development

```bash
cargo test --workspace                       # full suite (unit + API + schema)
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p memgardend                      # daemon on 127.0.0.1:9100
cargo test -p memgardend -- --ignored        # live tests (need Ollama running)
```

Configuration: `config.example.toml` → `~/.config/memgarden/config.toml`, env overrides `MEMGARDEN_*`. The daemon creates its data dir `0700` and speaks plain HTTP on loopback with a Host-header guard — it is a single-user local tool by design.

## Repo map

- `crates/memgarden-core` — types, config, lock-free metrics
- `crates/memgarden-store` — SQLite layer (migrations, vec/FTS, banks/nodes/search/ledger)
- `crates/memgardend` — the daemon (routes, Ollama client, extraction, retain pipeline, embed worker)
- `docs/PRD.md` — the deep-interview product spec this repo executes
- `docs/design/` — one design note per merged PR (mirrored to my Obsidian vault)

## Lineage

- [memgarden-legacy](https://github.com/ohora23/memgarden-legacy) — Python-era system: role-split architecture, ops runbook, fork patches, memdash/memcompare tooling, and the measurement framework whose numbers set this rebuild's acceptance bars
- [ohora23/hindsight `claude-code-integration`](https://github.com/ohora23/hindsight) — the fork whose caps, tags, and profile presets are ported server-side here
