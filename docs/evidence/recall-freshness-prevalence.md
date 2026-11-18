# Recall-side freshness: measured, and held

2026-11-18. Proposal: when two results on the same subject carry different
dates, rank the newer above the older (ordering only; nothing deleted). Before
building it, measure how often the situation occurs in what the hook actually
injected.

## Method

All 881 injections from 2026-10-17 to 2026-11-17 in the local transcripts
(`scripts/usage-audit.py`'s parser). Two memories in one injection are "same
subject" when the Jaccard overlap of their content tokens is ≥ 0.4; each
memory's date is the one its injection line carries.

## Result

| | |
|---|---|
| injections with a same-subject pair of different dates | 45 (5.1%) |
| ...where the older one was ranked above the newer | 30 (3.4%) |

Reading the pairs, they are not a stale fact and its correction. They are the
**same fact paraphrased twice**, typically a monitoring rule re-extracted every
time a recurring automated prompt carried it. Reordering would change which
paraphrase comes first; both would still be injected and neither is wrong.

Near-duplicates inside one injection, all dates:

| overlap threshold | redundant memories | injections containing one |
|---|---|---|
| ≥ 0.4 | 151 / 3,534 (4.3%) | 78 / 881 (8.9%) |
| ≥ 0.5 | 90 / 3,534 (2.5%) | 50 / 881 (5.7%) |
| ≥ 0.6 | 63 / 3,534 (1.8%) | 46 / 881 (5.2%) |

## On the gold set

The same rule — a newer memory moves ahead of an older one when their content
tokens overlap ≥ 0.4 — applied offline to the RRF top 20 of the fully
labelled gold set (14 answerable queries; the metric code reproduces the
harness's 0.384 / 0.696 / 0.431 exactly before the change):

| | |
|---|---|
| same-subject pairs of different dates in a top 20 | 2, across all 14 queries |
| ...older ranked first | 1 |
| recall@10 / MRR / nDCG@10 after reordering | 0.384 / 0.696 / 0.431 — unchanged |

The gold corpus spans one week, so it holds few dated pairs by construction;
it cannot show a gain, and it shows no loss.

## Decision

**Held.** The case it targets — an older claim outranking a newer contrary one
— did not show up as a measurable share of injections; what did show up is
paraphrase duplication worth 2–4% of injected memories. Collapsing those at
injection would be the cheaper lever if it ever matters, and the dominant
source (facts re-extracted from recurring automated prompts) shrinks on its
own once those prompts stop receiving injections.

Re-open when the usage audit's judge starts reporting misleading memories in
more than isolated cases, or when a reported mislead traces to an older fact
outranking a newer one.
