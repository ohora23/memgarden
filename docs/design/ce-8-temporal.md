# CE-8 — Temporal parsing, the temporal recall arm, and proximity (PR B6)

Branch `feat/ce-8-temporal`. No migration: `event_date / occurred_start /
occurred_end / mentioned_at` and their indexes have existed since
`0001_init.sql:38-51`, and legacy has no bitemporal model to catch up with.

Legacy ports: `retain/fact_extraction.py:75-111` (`_infer_temporal_date`),
`retain/orchestrator.py:228-258` (ISO parsing), `temporal_periods.py` (the
period rules and the `NO_TEMPORAL_CONSTRAINT` sentinel),
`query_analyzer.py:228-258` (the call order), `search/retrieval.py:686-702`
(proximity).

## What this adds

* `memgardend/src/temporal/parse.rs` — ISO-8601 → unix ms (moved here from
  `retain/mod.rs`, one implementation) and `infer_temporal_date`: the 14
  relative expressions with their day offsets, truncated to midnight UTC,
  resolved against the retain job's `event_date`. Wired into
  `extract::parse::parse_facts`, which now takes the event date; it fires only
  for `fact_kind == "event"` facts the LLM left without an `occurred_start`,
  exactly as `fact_extraction.py:1517-1521` does.
* `memgardend/src/temporal/query.rs` — query-side constraint extraction, with
  three outcomes: a `[start, end]` range, `Unconstrainable`, or nothing.
* `memgarden_store::search::temporal_candidates` — the arm's entry predicate.
* `recall::scoring::{temporal_best_time, temporal_proximity}` — fills the
  `scores.temporal` slot CE-6 stubbed at 0.5.

## Pipeline

```text
recall(query, now)
  └─ extract_constraint(query, now)          ← pure string work, off the DB
       ├─ Range(start,end) → arm 3 is live, scores.temporal is real
       ├─ Unconstrainable  → no arm, scores.temporal = 0.5   (deliberate)
       └─ None             → no arm, scores.temporal = 0.5   (nothing said)
  └─ ONE spawn_blocking: knn + fts + temporal + hydrate(union of all three)
  └─ pass 1 RRF(sem, bm25, -, temporal) → graph seeds
  └─ spawn_blocking: graph expand + hydrate
  └─ pass 2 RRF(sem, bm25, graph, temporal) → score → budget
```

The temporal arm shares the existing blocking hop rather than adding a fourth
one — see *Measured* below for why that mattered.

## Key decisions

**The third state is load-bearing.** `Constraint::Unconstrainable` is legacy's
`NO_TEMPORAL_CONSTRAINT` (`temporal_periods.py:17-21`): a temporal expression
*was* recognized and deliberately has no range behind it. It short-circuits
before the fallback parser (`query_analyzer.py:230-231`), so "every Monday"
cannot become "whatever single date the fallback finds", and
`every monday, same as last week` cannot become last week's range. Producers:
recurrence markers (`every|each <time unit>`, `매일/매주/매달/격주/…`), an
open-ended `before <period>` / `<period> 이전`, which has no lower bound, and
`since <future period>`, which has no window at all
(`chinese_temporal_periods.py:451-454`). `since <past period>` /
`<period>부터` is the opposite case and *is* a range, `[period start, end of
today]` — legacy `since_constraint`, which closes on the reference *date*.

**An inverted window is never a score.** `since next week` would build
`start > end`; the SQL survives it (`BETWEEN` matches nothing) but
`temporal_proximity`'s zero-width shortcut did not — it returned 1.0, handing
every dated candidate a uniform +10% over every dateless one on a query where
the arm contributed nothing. Fixed in two places on purpose: the guard above
is the fix, and the shortcut is now spelled `start == end` with anything
backwards returning neutral, so a future regression upstream cannot re-inflate
scores.

**Casing.** Matching runs on the lowercased query; the fallback parser is
handed the **original** (`query_analyzer.py:228-229` vs `:253`).

