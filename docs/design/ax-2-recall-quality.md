# AX-2 — recall-quality harness (recall@k / MRR / nDCG + gold labels)

Branch `feat/ax-2-recall-quality-harness`. No migration, no schema change, no new
dependency, no new REST endpoint.

Phase B made at least ten decisions that move recall quality — RRF `k=60`, the
100/arm over-fetch against legacy's 1500, not porting `min_semantic` /
`min_keyword`, the `recallTypes` default, three alphas, the dual budget, the
graph-arm additive formula, temporal proximity. **Every one was justified by
port fidelity or latency. Not one was justified by measured recall quality.**
Until now the repo had three latency benches (`hybrid_recall_bench`,
`graph_arm_bench`, `temporal_arm_bench`) and zero quality metrics, so AC-1's
"user judgment" gate had nothing behind it but taste.

This item builds the instrument. It does not tune anything — CE-11 is the first
consumer, and its completion condition becomes "here is the recall@k / MRR /
nDCG delta the reranker buys and the latency it costs" instead of a hunch.

Origin: plan `.omc/plans/phase-b-impl.md`, "AX-2 상세 — 회수 품질 하네스".

## What this adds

| Path | What |
|---|---|
| `gold/export_legacy_corpus.py` | Read-only snapshot of the legacy bank (two GETs, no mutation). |
| `gold/corpus.jsonl` | The snapshot itself — 2718 facts. **Not committed**, gitignored: see `gold/README.md`. |
| `gold/corpus.sha256` | `sha256sum -c`-compatible checksum. Not committed, for the same reason. |
| `gold/queries.jsonl` | 20 queries, 331 graded judgments, one rationale per label. **Not committed** — the rationales quote the facts they grade. `gold/queries.example.jsonl` carries the schema. |
| `gold/results.jsonl` | Append-only results ledger — metrics, query ids and result uuids, no text, so it is committed. **Line 8 is the current baseline** (q17's labels ratified by `fix/q17-labels`); lines 1 and 5 were, and are kept. |
| `gold/results.pool.json` | The top-20 the last run produced, with text — the labelling pool and its audit trail. Rewritten each run, unlike the ledger. Not committed. |
| `crates/memgardend/src/bin/recall_bench.rs` | `import` + `bench`. |

Everything below describes the corpus this project measured against. It is the
reasoning that still applies to a corpus you export yourself; the numbers are
ours and are not reproducible from this repository alone.

## The corpus — the most important decision here

Gold labels are worth nothing against a corpus that cannot be rebuilt. Three
choices make this one reproducible.

**1. Text import, not a `/retain` round trip.** The plan says "export fact text
from legacy and re-retain". Taken literally that means running qwen3-14b
extraction over text that is *already* extracted facts: hours of GPU time, and —
fatally — the extractor **rewrites the text**, which invalidates every label the
moment it runs. `recall_bench import` writes the fact text straight in as memory
nodes and lets our own fastembed produce the vectors. That also makes AX-1's
"two embedding spaces" problem vanish: every vector in this corpus is ours.

**2. The legacy uuid is preserved as the node uuid.** `nodes::insert_batch`
mints a fresh v7 per row, so the import overwrites `uuid` (and `proof_count`) in
one follow-up `UPDATE`. Without this, labels would only match the one database
that happened to be built first.

**3. The snapshot is committed, not just checksummed.** 3.6 MB of JSONL, which
git's zlib packs down hard because the tag lists repeat. The alternative —
checksum plus exporter — fails the moment the legacy daemon is retired, which is
the entire point of this rebuild. The exporter is committed too, for provenance.

> **Reversed, and at a real cost.** Preparing this repository to be readable by
> others took the corpus and the labels back out (`gold/README.md`): they are one
> person's memories, and the label rationales quote the facts they grade. The
> objection above still stands and was not answered — once the legacy daemon is
> retired, `results.jsonl`'s numbers can no longer be recomputed by anyone,
> including us. What is kept is the ledger, which is enough to say what changed
> between two of our own runs and not enough to re-derive either. The exporter
> and the label schema are committed so a reader can build the same *kind* of
> instrument over their own bank; they cannot rebuild ours.

### Snapshot identity

| | |
|---|---|
| Source bank | `claude-code::bank-b` on the legacy daemon (`127.0.0.1:9077`, Hindsight 0.8.6) |
| Exported | 2026-08-03, `state=valid` only |
| Facts | 2718 (1738 world / 956 observation / 24 experience) |
| Fact dates | 2 at 2024-06-08, then 2026-07-30 (19), 07-31 (760), 08-01 (760), 08-02 (1177) |
| `sha256` | `baee3f40a4f9556601f9ee77816e4a57964f83b18e73f82a68910bcea4bda868` |

One bank, not three. Recall is per-bank in both systems and the Phase C hook
only ever queries the project's own bank, so a multi-bank corpus would measure a
configuration nothing runs in.

`memories/list` is fetched in **one** request, not paged: it has no `ORDER BY`
guarantee, and a paged read of a bank being written to concurrently both
duplicates and drops rows — the first paged run of the exporter returned 2717
unique ids against a reported total of 2718.

### What the import rebuilds, and how

Derived structures are built by the *production* code paths, never
reimplemented:

* **Embeddings + semantic links** — `embed_task::drain_once`, the real backlog
  worker, including its `on_batch_embedded` KNN pass. 26.2 s for 2718 nodes.
* **Temporal links** — `links::temporal_links` over the whole corpus as both
  sides, which converges to the same edge set (each node's 20 best
  24-hour neighbours) without replaying ingest order. 54 012 links.
* **Entities** — `graph::write_entities` from legacy's own resolved `entities`
  string. 2129 entity rows from 1471 facts.

**Known gap:** no causal links. Legacy's export carries no `causal_relations`,
so the graph arm's causal bucket is empty in this corpus and the graph stratum
exercises entity co-membership plus semantic/temporal edges only. Recorded
rather than invented — synthesising causal edges would be fabricating the very
signal the stratum is meant to measure.

## The harness

`recall_bench` is a binary, not an `#[ignore]`d test. The latency benches are
tests because they seed their own synthetic bank and print one number; this one
takes a corpus path, a label path and an output path, and its results are a
committed artifact.

```
recall_bench import <corpus.jsonl> <db-path>
recall_bench bench  <db-path> <gold.jsonl> <corpus.jsonl> [results.jsonl]
```

`bench` refuses to run if the database's node count differs from the corpus's
line count — the "benched the wrong database" mistake otherwise looks exactly
like a quality regression.

### Measurement configuration, and why each value

| Knob | Value | Why |
|---|---|---|
| `now_ms` | `1_785_715_200_000` (2026-08-03T00:00:00Z) | **Pinned.** `scoring::recency` and the temporal arm both read it; a wall-clock `now` would drift the baseline daily and eventually push "지난주"/"어제" off the end of the corpus. Midnight UTC after the newest fact. |
| `limit` | 20 | `[recall] limit`'s production default. `over_fetch` clamps at a 100 minimum, so 10 and 20 give the *same* per-arm over-fetch and the same ranking — the pool gets ten extra candidates for free. |
| `K` (measured) | 10 | recall@1/5/10, MRR and nDCG@10 all read the first 10 only. |
| `max_tokens` | 8192 (`MAX_RECALL_TOKENS`) | **Not** the production 1024. `fit_to_budget` truncates the result list, so the default would report nDCG@10 over a list the budget had already cut to six — measuring the budget, not the ranker. The budget is a real lever; it is a different lever. |
| `budget` | `mid` | `[profile] recall_budget`'s default; `rerank_limit` 600, far above K. |
| `recallTypes` | all three | The `[recall] types` default. |
| tags / `cap_per_source` | none / 0 | Defaults; neither affects ranking here. |

### nDCG convention

```
DCG@k = Σ_{i=1..k} (2^grade_i − 1) / log2(i + 1)
```

Exponential gain, binary-log discount, rank 1 undiscounted: grade 2 → gain 3,
grade 1 → gain 1. This is the **Burges/TREC** formulation, *not*
Järvelin-Kekäläinen's original linear gain and *not* the "no discount on the
first two ranks" variant. Three conventions are in common use and they do not
produce the same number, so a future reader comparing against a published figure
has to know which one these are. The ideal DCG sorts all labelled grades
descending and truncates at `k`, so a query with more than 10 relevant nodes can
still reach 1.0.

Aggregates are **macro**-averaged — every query counts once. A micro-average
would let the two or three broadest queries dominate and hide exactly the
per-stratum weakness the stratification exists to expose.

## Gold labels

20 queries in five strata: the 5 unique memcompare queries, then the plan's four
new strata (identifier, conclusion, temporal, graph).

* Grades: **2** = core (answers the query), **1** = related (useful context),
  **0** = judged and rejected. A 0 is kept, not dropped: it records that a
  plausible-looking hit was *examined*.
* **Every label carries a one-line rationale.** `read_gold` rejects the file if
  any rationale is empty — the requirement is enforced, not just documented.
* **Ratification is tracked per query, not per run.** `labels_status` lives on
  each `gold/queries.jsonl` record: `provisional-pending-user-review` until the
  corpus owner signs *that query's* labels off, then `ratified-YYYY-MM-DD`. The
  field was always per-query; **q17 (`ratified-2026-08-03`) is the first query
  to differ from the rest**, which is what made a single global flag stop
  telling the truth.

  Each results record carries the status twice, at the two resolutions a reader
  needs: on every `per_query` entry, and at the top level as the sorted **set**
  of the values present in the run. A set rather than a single flag is the whole
  point — a mixed run reports
  `["provisional-pending-user-review", "ratified-2026-08-03"]` and cannot be
  read as fully ratified, which one collapsed "ratified" flag would have
  allowed. `recall_bench` also prints the line to stdout, so the caveat is on
  the terminal the numbers were read off and not only in a file.

**The pool is not only the top-K.** Labelling from the current ranking alone
bounds recall@10 at 1.0 by construction and hides every miss. Candidates came
from the top-20 **plus targeted keyword scans of the corpus**. q05 is why this
mattered: not one of its six grade-2 nodes appears in the current top-20, so a
pool-only label set would have scored it near-perfect instead of 0.182.

### Six queries have no answer in this corpus

Reported per-query rather than papered over with invented labels, and excluded
from the aggregates.

| Query | Why empty |
|---|---|
| q10 `pkill -f` | Zero literal hits across 2718 facts. Nothing discusses process-kill side effects. |
| q12 consolidation ctx overflow | The 2026-08-02 incident lives in the curated `MEMORY.md` and `claude-code.env`, never in a retained fact. |
| q13 two-hop merge lever | Zero hits for `두 홉` / `2홉` / `two-hop` / `병합 레버`. Discussed in the plan document, which is not retained material. |
| q14 embedding-model tagging | AX-1 was decided *after* the snapshot's newest fact. A cut-off artifact, not a memory-system gap. |
| q16 yesterday's CI fix | 2026-08-02 holds 1177 facts but none describe a CI failure being fixed; the day's CI facts are policy statements and commit listings. |
| q20 watermark + data loss | Both halves exist separately (10 watermark facts, 7 data-loss facts); nothing links them. |

Four of the six are corpus cut-off (q12, q14) or genuinely-never-discussed
(q10, q13) — which is itself a useful signal about what the retain pipeline
does and does not capture.

## Baseline

### Which line of the ledger is current

`gold/results.jsonl` is append-only and now has three generations of the off
arm. This table is the index; the superseded figures are kept below because
`ce-11-reranker.md` and `ce-8-korean-absolute-dates.md` quote them.

| Ledger line | Commit | What | Status |
|---|---|---|---|
| 1-2 | `f1b7d143` / `52a8288` | The original AX-2 baseline, pre-Korean-date fix. | Superseded — kept, CE-11's first tables used it |
| 5 | `33d49519` | Re-baselined by `fix/ce-8-korean-absolute-dates`: `8월 2일` parses, q17's retrieval changes. | Superseded — kept, quoted in two notes |
| **8** | **`73ba3b2c`** | **q17's labels ratified by `fix/q17-labels`; `\|R\|` 4 → 5.** | **Current baseline** |

Lines 3-4, 6-7 and 9-10 are the matching `top_k = 10` / `top_k = 20` reranked
arms for each generation (CE-11).

> **Only q17 has ever moved.** Across both re-baselines, every other query
> reproduces digit-for-digit including its retrieved uuid list. The 2026-08-03
> ratification changed **no retrieval at all** — `gold/results.pool.json` is
> byte-identical across it, since only the label set moved.

**Note on the recorded commit.** `gold/results.jsonl` line 1 stamps
`commit: f1b7d143`, but its numbers already include the two gold-label
corrections that landed in `2247246` — the harness reads `git rev-parse HEAD`
at run time and the run predated the label fix's commit. The *numbers* are
correct and reproduce exactly (CE-11 re-ran them digit-for-digit at
`52a8288`, appended as line 2); only the stamp is stale. Records written from
CE-11 onward are stamped correctly.

**Commit `73ba3b2c`** (`gold/results.jsonl` line 8), corpus
`baee3f40…4bda868` (2718 nodes), `now = 1785715200000`,
`|R|` = labelled relevant nodes, `ceil` = `min(10,|R|)/|R|`.
The record carries the full per-query breakdown and the retrieved uuid lists.

The pre-fix baseline was reproduced three times digit-for-digit: twice from
separate `import` runs into fresh databases, and once more after rebasing onto
CE-10 (schema v6). CE-11 reproduced it a fourth time at `52a8288` (line 2).
Both re-baseline rounds imported into a fresh database again and reproduced
this note's own structure counts exactly — 2718 nodes, 2129 entity rows from
1471 facts, 54 012 temporal links.

```
query  stratum        |R|     r@1     r@5     r@10     ceil     mrr  nDCG@10
q01    memcompare      18   0.000   0.222    0.333    0.556   0.500    0.207
q02    memcompare      10   0.000   0.200    0.700    1.000   0.333    0.500
q03    memcompare       8   0.000   0.125    0.250    1.000   0.333    0.098
q04    memcompare      11   0.000   0.091    0.273    0.909   0.250    0.116
q05    memcompare      11   0.000   0.182    0.182    0.909   0.500    0.095
q06    identifier      10   0.100   0.400    0.400    1.000   1.000    0.328
q07    identifier      10   0.000   0.100    0.400    1.000   0.250    0.308
q08    identifier      13   0.077   0.308    0.462    0.769   1.000    0.665
q09    identifier      17   0.000   0.176    0.412    0.588   0.500    0.365
q11    conclusion       5   0.000   0.000    0.200    1.000   0.143    0.113
q15    temporal        16   0.000   0.000    0.000    0.625   0.000    0.000
q17    temporal         5   0.200   0.600    1.000    1.000   1.000    0.638
q18    graph            9   0.111   0.556    0.556    1.000   1.000    0.612
q19    graph           15   0.000   0.200    0.267    0.667   0.500    0.486

(5)    memcompare       -   0.000   0.164    0.348    0.875   0.383    0.203
(4)    identifier       -   0.044   0.246    0.418    0.839   0.688    0.416
(1)    conclusion       -   0.000   0.000    0.200    1.000   0.143    0.113
(2)    temporal         -   0.100   0.300    0.500    0.812   0.500    0.319
(2)    graph            -   0.056   0.378    0.411    0.833   0.750    0.549
(14)   ALL              -   0.035   0.226    0.388    0.859   0.522    0.324
```

### Superseded: the two earlier generations

Kept because `ce-11-reranker.md` and `ce-8-korean-absolute-dates.md` quote them.
In **both** transitions the **only** rows that differ are q17 and the two
aggregates it feeds.

**Pre-fix (lines 1-2, commit `d6165560`)** — `8월 2일` extracted no constraint:

```
q17    temporal         4   0.000   0.000    0.250    1.000   0.100    0.149
(2)    temporal         -   0.000   0.000    0.125    0.812   0.050    0.074
(14)   ALL              -   0.021   0.183    0.335    0.859   0.458    0.289
```

**Post-fix, pre-ratification (line 5, commit `33d49519`)** — the Korean date
parses, but 15 of q17's 20 pooled documents were still ungraded and `|R|` was
stale at 4:

```
q17    temporal         4   0.250   0.750    1.000    1.000   1.000    0.628
(2)    temporal         -   0.125   0.375    0.500    0.812   0.500    0.314
(14)   ALL              -   0.038   0.236    0.388    0.859   0.522    0.323
```

On the 13-query basis CE-11 uses (conclusion excluded), across both
transitions: recall@1 0.0222 → 0.0414 → **0.0375**, recall@5
0.1969 → 0.2546 → **0.2431**, recall@10 0.3449 → 0.4026 → **0.4026**, MRR
0.4821 → 0.5513 → **0.5513**, nDCG@10 0.3021 → 0.3390 → **0.3398**.

**The ratification moved q17's nDCG@10 up** (0.6285 → 0.6378) while moving
recall@1 and recall@5 *down* (0.250 → 0.200, 0.750 → 0.600). Nothing was
retrieved differently; `|R|` went 4 → 5 when the rank-9 document was graded 1,
which raises the shallow recalls' `1/|R|` denominator while raising both the
actual and the ideal DCG. recall@10 stayed at 1.000, and — with ranks 11-20 all
graded 0 — that 1.000 is now robust rather than an artifact of a stale `|R|`;
`ce-8-korean-absolute-dates.md` carries the corrected mechanism and strikes the
wrong one.

### Reading these numbers

**recall@k is deflated by duplication, nDCG is not.** This corpus is thick with
near-duplicates — the same statement retained as both a `world` and an
`observation` node, sometimes four times over successive chunks. That is real
data and the labels count every copy as relevant, so several queries have 15-18
relevant nodes and a recall@10 ceiling around 0.56. **recall@1 is bounded by
`1/|R|`** and should be read as a floor, never as precision@1. Where a single
number is wanted, use **nDCG@10** — its ideal list is truncated at 10 too, so it
is the metric duplication does not deflate.

**The proper-noun stratum is the strongest, which is the jcode cross-check.**
The plan adopted the floorless hybrid on jcode's argument that a hard cosine
floor zeroes recall on identifier-heavy agent memory, and that argument is only
visible on this axis:

| stratum | MRR | nDCG@10 |
|---|---|---|
| identifier (proper noun) | **0.688** | **0.416** |
| graph | 0.750 | 0.549 |
| temporal | 0.500 | 0.319 |
| memcompare | 0.383 | 0.203 |
| conclusion | 0.143 | 0.113 |

Identifier queries land a relevant node at rank 1-2 on average (two of the four
have MRR 1.0), against 0.143 for conclusion queries. The lexical arm is doing
exactly the work jcode said it would. **This is evidence the floor should stay
un-ported**, and CE-11 must not regress this column while chasing precision.

**Weak spots the aggregate would have hidden:**

* **q15 scores 0.000 on everything** despite 16 labelled relevant nodes. The
  worst single result in the set. Still true after the re-baseline, and still
  not a bug — see finding 2 below.
* ~~**Temporal is the weakest stratum by a wide margin**~~ — it was, at
  0.050 MRR / 0.074 nDCG@10, and both of its scored queries had a temporal-arm
  problem rather than a ranking problem. Finding 1 below has since been fixed,
  which took the stratum to 0.500 / 0.319 on q17 alone. **Conclusion is now
  the weakest**, for the structural reason two sections down.
* **q05 has MRR 0.500 but recall@10 0.182**: it surfaces one related node early
  and then misses all six core nodes entirely.
* **conclusion has one scored query out of four.** Its 0.113 is a sample of one
  and should not be treated as a stratum measurement yet. See below — the
  reason is structural, not a labelling shortfall.

### The conclusion stratum cannot be measured against this corpus, by construction

All four conclusion queries fail to produce a usable measurement, and they fail
the same way: q12, q13 and q14 have **no answer at all** in the corpus, and q11
has 23 labels of which **zero are grade 2**. That is the entire stratum.

This is not a gap in the labelling. It follows from what the corpus *is*. The
corpus is legacy Hindsight's **auto-captured** facts, and under the memory role
split adopted 2026-08-01 the curated conclusions — decisions, non-obvious
solutions, project state — are deliberately written to **native `MEMORY.md`
instead**, a store this export does not touch. Conclusion-type questions are
answerable from the system as a whole and unanswerable from this half of it.

The same effect was already observed independently: the memcompare A/B found
that curated saves win on conclusion-type questions while Hindsight wins on
breadth of specifics. AX-2 reproduces that finding from the other direction and
puts a number on the half it can see.

Consequences, both binding:

1. **CE-11 must not report a conclusion delta.** Moving 0.113 on a single query
   with no core answer in reach is noise with a decimal point on it.
2. **AC-1's transition gate cannot be evaluated on conclusion questions using
   this corpus** — and conclusion questions are arguably the highest-value class
   a memory system has. Closing this needs a second corpus covering the curated
   store, or an explicit decision that AC-1 scopes to auto-captured recall only.
   Recorded here rather than silently folded into the aggregate.

The four-stratum design still did its job: the stratification is what made this
visible at all. An aggregate-only harness would have reported **0.324** overall
and hidden the fact that one of its four axes is unmeasurable. (It read 0.287
against the pre-re-baseline 14-query figure, and 0.323 before q17's labels were
ratified.)

### Two temporal findings worth acting on — one fixed, one not a bug

1. **`8월 2일` extracts no constraint at all** (q17). **FIXED**, and this
   note's baseline is the post-fix one. The period table had no absolute
   Korean dates and `fallback_date` required ISO (`2026-08-02`), so the
   temporal arm never fired for the most natural way to write an absolute date
   in Korean. It turned out to be a parity gap rather than a coverage gap, and
   then — once legacy's dateparser was re-checked under an explicit
   `RELATIVE_BASE` — not even that: legacy resolves the *month* only and takes
   the day and year from today. `fix/ce-8-korean-absolute-dates` implements it
   day-precisely as a deliberate divergence. q17: nDCG@10 0.149 → **0.638**,
   MRR 0.100 → **1.000**, recall@10 0.250 → **1.000** — and with q17's pool now
   fully graded, that 1.000 is robust rather than an artifact of a stale `|R|`.
   See `docs/design/ce-8-korean-absolute-dates.md`.
2. **`지난주` covers almost the whole corpus** (q15). With `now` pinned to
   Monday 2026-08-03, `Period::Week(-1)` resolves to 2026-07-27..08-02, which
   contains 2697 of 2718 facts. The window is nearly a no-op, so the temporal
   arm contributes a near-uniform fourth ranked list and q15 is effectively a
   lexical/semantic test that fails.

   **This is not a bug and must not be "fixed".** It is a property of a corpus
   whose facts all fall within four days, not a defect in the window logic —
   the window is correct, there is simply nothing outside it to exclude.
   Changing `Period::Week` to make this number move would be breaking working
   code to flatter a metric. The fix is a corpus spanning more calendar time
   (see Follow-ups); this one cannot exercise the arm.

   Net effect on the stratum: it is now **half** valid. q17 exercises the
   temporal arm end to end; q15 still does not.

## Diverged from legacy

Nothing. This adds no runtime behaviour — the daemon, the schema and every
request path are byte-identical to `master`. `recall_bench` is a second binary
in `memgardend` and links the library; nothing links `recall_bench`.

## Follow-ups

* **The labels need the corpus owner's judgment — 1 of 20 queries done.**
  q17 is `ratified-2026-08-03`; the other 19 are still
  `provisional-pending-user-review`. The weakest calls remaining: q16 (see
  below), q11's grade-1-only set, and the choice to count near-duplicate facts
  individually rather than collapsing them into one relevant "answer".
* **q16 is the shakiest empty verdict.** If the `git add -A` image-in-commit
  incident counts as "a CI problem", q16 becomes answerable and should be
  relabelled with `18a5cb9a` / `3294c14d` as grade 2.
* **q11 has no grade-2 label at all.** The three-way memory role split was
  recorded in the user's CLAUDE.md, not in a retained fact, so the corpus holds
  only the split's motivation and its measured benefit.
* **The corpus spans four days.** Enough for `어제`, not enough for `지난달`
  or a real recency curve. Re-snapshot when the bank has a few months of
  history, and re-baseline — the old numbers stay valid against the old
  checksum.
* **CE-11 reports a delta against the table above**, per query and per stratum,
  not just the aggregate, and appends its run to `gold/results.jsonl`.
* **MG-1 (Phase D) owns the real import path.** The importer here is a harness
  fixture and deliberately has no REST endpoint.
