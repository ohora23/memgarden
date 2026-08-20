# CE-6b — the semantic boost, the first term that is not legacy's

`combined` gains a fourth multiplicative boost, weighted by where a
candidate's cosine sits inside the spread its own query produced. Default
`0.1`. `0.0` is legacy scoring to the bit.

Everything else in `recall/scoring.rs` is a port. This is not, and the note
says so at the top because the AC-1 gate measures against legacy: a divergence
has to earn its place with a number, and this one earns it on the labelled
benchmark and **not** on the gate it was built for.

## Why

Legacy fuses the retrieval arms by RRF, then scores on **rank alone**
(`reranking.py:134-145`). The cosine reaches the final order only as an
ordinal, so a keyword arm matching a command log literally carries exactly the
weight of a semantic match on the answer. Measured on a live query:

```
candidates n=309, returned=12
final span   0.0477
temporal     0.50 .. 0.50    constant, cannot reorder
recency      0.943 .. 0.946  a 365-day window against a two-week bank
proof        0.50 .. 0.50    constant
semantic     0.715 .. 0.866  the only signal with real spread — discarded
final order == RRF order? True
```

Three of the four terms are constants on an ordinary query, so the scoring
stage contributes no information at all and the one signal that discriminates
is the one thrown away.

## Normalised per query, and it has to be

Raw cosine over a real bank sits in a narrow high band — 0.63-0.94 across four
live queries. Feeding it in raw would produce another near-constant
multiplier, which is exactly how `recency` became inert. What carries
information is a candidate's *position inside this query's* range, so
`semantic_norm` is min-max over the candidates the semantic arm scored.

Two cases matter more than the formula:

* **a candidate with no cosine is `NEUTRAL`, not zero.** Keyword-only hits
  must not be pushed down for lacking a signal they were never eligible for.
* **a degenerate span is `NEUTRAL`.** A candidate set with no semantic arm
  leaves `lo > hi` from the fold's `(MAX, MIN)` start; that reads as a
  degenerate span and answers neutral rather than dividing by zero.

## Measured — AX-2, ledger line 14

| alpha | recall@5 | recall@10 | MRR | nDCG@10 |
|---|---|---|---|---|
| **0.0** (legacy) | 0.2090 | 0.3704 | 0.5162 | 0.3063 |
| 0.05 | 0.2561 | 0.3897 | 0.7282 | 0.3603 |
| 0.08 | 0.2434 | 0.3972 | 0.7222 | 0.3685 |
| **0.10 (shipped)** | **0.2379** | **0.4027** | **0.7095** | **0.3729** |
| 0.12 | 0.2458 | 0.3948 | 0.7095 | 0.3744 |
| 0.15 | 0.2444 | 0.3954 | 0.7000 | 0.3805 |
| 0.3 | 0.2278 | 0.3392 | 0.6714 | 0.3459 |
| 1.0 | 0.1998 | 0.3116 | 0.6650 | 0.3218 |

**MRR +0.193, recall@10 +0.032, nDCG@10 +0.067** at the shipped value — and
it also clears ledger line 12, the pre-dedupe baseline, on every aggregate.

**0.1 is the middle of a plateau, not an argmax.** Every value in 0.05..=0.15
beats legacy scoring on all four aggregates, and no single one of them wins
all four. A result that survives a 3x range of its own knob is not a fit to
the corpus, which is the charge that sank the recency-window candidate on
2026-08-12.

Per query: **8 improved, 4 regressed, 2 unchanged.** By stratum,
`memcompare` +0.063 and `identifier` +0.084 recall@10 — the two largest — and
`identifier` reaches **MRR 1.0**, every one of its queries answering at rank 1.

### The cost, which is one stratum

`temporal` loses **0.10 recall@10**, all of it q17 (−0.2). A semantic boost on
a date-constrained query promotes items the *semantic* arm liked over items
`temporal_candidates` retrieved precisely because they fall in the window.
This is the same shape review flagged separately: `temporal_proximity`
returns NEUTRAL for an undated node and less than that for a dated node in the
outer half of the window, so the boost is already anti-correlated with the arm
feeding it. Not fixed here, and it is the first thing to look at if this term
is revisited.

## What it does not do, which is what it was built for

It was built to fix the four losses in the AC-1 blind panel. **It fixes none
of them.** Re-judged blind at 0.1 against the same frozen legacy answers:

| | better | worse | equivalent |
|---|---|---|---|
| alpha 0.0 | 12 | 4 | 4 |
| alpha 0.1 | 13 | 5 | 1 |

Three equivalents became wins and one became a loss; ab5, en9, en14 and en15
all still lose. Counted directly, **one** of the 27 relevant items MemGarden
had retrieved-but-not-injected on those queries entered the window. Raising
alpha recovers more of them — 9 of 27 at `1.0` — while collapsing the gold
numbers, so no value satisfies both instruments.

**The starting evidence was misread, and that is worth recording.** The case
for building this was that 12 of 12 missed items on `@agentmemory/mcp`, and 3
of 3 on `hindsight`, held a higher cosine than the injected items. They held a
higher cosine than the *worst* injected item — a much lower bar than the cut,
and not the one that decides whether an item is promoted.

Three ranking-weight changes have now been measured against these losses —
a recency window, an action-record penalty (both 2026-08-12), and this — and
none moved them. That pattern is itself a result: **the AC-1 losses are not a
scoring-weight problem.** ab5 is topic drift, en14 is a query nothing in the
bank answers, and en9 and en15 want items the retrieval arms rank far below
the cut for reasons a multiplier on the top of the pile does not reach.

## Why it ships anyway

The gate holds either way (`worse <= better` at both 4-to-12 and 5-to-13), so
shipping risks nothing there, and AX-2 is the only instrument in this project
with ratified human labels rather than a model's judgement. A +0.19 MRR on it
is the largest single improvement the harness has recorded.

Latency, same 20 live queries: p50 **12.1 -> 12.6 ms**, p95 **22.3 -> 24.9
ms**, against AC-2's 35/60 ms and legacy's 87 ms p50. The added work is one
min/max pass over the fused candidates.

## Diverged from legacy

- **The whole term.** Legacy has no semantic boost; its scoring is rank plus
  three circumstantial multipliers. `alpha = 0.0` restores exact parity and is
  what every ledger row before line 14 was measured at.
- **Consolidation and reflection keep `0.0`.** Those paths feed dedup and
  mental models, not the injection. Re-ranking them is a separate question
  from the one this was measured against, and CE-9a's dedup in particular was
  tuned against the order it currently sees.