**Three COALESCE orders, kept apart.** Documented as a table on
`scoring::temporal_best_time` and pinned by
`the_three_coalesce_orders_are_deliberately_different`, which names all three
and asserts the inputs where they disagree:

| order | site |
|---|---|
| `occurred_start ?? mentioned_at ?? occurred_end` | recency (`reranking.py:156`) |
| `occurred_start ?? mentioned_at` | the arm's SQL (plan PR B6) |
| `midpoint ?? occurred_start ?? occurred_end ?? mentioned_at` | proximity (`retrieval.py:686-693`) |

The arm-SQL leg of that test asserts in Rust; the SQL's own behaviour is
pinned by `search::tests::temporal_candidates_range_boundaries_and_coalesce_order`.

**`event_date` is never an entry predicate.** It exists for temporal-link
creation. A dedicated test seeds a node whose `event_date` is inside the
window and whose other dates are outside, and asserts the arm does not return
it.

**Korean is first-class, and the rule *order* is what makes it work.** English
phrases match on Unicode word boundaries (Python's `\b`, ported by hand — no
regex crate); Korean matches by containment, because it has no whitespace
boundary and `지난주에` is how the expression is actually written. That makes
rule order behaviour, in **both** directions:

* *suffix* — `지난주말` contains `지난주`, so every weekend rule runs before
  its week rule. The reverse of legacy's order, which is safe only because
  `\blast week\b` does not match inside "last weekend".
* *prefix* — `지지난주` contains `지난주`, `지지난달` contains `지난달`,
  `재작년` contains `작년`. Review round MEDIUM-1: these were **wrong
  answers, not supersets** — one period too late, with nothing to signal it.
  Legacy guards exactly this with `(?<![上下大小])` on every Chinese period
  rule (`chinese_temporal_periods.py:551,726,759,825`); with literal matching
  the equivalent is listing the longer form first, which is what
  `korean_double_prefix_is_not_swallowed_by_its_container` pins.
  `재작` on its own is deliberately *not* a literal — it would also match
  `재작업` ("rework").

## Diverged from legacy

* **The Chinese temporal module is deferred to Phase C+**, not dropped.
  `chinese_temporal_periods.py` is ~150 ordered rules / ~1,800 lines whose
  ordering is load-bearing; these banks hold no Chinese, so porting it buys
  nothing measurable and gets the ordering subtly wrong. Flagged in the module
  doc. Consequence: the `Unconstrainable` producers here are the English and
  Korean ones, since every legacy producer lives in that module.
* **No dateparser.** The fallback is an explicit ISO-8601 scan over the
  original query. Extended format only — `jiff` also accepts basic ISO, which
  would read a bare 8-digit node id as a date.
* **Additions to the period set**, beyond legacy's non-Chinese rules: the
  whole Korean column; the `this`/`next` rows (legacy's non-Chinese set is
  past-only, though its *Chinese* set has 这周/下周, so the concept is
  legacy-supported); English `tomorrow` and `day before yesterday`, neither of
  which legacy's non-Chinese set carries; and the before/since marker set
  itself, which in legacy exists only inside the Chinese module.
* **English recurrence adjectives are not markers.** `weekly`/`monthly`/… were
  in the sentinel set and are gone (review MEDIUM-3): they are adjectives on a
  noun, so "the weekly report from last week" discarded a window the query
  also states. Legacy's sentinel needs 每/各/隔 *bound to a unit*
  (`chinese_temporal_periods.py:540-548`), which is what `every|each + unit`
  ports. `from` is likewise not a since-marker — "notes from yesterday" means
  yesterday, and legacy has no `from` rule.
* **Month-name + year ("July 2026") is not ported.** Legacy carries a
  six-language month table for it; the ISO fallback covers the explicit-date
  case these banks actually produce.
