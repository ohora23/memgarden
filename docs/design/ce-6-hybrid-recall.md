# CE-6 — Hybrid recall: BM25 + vector, RRF, budget (PR B4)

PRD: CE-6 + the MX-1 AC-6 deferral. Plan: `phase-b-impl.md` §PR B4 +
Critic Revisions R1/R7/R8/R12 and NIT-20/21/22. Legacy references:
`hindsight-api-slim/hindsight_api/engine/` (`memory_engine.py`,
`search/{fusion,reranking,retrieval,tags}.py`) and the fork hook's
`hindsight-integrations/claude-code/scripts/` (`recall.py`, `lib/content.py`)
for the injection format.

## What this adds

- **`store/src/search.rs`** — `fts_query_string` now joins with **` OR `**
  and keeps the 12 longest terms (Critic Revision R1);
  `fts_candidates_filtered` adds a `fact_type` restriction and returns the
  raw `bm25()` score; `hydrate` loads every candidate's row + tags in one
  `json_each` query (no N+1).
- **`memgardend/src/recall/`** — `fusion.rs` (RRF, `k = 60`, 1-based ranks,
  fixed arm order, per-arm cap), `scoring.rs` (passthrough base, recency,
  the three multiplicative boosts), `budget.rs` (`low/mid/high` →
  100/300/1000, greedy cl100k fit), and `mod.rs` (the pipeline, tag
  matching, `injected_text`).
- **`POST /v1/banks/{id}/recall`** → `{results[], injected_text, counts}`.
  Each result carries `scores {final, semantic, keyword, rrf, recency,
  temporal, proof}`.
- **`[recall]` config** — `types` (default all three), `limit` (20),
  `max_tokens` (1024), `cap_per_source` (0 = off), `preamble`.
- **`ApiJson<T>`** (`src/json.rs`) — every JSON body now rejects into the
  `{"error":{code,message}}` envelope instead of axum's plain-text 422.
- **Metrics**: `recall_requests`, `recall_errors`, `recall_latency`,
  `recall_injected_tokens`, `recall_injected_memories` now move.

## Pipeline

```
query --(<5 chars? -> empty)--> [embed] --> KNN(over-fetch)  --\
                            \-> fts_query_string -> FTS(bm25) --+-> hydrate (1 query)
                                                                 |
   type+tag filter (Rust, one impl for both arms) <--------------/
                     |
   cap_per_source -> RRF(k=60) -> passthrough base -> recency/temporal/proof boosts
                     |
   sort -> truncate(budget*2) -> greedy cl100k fit(max_tokens) -> truncate(limit) -> injected_text
```

## Key decisions

