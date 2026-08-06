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
| **D — Migration** | read-only legacy snapshot (MG-1a) ✅ · archive → SQLite importer (MG-1b) 🔄 · the AC-3 verifier (MG-2) ⏳ | 🔄 in progress |
| **E — UI & metrics** | dashboard, graph API, WebGL viewer (pan/zoom/drag, live SSE), ledger views | ⏳ |
| **F — Cutover** | run the AC-1..3 gates → shut the legacy system down → final record in the legacy repo | ⏳ |

Dependencies are `A → B → (C, D, E in parallel) → F`. The graph viewer needs
the link data from Phase B and nothing from C or D.

---

## The cutover gates

The old system is shut down when **all three** are met, and not before.

### AC-1 — quality parity — *collectable now, not yet collected*

Recall quality on a fixed query set (8 existing A/B log entries + 12 new) must
be at least equal to the current system, judged by the user.

The instrument exists: install the hooks in `shadow` mode alongside the legacy
ones and every prompt appends what MemGarden *would* have injected to
`shadow-recall.jsonl`, while the bank fills from the same real sessions. That
log plus the graded gold set is the evidence.

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

### AC-3 — lossless migration — 🔄 **the migration runs; the instrument that certifies it does not yet**

Node, link and document counts must match across the existing banks, plus a
50-sample content diff.

Two of Phase D's three PRs have landed the moving parts. `mg-migrate snapshot`
freezes legacy over read-only GETs and refuses on fifteen integrity properties;
`mg-migrate import` carries the archive into SQLite. Measured against legacy's
own frozen `/stats`, **four banks**: 25 == 25 documents, **5,288 == 5,288
nodes**, 200 == 200 authored causal links, 1,747 observations with 2,114
provenance edges.

**That is not AC-3 met.** AC-3 is a *verification* criterion, and MG-2 — the
three-tier reconciliation and the 50-sample content diff — has not been
written. Counts printed by the thing that wrote the rows are not evidence that
the rows are right; that is the whole reason the verifier is a separate PR with
a separate oracle. Phase F reads AC-3 from MG-2's report or not at all.

Two things the migration establishes that Phase F will need:

* **the numbers cannot all be equal, and the honest ones say so.** Authored
  `caused_by` edges transfer exactly. `temporal` and `semantic` are *rebuilt*,
  because a semantic edge is a function of an embedding space that is ours and
  not legacy's, and legacy's temporal neighbour query applies no 24-hour
  window where ours does. Legacy's four banks are also the last four: `entity`
  links exist in neither system's storage;
* **the migration is rehearsable at zero cost.** `--db <scratch>` builds a
  complete migrated database beside the live one with both daemons untouched,
  which is how the numbers above were taken.

Four further criteria (AC-4 graph viewer, AC-5 dashboard, AC-6 metrics, AC-7
PR discipline) are tracked but do not gate the shutdown.

---

## Known limits and open defects

Ordered by what blocks what.

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
