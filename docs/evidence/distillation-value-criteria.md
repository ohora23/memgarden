# Is a distilled observation worth more than the facts it came from? — rules, fixed before the run

Committed before any arm was run. The commit timestamp is what makes that
checkable.

## Why this measurement exists

This project was built for three things: **save the user tokens, hold context
over time, and periodically distil memory into something better.** The first is
measured and came out negative — injection costs ~1,325 tokens a turn and
[MX-3](mx-3-result.md) found the memory arm spending 5% more. The third has
never been measured at all, and the
[corpus census](bank-uniqueness-result.md) was quoted as if it had settled it,
which it could not: a test for whether a node's terms appear on disk cannot
judge a synthesis whose constituents are all on disk.

Consolidation (CE-9) is live and daily — **2,770 observations, 2,757 carrying
source links**. This asks what they are worth.

## The question, stated so it can fail

**Does allowing `observation` nodes into recall retrieve better than restricting
recall to the `world` facts they were synthesised from?**

If observations are a real distillation, a query answered by an observation
should be answered *better* than by its constituent facts — higher grades
earlier in the ranking. If they are lossy paraphrase, the same query does worse
and the layer is costing storage and extraction time for nothing.

## The arms

One variable: `recall_types`, which is already a field on the recall request
(`routes/recall.rs:49`). Nothing else changes — same corpus, same queries, same
scoring, same daemon build.

| arm | `recall_types` |
|---|---|
| **facts-only** | `["world"]` |
| **shipped** | `["world","observation","experience"]` — today's default |
| **observations-only** | `["observation"]` |

The third arm exists because the first two cannot separate *"observations help"*
from *"more candidates help"*. If observations-only beats facts-only per
retrieved item, the distillation is carrying information; if it only wins when
added on top, the gain may be recall breadth rather than synthesis.

## Corpus, queries, scoring

The frozen AX-2 gold set, unchanged: **2,718 facts** (world 1,738 ·
observation 956 · experience 24) and **20 graded queries** across five strata,
331 labels, macro-averaged recall@k / MRR / nDCG@10 (Burges/TREC), exactly as
`recall_bench` already computes them.

## The limit that decides how this can be read — stated first

**The labels are 71% `world`.**

| stratum | world | observation | experience |
|---|---|---|---|
| conclusion | 20 | 3 | 0 |
| graph | 39 | 11 | 0 |
| identifier | 55 | 31 | 0 |
| memcompare | 84 | 29 | 1 |
| temporal | 38 | 20 | 0 |
| **total** | **236 (71%)** | **94 (28%)** | **1** |

A gold set whose answers are mostly raw facts **cannot show an observation
beating them**, because an observation is not in the answer key for those
queries. So:

* **facts-only winning is not evidence against distillation.** It is the
  expected result of a fact-labelled key, and will be reported as such.
* **observations winning anyway is strong**, because it happens against the
  labelling's grain.
* the **observation-labelled subset** (94 labels) is reported separately, and it
  is the only part of this that speaks to the question directly. n is small and
  the number carries that.

This limit is written here, before the run, so that a facts-only win cannot
later be presented as "distillation measured and found wanting". **This
measurement can confirm value; it is much weaker at refuting it.**

## What would decide it

* **Observations lift the shipped arm above facts-only, and observations-only
  is competitive per item** → the distillation carries information. The CE-10
  tier above it (0 rows, never run) is then worth wiring, with a consumer.
* **The shipped arm gains only in proportion to added candidates** → what is
  being bought is breadth, not synthesis, and the cheaper move is more
  candidates rather than more layers.
* **Observations lower the shipped arm** → consolidation is producing noise that
  displaces better answers, which is a defect and not a null result.

There is no outcome these rules prefer.

## Limits, stated in advance

1. **A retrieval benchmark is not a usefulness benchmark.** It measures whether
   the right node is returned, not whether a reader learns more from it.
2. **This gold corpus is legacy-imported.** Its observations were made by the
   old system's consolidation, not by CE-9 on this bank. Applying the result to
   MemGarden's own 2,770 observations is an inference, and it is named as one.
3. **20 queries, 331 labels, 94 of them observations.** Per-stratum it is
   smaller still. Distributions get reported, not just means.
4. **The author built the system.** The rules are here, committed first; the
   arms differ by one field; the harness is the one already in the repository.