* **The arm's entry predicate is narrower than legacy's.** Legacy uses a
  four-branch OR (`retrieval.py:624-633`): interval overlap, `mentioned_at` in
  window, `occurred_start` in window, `occurred_end` in window. The plan
  specifies the single `coalesce(occurred_start, mentioned_at) BETWEEN`, so
  that is what shipped — but it misses three shapes: (a) interval overlap, so
  a Jan→Dec fact is invisible to a July window; (b) end-only facts, which are
  reachable in practice (`extract::parse` sets `end = llm_end.or(start)`, so
  an LLM emitting only `occurred_end` produces one); (c) a fact whose
  `occurred_start` is outside the window but whose `mentioned_at` is inside —
  the exclusion `temporal_candidates_range_boundaries_and_coalesce_order`
  currently asserts *as intended*. Adopting the four-branch OR would also
  **retire the generated-column upgrade path below**: both of its branches are
  sargable against `idx_memory_nodes_occurred` and `idx_memory_nodes_mentioned`
  as they already exist, where the `coalesce()` is not.
* Legacy's per-fact-type temporal spreading BFS (`retrieval.py:704-760`) is
  not ported — RRF plus the CE-7 graph arm already does the "reach neighbours"
  job, on all fact types at once.

## Known limits

* No NFKC normalization of the query (legacy `temporal_periods.py:180`).
  Full-width digits and compatibility jamo will miss. No normalization crate
  is in the tree and no observed query needs one.
* On a Sunday, "last weekend" is yesterday-and-today — legacy walks back to
  the most recent Saturday (`:130-136`) and it is ported as written.
* `이번 주말` / `다음 주말` have no rule of their own, so they match
  `이번 주` / `다음 주` and answer with the whole week — a superset, not a
  wrong answer (unlike the prefix cases above, which were fixed).
