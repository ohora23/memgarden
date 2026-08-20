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
| Graph score = entity + causal + link, summed, **no causal boost** | `link_expansion_retrieval.py:216-241`: `tanh(shared * 0.5)` for entity co-membership and the bare `weight` for causal and semantic/temporal, each bucket keeping its max. The module docstring at `:16` claims a `+1.0` causal boost; the query it documents does not have one (`db/ops_postgresql.py:695` selects `ml.weight AS score`, `:239` takes it verbatim), so the docstring is stale. The first draft of this PR trusted it, which inverted the very principle the ranking exists for: retain writes `caused_by` at exactly 1.0, so `+1.0` puts every bare causal neighbour (2.0) above every convergent-evidence node (3 shared entities 0.91 + semantic 0.95 = 1.86). Test `causal_carries_no_score_boost` pins both halves. |
| Entity fan-out capped at `mention_count <= 200` | The outer `LIMIT` lands *after* the `GROUP BY`, so it bounds output, not work: one hub entity makes the self-join `seeds × |entity|`. Measured 10.3 ms for one entity naming all 3000 nodes vs 0.10 ms uniform — alone over the 5 ms budget, and `normalize()` merging name variants is exactly what builds such buckets. `mention_count` (added by this migration, written in the same transaction as `node_entities`) is the cheap proxy; counting mentions rather than distinct nodes only ever makes the gate stricter. Legacy caps the same thing (`graph_per_entity_limit`), and for the same second reason: an entity on every node connects nothing to anything. |
| Every unbounded read got a ceiling | `MAX_RESOLUTION_CANDIDATES` 5000 (newest `last_seen` first), `MAX_COOCCURRENCE_PARTNERS` 64 per entity (ranked by count, `idx_entity_cooc_count`), `MAX_TEMPORAL_WINDOW_NODES` 20 000, `/graph` link query at `nodes × 50`, node text truncated to a 160-char label. None of these are reachable in a healthy bank; all of them are reachable in an old or a hostile one, and the resolver is `O(mentions × candidates)`. |
| Resolution runs a length prefilter first | `ratio <= 2 · min(len) / (len_a + len_b)`, and the other two terms cap at 0.5, so a candidate whose best conceivable total cannot clear 0.6 is skipped before the `O(n·m)` comparison. The bound is computed *through* `resolution_score` rather than inlined — `0.1 + 0.3 + 0.2` lands one ulp above 0.6 and an inlined `× 0.5 + 0.5` would have skipped a candidate that could win. An exhaustive test over length pairs 1..64 pins it. |
| Client `event_date` clamped at the boundary | It flows into `(a - b).abs()` in resolution and temporal linking, where `i64::MIN` overflows. Clamped to year 1 .. year 9999 in ms — the range SQLite's own date functions accept. |
| `CROSS JOIN` pins the expansion's join order | Left alone the planner drove from `memory_nodes` and scanned the whole bank partition: **15 ms** at 3k nodes vs **0.2 ms** with the seed list pinned outermost. `CROSS JOIN` is an ordering directive in SQLite, not a different join. |
| `links` is rebuilt rather than altered | SQLite has no `ADD CONSTRAINT`. Safe here because nothing has an FK *pointing at* `links`, and CE-7 is its first writer, so the copy moves zero rows in any deployed database (a pre-existing out-of-range weight would be clamped, not dropped — there is a test). |
| Temporal window loaded into Rust | One `event_date BETWEEN` query instead of legacy's LATERAL per-unit top-N. The window is 24 h — one session's facts — and the pairing is O(new × window) with `new` ≤ one chunk's facts. |
| `idx_node_entities_entity` added | Not in the plan's DDL, but the co-membership join is on `entity_id` and the `(node_id, entity_id)` PK cannot serve it. Legacy has the same index (`idx_unit_entities_entity_unit`). |

## Diverged from legacy

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
- **Causal edges are walked in both directions and seeds are excluded from
  every signal.** Legacy walks causal forward-only and excludes seeds from
  the semantic and entity CTEs but *not* the causal one
  (`db/ops_postgresql.py:696-703`). Both differences are deliberate: a
  `caused_by` edge is a claim about a relationship, and the fact that caused
  yours is as relevant to a query as the one yours caused; and letting seeds
  back in through one signal only would spend part of the 200-node cap
  re-ranking nodes the retrieval arms already returned.