| Decision | Why |
|---|---|
| `fts_query_string` joins with `OR`, capped at 12 terms | R1. Whitespace is an implicit AND in FTS5: a 16-token English prompt measured **0** hits, a 5-token Korean one **0**. OR restores them (100 hits in the same fixture) and `bm25()` still ranks multi-term matches first. The cap keeps the expression bounded; longest-first because longer terms are more selective. |
| `fact_type` filtered in SQL, tags filtered in Rust | vec0 partitions on `bank_id` only, so the semantic arm cannot filter tags in SQL at all. One Rust implementation of the four tag modes beats two dialects of the same semantics; the over-fetch absorbs the post-filter. `fact_type` still goes to SQL for the FTS arm because an excluded type would otherwise eat the `LIMIT`. |
| Missing embedder degrades to BM25-only, not 503 | The embedder is absent while loading, when disabled, or after a load failure. A keyword-only answer beats no answer — and FTS is the arm that carries Korean (Phase A decision #7). |
| Sub-5-char query returns 200 with zero results | The Phase C hook fires on every prompt; a 400 there would be pure noise. Legacy's hook makes the same call client-side (`recall.py:128`). |
| One `spawn_blocking` for all DB work | KNN + FTS + hydrate share the blocking hop; only the query embedding gets its own (it must finish first). |
| Budget filter `break`s instead of skipping | Ported quirk (`memory_engine.py:5915-5919`): one long fact truncates everything after it. AC-1 compares MemGarden's injections against legacy's, so the behaviour has to match, not improve. |
| `injected_text` built server-side | Keeps the Phase C hook thin (plan §Workspace decision) — the hook pastes one string. |
| `temporal`/`proof` ship as `0.5` stubs | R12: the *fields* exist now so CE-8/CE-9 fill values without another response-shape change (and without regenerating the `injected_text` fixture). |
| `budget` and `max_tokens` are separate knobs | Legacy sends both: `budget` sizes the rerank depth (`rerank_limit = thinking_budget * 2`), `max_tokens` caps the injection (`recall.py:190`, default 1024). Collapsing them made the fork's own coding profile (`budget = "low"` + 1024) clip the injection to 100 tokens, which would have invalidated the AC-1 A/B. |
| Recalled text is defanged before injection | Fact text is model-extracted from a transcript, so anything that reaches a retained conversation reaches a fact. A fact containing `</memgarden_memories>` would close the container early and everything after it would read as out-of-band instruction. A zero-width space after the `<` keeps it legible and harmless. |
| `recall_injected_tokens` counts the whole block, `counts.tokens` counts text only | The meter should say what the client actually pays for (framing + preamble + timestamp included). `counts.tokens` stays text-only because that is the number the budget enforces and the number legacy reports — AC-1 compares those. |
| `hydrate` reads tags from a second query | No character is safe as a `group_concat` separator when the values are user-supplied tags. Two statements, still no N+1, and `ORDER BY tag` makes response bodies deterministic. |
| No `metrics-off` cargo feature | R8, see AC-6 below. |

## Divergences from legacy

- **`recallTypes` defaults to all three.** Legacy's client default is
  `["observation"]` (`lib/config.py:16`); observation-only recall measurably
  degraded results and the live user overrode it, so that override is the
  server default here (`docs/measurement.md`, memcompare findings).
- **Tag mode `exact`** (observation scope equality) is not ported — nothing
  in Phase B requests scopes. `any` / `all` / `any_strict` / `all_strict` are.
- **`mentioned_at` renders as `%Y-%m-%d %H:%M UTC`** in `injected_text`
  rather than legacy's ISO string, matching the "Current time" line directly
  above it. MemGarden owns both ends of this string.
- **Graph and temporal arms are empty** in this PR; the 4-slot arm order is
  already fixed so CE-7/CE-8 slot in without re-ranking anything.
- **The passthrough reranker is the only path.** No cross-encoder ships, so
  `is_passthrough_reranker` is effectively always true; the branch itself is
  not ported.
- **Over-fetch is derived from `limit`, not from the budget.** Legacy fetches
  `max(limit * 5, 100)` where its `limit` is the *thinking budget*, i.e.
  ~1500 candidates per arm at `mid`. MemGarden derives it from the response
  `limit` (20 → 100/arm). Declared, not accidental: the KNN here is a
  brute-force partition scan rather than HNSW, so a 15x over-fetch buys
  candidates the budget can never return while paying the scan for them, and
  the bench shows the narrower fetch already saturates recall at this scale.
  Revisit together with the ANN upgrade if a recall-quality gap shows up in
  the AC-1 comparison.
- **`min_semantic` / `min_keyword` score floors are not ported**
  (`retrieval.py:220-222`, `semantic_min_similarity` / `bm25_min_score`).
  Legacy prunes weak matches inside the SQL arms; MemGarden does not, because
  the two score scales are not comparable across backends (pgvector cosine vs
  vec0 cosine, Postgres `ts_rank_cd` vs SQLite `bm25()` — the latter is
  *negative*), so porting the legacy thresholds numerically would prune the
  wrong things. RRF is rank-based and therefore indifferent to the absolute
  scale; a weak candidate simply ranks last and the token budget drops it.
  Add real floors only with MemGarden-calibrated numbers.
- **No `knn_filtered` wrapper.** The plan specified one as "a thin wrapper on
  `knn` with the over-fetch `k` computed by the caller" — that is `knn`. The
  caller computes `k`.
- **`mentioned_at` is rendered, not echoed.** AC-1 compares recall *behaviour*
  (which memories surface, how many tokens they cost), not the byte layout of
  a date inside the block, and MemGarden owns both the producer and the
  consumer of this string. Matching the "Current time" line directly above it
  is worth more than matching legacy's ISO form.

## Known limits (recorded per NIT-21/22)

- **The <5-character skip is a character count.** A dense CJK query clears it
  trivially; a genuinely meaningful 4-character English one ("RRF?") is
  skipped. Kept as legacy wrote it because AC-1 compares injection behaviour.
- **`recall_substitution` ledger rows are manual in v1** (NIT-22). Recall
  cannot know that an injected memory replaced work the model would
  otherwise have done; only `retain_cap_saving` auto-populates.
- **Tag filtering runs after the arms' `LIMIT`.** The over-fetch (≥100/arm
  against a `limit` of 20) gives it headroom, but "absorbs it" would be too
  strong a claim: a selective tag filter over a large bank *can* leave fewer
  than `limit` results even though matching nodes exist beyond the fetch
  window, and nothing detects that today. It is bounded, not solved. The fix
  is pushing the filter into SQL for the FTS arm and into a pre-filtered
  partition for the vector arm; flagged with a `ponytail:` comment at
  `search.rs`.
- **Brute-force KNN is the scaling ceiling.** p95 9.7ms at 3k nodes vs 40.7ms
  at ~32k, against a 60ms budget — see the `ponytail:` comment on
  `search::knn` for the upgrade path (~50k nodes).

## Verification