* `from last week until yesterday` resolves to `Unconstrainable`: `yesterday`
  matches first and the word before it is `until`. Range-of-ranges is not
  supported by any rule here (or by legacy's non-Chinese set); rare enough to
  declare rather than build.
* Recurrence gaps, declared not fixed: `every 2 weeks` and `every other week`
  return `None` (the unit must sit immediately after `every|each`), and only
  the *first* `every` in a query is examined. Both are one more rule each if a
  real query needs them.
* **Scan ceiling.** The arm's `coalesce()` defeats `idx_memory_nodes_occurred`,
  so it is a bank-partition scan: 0.54 ms per query at 3k nodes, linear, which
  crosses the 3 ms budget at roughly **17k nodes** — under 6× current scale,
  and the loaded bench below writes 36k nodes in a single run. Two upgrade
  paths when that hits: a stored `effective_at` generated column with its own
  `(bank_id, effective_at)` index (the ponytail comment on the query), or
  legacy's four-branch OR above, which needs no new column because both of its
  branches already have indexes.

## Verification

`cargo test --workspace`: **298 passed, 0 failed, 6 ignored** — up from 273 at
CE-7. `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo fmt --all -- --check` clean.

New coverage: all 14 relative expressions at their offsets (15 literals —
legacy's `\btonigh?t\b` also matches the misspelling) plus midnight truncation,
first-match-wins ordering, and word boundaries; the Korean and English period
sets; the weekend-before-week ordering **and** the double-prefix forms
(`지지난주/지지난달/재작년/지지난주말`, each asserted to differ from its
container, with `재작업` asserted not to be a date); range end at the last
millisecond of its last day; `Unconstrainable` short-circuiting *before* both
the period rules and the fallback, with the same queries minus the recurrence
marker proving the assertion is not vacuous; `every` needing a time unit;
English recurrence adjectives *not* eating a stated window; before-vs-since,
`from` not being a since-marker, and `since <future period>` yielding the
sentinel rather than an inverted range; case-insensitive matching; the arm's
SQL boundaries, COALESCE order, bank scoping and `event_date` exclusion; the
three-COALESCE-orders test; proximity at 1.0/0.5/0.0, clamped, neutral for a
dateless node, the zero-width window, and neutral (never 1.0) for an inverted
one; `temporal_boost` at 0.0/0.5/1.0; end-to-end, the arm pulling in a node
BM25 cannot reach and that node disappearing when the temporal expression
leaves the query; the same in Korean.

### Measured — temporal arm latency

`cargo test --release -p memgardend --test recall_api -- --ignored --nocapture
temporal_arm_bench`, **in-memory** (the AC-2 bench below is file-backed, so
these two numbers are not directly comparable), 3000 nodes, half carrying
`occurred_start` and half only `mentioned_at` so both COALESCE branches are
exercised, 200 samples:

```
temporal arm [one week]   @ 3000 nodes:   72 hits, p50 125us p95 129us max 134us
temporal arm [whole year] @ 3000 nodes: 1000 hits, p50 540us p95 628us max 836us
```

**0.13 ms (0.54 ms worst case) against the plan's ≤3 ms.** The worst case is a
window covering the whole bank, where the `LIMIT` rather than the predicate is
what stops it — see the scan ceiling under Known limits for where that stops
being comfortable.

Legacy tried the same shape and abandoned it, for reasons worth not
rediscovering (`retrieval.py:595-603`): ordering the whole match set by
`COALESCE(...)` and keeping the N most recent biases results toward the end of
the window, degenerates when a retain batch stamps one date on everything (the
key stops discriminating and "most recent" becomes a near-random sample), and
degraded to a full scan with a disk-spilling sort — 30 s+ on a 660k-row bank.
MemGarden is not there: this arm's output is one of four fused inputs rather
than the entry-point selection, and 3k rows is four orders of magnitude short
of that bank. But the failure mode is real and the ceiling above is where to
start watching for it.

### Measured — AC-2 with the fourth arm active

Real `bge-small-en-v1.5`, 3000 nodes, 2000 requests. The CE-8 harness spreads
`mentioned_at` over the last 90 days and rotates **seven** queries, two of them
temporal (one Korean), so the fourth arm is live on ~29% of requests instead of
returning empty. `MEMGARDEN_BENCH_CONTROL=1` restores CE-7's exact harness —
five queries, none temporal, no spread — so one row below is comparable with
CE-7's recorded numbers on this build.

```
                      p50       p90       p95       p99       max     <35ms      <60ms
idle                 6906us    7612us    7985us    8982us   31647us   2000/2000  2000/2000
loaded #1           21276us   46912us   54863us   64563us   86290us   1534/2000  1941/2000
loaded #2           20759us   45754us   52546us   63396us   74939us   1591/2000  1957/2000
loaded, CONTROL     19628us   43458us   48964us   57744us   63789us   1598/2000  1993/2000
CE-7, for reference 19124us   43364us   48654us   57272us   65460us   1605/2000  1997/2000
```

**AC-2 (p50 ≤ 35 ms, p95 ≤ 60 ms) holds in every run.** p99 is a watch item,
not a gate — AC-2 defines p50 and p95 only.

**The size-controlled run settles what the trend line could not.** The
18.1 → 11.3 → 5.1 ms headroom series is *not* one trend line: the third point
changed the harness (7 rotating queries instead of 5, plus the `mentioned_at`
spread), so part of that drop is the query mix, not the code. Holding the
harness and the ingest volume fixed (35,712 background nodes written here vs
35,840 at CE-7, 0.4% apart), CE-8 costs **+0.5 ms p50 / +0.31 ms p95 loaded**
— flat, exactly as the +0.09 ms idle delta predicted. The remaining
48.96 → 52.5/54.9 ms gap belongs to the two temporal queries in the CE-8 mix,
which hydrate up to `over_fetch` extra ids each; the other 71% of requests are
unchanged.

**Watch metric: the fraction over 60 ms**, not p95 — it is far less jumpy than
a tail quantile and it tracks user-visible harm directly. On the CE-8 mix it
moved 0.15% → **2.95%** (1941/2000 worst run); on the controlled run it is
0.35%, i.e. CE-7's 0.15% within noise.

**The known lever was not taken, and the reason is in the data.** CE-7 recorded
merging the graph arm's two hops (expand + hydrate into the main hop) as the
reserve lever. Idle p95 moved +0.09 ms and controlled loaded p95 +0.31 ms, so
CE-8 added essentially no pipeline cost; what makes the loaded case slow is
write-lock contention, r2d2 pool pressure, and the vec0 partition growing by
36k nodes under the recall loop. Merging expand+hydrate removes one scheduler
round trip and does **nothing** about any of those three. Spending a recall-core
refactor (pass-1 filtering and fusion move inside a blocking closure) on a cause
it does not address is the wrong trade while the gate passes.

**Trigger to spend it**, on a size-controlled run (`MEMGARDEN_BENCH_CONTROL=1`,
3000 nodes, 2000 requests) so it cannot fire on a harness change: **loaded
p95 > 50 ms, or idle p95 > 15 ms.** Margin is the point — a 55 ms trigger sits
0.1 ms past the worst run already recorded and would leave no lead time for the
refactor it triggers. B7 should re-run the controlled bench before landing.

### Manual verification

Real daemon (release, `127.0.0.1:9199`, embeddings off so this is BM25 +
temporal + graph), two transcripts retained through the real Ollama with
`event_date` set to 8 days and 95 days ago, then recalled. Abridged output —
`temporal` and `keyword` are from the response's own `scores`:

```
=== what did we decide last week ===       candidates 5
  [2026-07-25] temporal=0.571 recency=0.977 final=1.1110 kw=-0.302 :: Decided to use sqlite-vec …
  [2026-04-29] temporal=0.000 recency=0.738 final=0.7307 kw=None   :: User asked how the re-ranker …
  [2026-07-25] temporal=0.571 recency=0.977 final=0.6110 kw=None   :: RRF constant k was fixed at 60 …
  [2026-04-29] temporal=0.000 recency=0.738 final=0.3064 kw=-0.290 :: Decided not to use cross-encoder …
  [2026-07-25] temporal=0.571 recency=0.977 final=0.1111 kw=None   :: User asked about which vector store …

=== 지난주에 뭘 결정했지 ===                candidates 4
  [2026-04-29] temporal=0.000 recency=0.738 final=0.9429 kw=None   :: User asked how the re-ranker …
  [2026-07-25] temporal=0.571 recency=0.977 final=0.7777 kw=None   :: RRF constant k was fixed at 60 …
  [2026-07-25] temporal=0.571 recency=0.977 final=0.4444 kw=None   :: Decided to use sqlite-vec …
  [2026-07-25] temporal=0.571 recency=0.977 final=0.1111 kw=None   :: User asked about which vector store …

=== what did we decide ===                 candidates 3   (no temporal expression)
  [2026-07-25] temporal=0.500 recency=0.977 final=1.0953 kw=-0.302 :: Decided to use sqlite-vec …
  [2026-07-25] temporal=0.500 recency=0.977 final=0.6024 kw=None   :: RRF constant k was fixed at 60 …
  [2026-04-29] temporal=0.500 recency=0.738 final=0.1048 kw=-0.290 :: Decided not to use cross-encoder …
```

Three things to read off it. The in-window facts carry a real `temporal`
(0.571 for a 2026-07-25 fact in a 07-20..07-26 window) and the out-of-window
ones 0.000. Every `kw=None` row is a node **no keyword arm reached** — on the
Korean query that is all four, i.e. the answer is entirely the temporal arm's
(plus the graph arm expanding off its seeds, which is how the 04-29 node gets
in). And dropping "last week" from the query drops the temporal-only rows and
returns every `temporal` to the 0.5 neutral.