- **The per-entity fan-out cap uses `mention_count`, not legacy's per-entity
  `row_number()`.** Same ceiling (200), one join instead of a window
  function, and it prunes before the aggregate rather than inside it.
- **Names are compared over `char`s, not graphemes.** Python's
  `SequenceMatcher` compares code points and legacy's scores come from that,
  so matching it matters more than being linguistically right about
  combining marks.

## Known limits

- **~~Name similarity alone can never resolve an entity.~~ It still cannot,
  but it is now allowed to veto.** `name_ratio * 0.5` peaks at 0.5, under the
  0.6 threshold, so a near-name match needs the co-occurrence or temporal term
  to carry it — legacy's formula verbatim, not a porting slip. This note filed
  it as a limit and let it stand. **Measured 2026-08-20 on the largest live
  bank, it is the resolver's dominant failure mode**: the two circumstantial
  terms also cap at exactly 0.5, so a mention needs a name ratio of only 0.2
  to merge when they max out, and 26% of the bank's 2,406 replayable merges
  rest on a similarity below 0.5 — `ollama` into `ddl`, `llm` into `legacy`.
  A further 130 sit above 0.7, where no floor reaches them, because the
  character carrying the meaning is the one not shared: `ce-11` into `ce-9`,
  `version 0.7.4` into `version 0.7.5`.

  `resolve_fact` now applies two gates before scoring — **a 0.5 name floor**
  and **differing digits reject**, compared as ordered runs — which block
  1,089 of the 2,406 (45%). Neither touches `resolution_score`, so the
  weights stay legacy's and the parity tests still assert them; what changed
  is that circumstance can no longer *create* an identity the name does not
  support. The remark that an exact match never reaches the resolver is also
  wrong for a migrated bank, which is what the 2026-08-09 short-circuit
  addressed. See `book/src/roadmap.md` for both measurements.

  **Not fixed:** the 1,317 surviving merges include wrong ones character
  similarity cannot detect — `security-reviewer` into `code-reviewer` at 0.67.
  That needs a signal the score does not have.
- **Truncation is silent at every ceiling above.** A bank that exceeds
  `MAX_TEMPORAL_WINDOW_NODES` inside one 24 h window loses the far end of it,
  and nothing reports that. Bounded, not solved — the same trade the CE-6
  note records for tag filtering.
- **Resolution is per-chunk, not per-batch-then-flush.** Two facts in the
  same chunk resolve against the same snapshot of the bank, so a brand-new
  entity introduced by fact 1 cannot be the resolution target for fact 2 in
  that same chunk. It will be from the next chunk on.
- **`difflib` autojunk is ported, and it is reachable.** Entity names are
  capped at 256 chars (`extract/parse.rs`), so the 200..=256 band is live —
  an earlier draft of this note called it "effectively unreachable" and was
  wrong. Worse, the port was initially only half done: autojunk drops
  elements into CPython's `bpopular`, *not* `bjunk`, so `isbjunk` stays false
  for them and the post-DP extension loops — which the first draft skipped as
  "no-ops without a junk predicate" — do extend across exactly those
  elements. `ratio("a"*250, "a"*250)` scored **0.0** against CPython's
  **1.0**. Both extension loops that matter are now ported and the test
  vectors include two that cross the gate.
- **`EDGE_FETCH_CAP` prunes by weight before the additive rank.** Legacy
  budgets after merging its three signals, so a node reachable by several
  weak-but-convergent edges can be cut here where legacy would have kept it.
  The cap is 4× the node cap, so it only bites on genuinely dense seeds; the
  fix, if it ever matters, is to rank inside SQL rather than fetch-then-rank.

## Verification

`cargo test --workspace`: **273 passed, 0 failed, 5 ignored** — up from 234 at
CE-6. The 5 ignored are the 3 pre-existing live/model tests, the CE-6 recall
bench, and the new `graph_arm_bench`.
`cargo clippy --workspace --all-targets -- -D warnings` clean.

