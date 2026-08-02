# CE-7 — Entities, links, co-occurrence and the graph recall arm (PR B5)

PRD: CE-7 (and AC-4's viewer data source). Plan: `phase-b-impl.md` §PR B5 +
Critic Revisions R2/R6/R13/R15 and NIT 18/24. Legacy references:
`hindsight-api-slim/hindsight_api/engine/` — `retain/entity_processing.py`,
`retain/entity_resolver.py` (`:684-717` scoring, `:80-93`/`:220-231`
co-occurrence), `retain/link_utils.py` (`:30`, `:92`, `:291`, `:377`,
`:394-395`), `causal_links.py:18`, `retain/orchestrator.py:1232`, and
`search/link_expansion_retrieval.py` for the expansion signals.

## What this adds

- **Migration `0003_entities_graph.sql`** — `entities.mention_count /
  first_seen / last_seen`, the `entity_cooccurrences` table (canonical
  `a < b` CHECK), `idx_links_to` for reverse traversal, and the deferred
  `links.weight` `[0,1]` CHECK (NIT 18), applied by rebuilding `links`.
- **`store/src/graph.rs`** — `load_resolution_context`, `write_entities`
  (upsert + `node_entities` + co-occurrence fold, one transaction),
  `insert_links`, `nodes_in_window`, `node_types`, `expand`, `graph_view`.
- **`memgardend/src/entities.rs`** — trim+lowercase normalization, a ported
  Ratcliff/Obershelp `ratio()` (Python's `SequenceMatcher.ratio()`, autojunk
  included), the three-term resolution score and the strict `> 0.6` gate.
- **`memgardend/src/links.rs`** — the temporal / causal / semantic rules as
  pure functions. These three are the *only* producers of a `NewLink`, which
  is what makes "retain never writes an `'entity'` row" checkable.
- **`memgardend/src/recall/graph.rs`** — the arm: seeds → 1 hop → ranked,
  capped at 200 nodes.
- **`GET /v1/banks/{id}/graph?limit=&types=&session=`** → `{nodes[], links[]}`.

## Pipeline

```
retain (write_facts)                 backlog worker (R2)
  facts -> resolve_fact -> entities    set_embeddings_batch
        -> write_entities                -> knn per node, same fact_type,
        -> causal_links                     sim >= 0.7, top 20
        -> temporal_links (24h window)   -> semantic links
        -> insert_links

recall
  pass 1: RRF(semantic, bm25, -, temporal) -> top-20 seeds
          -> expand: links both directions + node_entities co-membership
          -> rank(entity tanh + causal weight+1 + link weight), cap 200
          -> hydrate whatever pass 1 did not already load
  pass 2: RRF(semantic, bm25, graph, temporal)   <- first occurrence decided here
```

## Key decisions

| Decision | Why |
|---|---|
| Candidates = every entity in the bank | R6. The plan's "FTS index over entity names" does not exist, and a prefix/trigram prefilter would miss exactly the near-matches resolution is for. A bank holds hundreds to a few thousand entities and this is one indexed scan per chunk, not per mention. `ponytail:` comment names ~10k as the upgrade point. |
| `entity_type` is never persisted | Legacy hardcodes `"CONCEPT"` for every LLM-extracted entity (`entity_processing.py:32`) and never reads it back. A column holding one constant is a column. |
| Semantic links are created after embedding, not at retain | R2. B3 writes `embedding = NULL`, so a retain-time KNN would find nothing forever. `embed_task::on_batch_embedded` is the hook — the same streaming placement legacy uses (`orchestrator.py:418-420`). |
| Two-pass RRF, graph seeded from pass 1 | R13. The graph arm needs candidates before it can expand, and first-occurrence attribution has to be decided once, over all four arms. Pass 1's output is used *only* to pick seeds. |
| Expansion excludes the seeds | Legacy does (`link_expansion_retrieval.py:626,670`). Including them would let densely-linked seeds consume the 200-node cap and crowd out the genuinely new 1-hop nodes the arm exists to find. |
| Graph score = entity + causal + link, summed | `link_expansion_retrieval.py:216-228`: `tanh(shared * 0.5)` for entity co-membership, `weight + 1.0` for causal (legacy's highest-quality signal), `weight` for semantic/temporal, each bucket keeping its max. A node reached by two signals outranks one reached by the strongest alone. |
| `CROSS JOIN` pins the expansion's join order | Left alone the planner drove from `memory_nodes` and scanned the whole bank partition: **15 ms** at 3k nodes vs **0.2 ms** with the seed list pinned outermost. `CROSS JOIN` is an ordering directive in SQLite, not a different join. |
| `links` is rebuilt rather than altered | SQLite has no `ADD CONSTRAINT`. Safe here because nothing has an FK *pointing at* `links`, and CE-7 is its first writer, so the copy moves zero rows in any deployed database (a pre-existing out-of-range weight would be clamped, not dropped — there is a test). |
| Temporal window loaded into Rust | One `event_date BETWEEN` query instead of legacy's LATERAL per-unit top-N. The window is 24 h — one session's facts — and the pairing is O(new × window) with `new` ≤ one chunk's facts. |
| `idx_node_entities_entity` added | Not in the plan's DDL, but the co-membership join is on `entity_id` and the `(node_id, entity_id)` PK cannot serve it. Legacy has the same index (`idx_unit_entities_entity_unit`). |

## Divergences from legacy

- **The canonical name is the display name.** Normalization is trim +
  lowercase, so `Ollama` comes back as `ollama` (NIT 24). Hangul has no case,
  so Korean names round-trip byte-identically — asserted at both the pure
  function and the store level.
- **No `pg_trgm` prefilter.** See R6 above.
- **`caused_by` only.** `causes` / `enables` / `prevents` remain valid
  `link_type`s (transfer-imported banks) and the graph arm boosts them the
  same way, but retain writes only the canonical form
  (`causal_links.py:11`).
- **`'entity'` link rows are never written**, by anything. Entity grounding
  lives in `node_entities`, which is what the expansion traverses.
- **No per-entity fan-out cap** on the co-membership join (legacy's LATERAL
  `graph_per_entity_limit = 200`). The outer `LIMIT` bounds the result, not
  the join; `ponytail:` comment names the upgrade.
- **Names are compared over `char`s, not graphemes.** Python's
  `SequenceMatcher` compares code points and legacy's scores come from that,
  so matching it matters more than being linguistically right about
  combining marks.

## Known limits

- **Name similarity alone can never resolve an entity.** `name_ratio * 0.5`
  peaks at 0.5, under the 0.6 threshold, so a near-name match needs the
  co-occurrence or temporal term to carry it. That is legacy's formula
  verbatim, not a porting slip: an *exact* name match never reaches the
  resolver at all, because it collides on `UNIQUE (bank_id, canonical_name)`.
  In practice retain always supplies a date, so the temporal term is live.
- **Resolution is per-chunk, not per-batch-then-flush.** Two facts in the
  same chunk resolve against the same snapshot of the bank, so a brand-new
  entity introduced by fact 1 cannot be the resolution target for fact 2 in
  that same chunk. It will be from the next chunk on.
- **`difflib` autojunk is ported but effectively unreachable** — entity names
  are capped at 256 chars upstream, and the heuristic needs 200+ with a
  repeated character. Included because omitting it would be a silent
  divergence on the longest names.

## Verification

`cargo test --workspace`: **267 passed, 0 failed, 5 ignored** — up from 234
at CE-6. The 5 ignored are the 3 pre-existing live/model tests, the CE-6
recall bench, and the new `graph_arm_bench`.
`cargo clippy --workspace --all-targets -- -D warnings` clean.

New coverage: `ratio()` against ten CPython `SequenceMatcher.ratio()`
reference values (Korean included) plus symmetry; normalization leaving
Hangul untouched; each resolution term weighted independently and the
threshold strict at exactly 0.6; R6's full-scan candidate recall; temporal
weight at 0/12/24/48 h, same-`fact_type`-only, bidirectional-within-batch and
the 20-per-node cap; the semantic threshold inclusive at 0.7, per-`fact_type`,
top-20, self-link-free; self and out-of-range causal targets dropped; the
graph rank's three signals adding and each bucket keeping its max, the entity
`tanh` saturation curve, and the 200-node cap; migration 0003 on a fresh DB
and upgrading a v2 DB with an out-of-range link weight in it; both new CHECKs
rejecting bad rows; `write_entities` counts/first_seen/last_seen/pairs across
two batches; Korean entity names round-tripping through the store and the
co-occurrence join; `expand` walking both directions, one hop, excluding
seeds, bank-scoped; `graph_view` with no dangling edges; the endpoint's type
/ session / limit filters, 404 and three 400s; the graph arm surfacing a
neighbour BM25 cannot reach (via a link and via a shared entity) and still
obeying the type and tag filters; and a full retain → entities →
co-occurrences → links round trip against a stub Ollama that asserts zero
`'entity'` rows and zero self-links.

### Measured — graph arm latency

`cargo test --release -p memgardend --test graph_api -- --ignored --nocapture graph_arm_bench`,
3000 nodes / 59,790 links / 3000 entity rows, 20 seeds, 200 samples:

```
graph arm @ 3000 nodes / 59790 links: p50 256us  p95 260us  max 269us
```

**0.26 ms against the plan's ≤5 ms** — 19x headroom. Before the `CROSS JOIN`
ordering fix the same bench measured **19.7 ms**; the plan's budget is what
caught it.

### Measured — AC-2 with the graph arm active

The CE-6 bench (`hybrid_recall_bench`) now seeds 59,790 links and 3000 entity
rows before the loop, so the arm does real work rather than returning empty.
Same shape as CE-6: real `bge-small-en-v1.5`, 3000 nodes, 2000 requests, five
rotating queries (one Korean).

```
             p50       p90       p95       p99       max     <35ms    <60ms
idle       7025us    7476us    7658us    8556us   32275us   2000/2000  2000/2000
loaded    19462us   44017us   49203us   58433us   66572us   1609/2000  1990/2000
```

`loaded` is R7's concurrent-ingest case (`MEMGARDEN_BENCH_LOAD=1`), which
wrote and embedded **35,832** extra nodes during the 47 s run.

**AC-2 (p50 ≤ 35 ms, p95 ≤ 60 ms) still holds in both.** Against CE-6's
numbers the graph arm costs **+1.0 ms p50 / +0.8 ms p95 idle** and
**+2.0 ms p50 / +7.3 ms p95 loaded** — the loaded delta is the second
`spawn_blocking` hop contending for the same write lock as the ingest, not
the queries themselves (0.26 ms, above).

### Manual verification

A live daemon (real Ollama `qwen3-14b-nothink`, real embedder) retaining one
two-message transcript naming Ollama, Jetson Xavier, VRAM, MemGarden and
BM25, then `GET /v1/banks/manual/graph`:

```
4 facts, 16 node_entities rows, 9 entities
  jetson xavier (3 mentions), ollama (3), bm25 (2), memgarden (2), vram (2), …
20 co-occurrence pairs, all canonically ordered
  (jetson xavier, ollama) 3   (bm25, memgarden) 2   (ollama, vram) 2   …
8 links: 4 temporal (weight 0.490), 4 semantic (0.802 / 0.843)
0 'entity' rows, 0 self-links
```

The semantic links are the R2 hook firing for real: they appear only after
the backlog worker's next tick, not at retain time. The temporal weight of
0.490 is correct rather than suspicious — the facts the LLM dated carry
`event_date = occurred_start` (midnight UTC) while the undated ones fall back
to `mentioned_at`, putting the pairs 12.25 h apart inside the 24 h window.