`cargo test --workspace`: **234 passed, 0 failed, 4 ignored** — up from 189
at CE-5b. The 4 ignored are the 3 pre-existing live/model tests plus the new
`hybrid_recall_bench`.
`cargo clippy --workspace --all-targets -- -D warnings` clean.

New coverage: Korean recall end-to-end through `fts_query_string` (single
token and 5-token), a >12-token English query, the R1 multi-token guard at
the store level, the Phase A compound-negative guard **unchanged**, exact RRF
arithmetic (incl. a doc in three arms and arm-order-dependent attribution),
passthrough base at n=0/1/2/11, recency at 0/182/365/400 days plus null and
future dates, the budget filter's break-not-skip boundary, `recallTypes`
filtering and its 400s, all four tag modes, sub-5-char and punctuation-only
short circuits, `injected_text` byte-exact against a fixture with an injected
clock, `[recall]` config precedence and validation, and the JSON-rejection
envelope.

Added in the 3-way review round: FTS5 bareword operators (`cats AND dogs`
and friends) both as `fts_query_string` unit assertions and as live 200s;
`budget` vs `maxTokens` proven independent (all three budget levels return
the full fixture at the default ceiling) plus `maxTokens` validation; a fact
carrying `</memgarden_memories>` round-tripping inside the block without
closing it; oversized-preamble rejection; whitespace-trimmed query gating;
`hydrate` bank scoping and a tag containing U+001F surviving whole; and
every retain tag source (session id, `file:` tags, caller tags) going
through one sanitizer with one shared count cap.

### AC-2 — recall latency

`cargo test --release -p memgardend --test recall_api -- --ignored --nocapture hybrid_recall_bench`,
both arms live (real `bge-small-en-v1.5`), 3000 nodes, 2000 requests, five
rotating queries (one Korean):

```
idle:   p50  5993us  p90  6534us  p95  6828us  p99  7671us  max 32296us  (2000/2000 under 35ms)
loaded: p50 17495us  p90 37968us  p95 41896us  p99 47381us  max 61253us  (1727/2000 under 35ms,
                                                                          1999/2000 under 60ms)
```

Percentiles are the bench's own samples, not the process-global
`recall_latency` histogram — that static is cumulative across every test in
the binary, so its quantiles are the bench's only by accident. Timing the
whole `oneshot` round trip also makes these a slight over-estimate of the
handler, which is the right direction for a gate.

`loaded` is R7's concurrent-ingest case (`MEMGARDEN_BENCH_LOAD=1`): a
background task writes and embeds nodes into the same bank for the whole
loop, contending on the single ONNX mutex (R9) and the SQLite write lock. It
wrote **30,360** extra nodes during the 40s run, so by the end the bank held
~33k nodes — the loaded number is simultaneously a 10x-scale number, and the
brute-force KNN scan is most of the gap between the two rows.

**AC-2 (p50 ≤ 35ms, p95 ≤ 60ms) holds in both: ~6x headroom idle, ~1.4x under
a write load heavier than any real session produces.** One loaded sample of
2000 did cross 60ms (max 61.3ms) — the gate is on p50/p95, but that tail is
the brute-force scan and is the honest reason the `ponytail:` ceiling on
`search::knn` names ~50k nodes as the upgrade point.

The bench runs on a **file-backed** temp DB, unlike the hermetic tests. That
is not cosmetic: production is a file and therefore in WAL, where a reader
never blocks a writer, while the shared-cache `:memory:` pool the other tests
use has no WAL and returns an immediate table-lock error under exactly this
concurrency. Measuring on `:memory:` would have measured an artifact.

### AC-6 — metrics cost on the recall path

Critic Revision R8 dropped the planned `metrics-off` cargo feature: Phase A
deliberately removed the wrapper layer the `#[cfg]`s would have hung off, and
a wall-clock A/B of a ~7ms request against a sub-microsecond effect is
statistically meaningless — run-to-run noise is three orders of magnitude
larger. The measurable quantity is the recording sequence itself
(`memgarden-core/tests/metrics_overhead.rs`, release):

```
record_us                         = 74.3 ns/op   (MX-1's gate: < 100)
full recall-path metrics sequence = 87.7 ns/request
```

The sequence is every site one `POST /recall` touches: `http_requests` +
`http_latency` (middleware), `recall_requests` + `recall_latency` (route),
`recall_injected_tokens` + `recall_injected_memories` (pipeline) — 4 counters
and 2 histograms. "Metrics off" is exactly zero of that, so **88 ns is the
on/off delta**: 0.00025% of the 35ms p50 SLO, and 0.0015% of the measured
5.99ms p50. AC-6 is closed.

(`recall_injected_tokens` now counts the whole injected block rather than
just the fact text — the meter reports what the client pays for, while
`counts.tokens` stays text-only for AC-1 parity. Same number of atomic
operations either way, so the measurement above is unchanged.)