New coverage: `ratio()` against thirteen CPython `SequenceMatcher.ratio()`
reference values — ten short ones (Korean included) plus three that cross the
200-element autojunk gate, which is where the half-ported version scored 0.0
against CPython's 1.0 — and symmetry on all of them; the length prefilter
proved exhaustively over length pairs 1..64 to reject only candidates that
could not have won; normalization leaving Hangul untouched; each resolution
term weighted independently and the threshold strict at exactly 0.6; R6's
full-scan candidate recall, and the newest-first bound on it; the
co-occurrence partner cap keeping the loaded view bounded while the table
stays complete; temporal weight at 0/12/24/48 h, same-`fact_type`-only,
bidirectional-within-batch and the 20-per-node cap; the semantic threshold
inclusive at 0.7, per-`fact_type`, top-20, self-link-free; self and
out-of-range causal targets dropped; the graph rank's three signals adding,
each bucket keeping its max, no causal boost, and convergent evidence beating
a bare causal edge; the entity `tanh` saturation curve and the 200-node cap;
the hub-entity fan-out gate at, below and above `MAX_ENTITY_FANOUT`;
migration 0003 on a fresh DB and upgrading a v2 DB with an out-of-range link
weight in it; both new CHECKs rejecting bad rows; `write_entities` counts and
**per-fact** first_seen/last_seen/pairs across two batches; Korean entity
names round-tripping through the store and the co-occurrence join; `expand`
walking both directions, one hop, excluding seeds, bank-scoped; `graph_view`
with no dangling edges; the endpoint's type / session / limit filters, 404
and three 400s; the graph arm surfacing a neighbour BM25 cannot reach (via a
link and via a shared entity) and still obeying the type and tag filters; a
full retain → entities → co-occurrences → links round trip against a stub
Ollama asserting zero `'entity'` rows and zero self-links; and R2's mandated
backlog-tick test.

**`backlog_tick_creates_semantic_links`** deserves its own line. Plan line 628
mandates "retain → backlog tick → semantic link exists" and the first draft
shipped without it: `on_batch_embedded` was reachable only through
`drain_once`, which needs a loaded 133MB model, so every test wrote embeddings
with `set_embeddings_batch` directly and skipped the hook entirely. The two
things that live in the hook rather than in `links::semantic_links` — the
`1.0 - distance` cosine conversion and the `TOP_K * 5` over-fetch — had zero
coverage. The test now drives the hook with hand-built vectors (no model) and
both mutations were confirmed to fail it: reading the distance as a
similarity, and dropping the over-fetch to a bare `TOP_K`.
`fusion::arm_slots_are_pinned_for_ce7_and_ce8` closes the matching gap for
R13's arm order, which nothing asserted — swapping `graph` and `temporal` in
`SOURCE_NAMES` used to leave the suite green.

### Measured — graph arm latency

`cargo test --release -p memgardend --test graph_api -- --ignored --nocapture graph_arm_bench`,
3000 nodes / 75,525 links (temporal + semantic + causal, so all three score
buckets are live) / 3000 entity rows, 20 seeds, 200 samples each:

```
graph arm [uniform] @ 3000 nodes / 75525 links: p50 292us p95 299us max 306us
graph arm [skewed ] @ 3000 nodes / 75525 links: p50 296us p95 300us max 307us
```

**0.29 ms against the plan's ≤5 ms.** Two distributions, because a uniform
one (300 entities × 10 nodes) is precisely the shape that hides an uncapped
fan-out: `skewed` adds a hub entity naming all 3000 nodes, which review
measured at **10.3 ms** — 100× and alone over budget — before
`MAX_ENTITY_FANOUT`. With the cap the two runs are within noise of each other.
An earlier `CROSS JOIN` ordering fix took the same bench from **19.7 ms** to
0.26 ms; the plan's budget caught both.

This number is **arm-internal**: the two expansion queries plus the ranking.
The real recall path additionally pays a `spawn_blocking` hop and a hydrate of
the newly-reached nodes, which is the end-to-end delta below.

### Measured — AC-2 with the graph arm active

The CE-6 bench (`hybrid_recall_bench`) now seeds 59,790 links and 3000 entity
rows before the loop, so the arm does real work rather than returning empty.
Real `bge-small-en-v1.5`, 3000 nodes, 2000 requests, five rotating queries
(one Korean).

```
             p50       p90       p95       p99       max     <35ms      <60ms
idle       7060us    7589us    7899us    8668us   31967us   2000/2000  2000/2000
loaded    19124us   43364us   48654us   57272us   65460us   1605/2000  1997/2000
```

`loaded` is R7's concurrent-ingest case (`MEMGARDEN_BENCH_LOAD=1`), which
wrote and embedded **35,840** extra nodes during the 46 s run.

**AC-2 (p50 ≤ 35 ms, p95 ≤ 60 ms) holds in both.** End-to-end cost of the
graph arm against CE-6: **+1.1 ms p50 / +1.1 ms p95 idle** and **+1.6 ms p50 /
+6.8 ms p95 loaded**. The loaded delta is not the arm's queries (0.29 ms) —
it is the second `spawn_blocking` hop competing for the 4-slot r2d2
connection pool against an ingest that is already holding connections.

**Trend worth recording.** Loaded headroom to the 60 ms gate fell from 18.1 ms
at CE-6 to 11.3 ms here, and the tail already grazes it (1997/2000 under
60 ms, max 65.5 ms). CE-8 adds a fourth arm; if it costs similarly, loaded p95
lands near 55 ms. The obvious lever is merging the graph arm's two hops (expand
+ hydrate) into one `spawn_blocking`, which halves its pool pressure —
recorded as a known lever, deliberately not taken now, because it is only
worth doing once B6 shows whether it is needed.

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

Note the Zipfian entity shape even in a four-fact bank: two entities at 3
mentions of 4 facts. That distribution is the argument for
`MAX_ENTITY_FANOUT`, not a hypothetical.

### The relink repair (`POST /v1/banks/{id}/relink`)

The fact_type oracle in `on_batch_embedded` was first built from the embedding
batch alone, which silently turned the fact_type filter into a *same-batch*
filter: every semantic edge joined two nodes of one `embedding.batch_size`
batch, and out-degree capped at exactly `batch_size - 1` against a
`SEMANTIC_LINK_TOP_K` of 20. Fixing the oracle only helps nodes embedded
*after* the fix, so every database built before it keeps the thin graph — the
live one showed 7,040 semantic links, out-degree max 7.

`relink` walks a bank's already-embedded nodes in keyset chunks of 500,
decodes each stored vector (`vecblob::decode`, the inverse of what
`set_embeddings_batch` wrote) and hands them back to the same
`on_batch_embedded` the backlog worker uses. Nothing is deleted first:
`graph::insert_links` is `ON CONFLICT DO NOTHING`, so the pass is purely
additive and a second run writes 0. `reindex` is *not* this — it rebuilds
`vec_nodes` from `memory_nodes.embedding` and leaves `links` untouched.

It also answers the narrower shape recorded in
`a_semantic_link_reaches_a_node_embedded_in_an_earlier_batch`: the pass only
writes edges *out of* the nodes handed to it, so in a growing bank an early
node's out-edges are fixed at the moment it drains. Relink is what lets a
settled node acquire edges to everything embedded since.

Live run, six banks, 5,377 embedded nodes:

```
semantic links   7,040 -> 92,417        (legacy PostgreSQL: 65,149)
out-degree max       7 -> 27            (20 own + reciprocal edges written by neighbours)
out-degree avg              17.78
nodes with >=1 out-edge      5,197 / 5,377
cross-fact_type 0 · cross-bank 0 · weight in [0.7, 1.0]
second run: {"nodes":65,"links_written":0}
```

// ponytail: synchronous like `reindex`, and unlike it this runs a k=100 KNN
per node — the live 3,200-node bank took seconds, but a bank large enough to
outlive the client's timeout wants 202 + a job id. It reads a chunk at a time,
so an interrupted run simply resumes on the next call.

**It buys no recall, and that was measured rather than assumed.** Relinking the
AX-2 gold database added 25,250 semantic edges (43,830 → 69,080, +58%) in 2.4 s
and every aggregate reproduced to the last floating-point digit — ledger line 12
against line 11. The graph arm is already saturated against
`GRAPH_EXPANSION_CAP = 200` before the relink, so the new edges, whose mean
weight is *higher* than the existing ones, feed it more of what it was already
discarding. See `ax-2-recall-quality.md`. The repair is worth shipping because a
database whose graph does not match what the code claims is a defect on its own
terms, not because it moves a number.
