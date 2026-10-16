# Multilingual embeddings — why the semantic arm was replaced

2026-10-14. Measured while calibrating an abstain gate (a score floor below
which recall injects nothing). The gate could not be calibrated, because the
score it would gate on carried no signal. This records why, and what replaced
it. Prompts and memory texts stay on the measuring machine; only counts are
here.

## The calibration set

- **80 real prompts**, sampled (seed 7) from 280 typed prompts across six
  project banks over 2026-09-27 … 10-14. 72 of the 80 are mostly Korean.
- Each sent to the live `/recall` with the hook's own defaults: **1,208
  recalled items**, 15.1 per prompt.
- Each item labelled by a local 14B model (thinking on): *would knowing this
  plausibly help answer this prompt — same specific topic, task or decision,
  not just the same project?* 309 items (25.6%) came back useful; 14 of the 80
  prompts had none.
- A hand check of 36 labels: the "not useful" side held up; the "useful" side
  is generous (roughly half of those would survive a strict reader). The
  comparisons below are between models on the same labels, so a generous
  labeller shifts every row the same way.

## What the old model's cosine knew

`bge-small-en-v1.5` is English-only.

| prompts | items | cosine floor | precision kept | precision dropped |
|---|---|---|---|---|
| Korean | 1,066 | none | 23.7% | — |
| Korean | | ≥ 0.75 | 23.9% | 21.8% |
| Korean | | ≥ 0.80 | 20.0% | 23.7% |
| Korean | | ≥ 0.85 | 18.5% | 23.2% |
| English | 142 | ≥ 0.75 | 47.5% | 35.1% |

On Korean prompts **no floor helps, and a high one hurts**. The keyword arm
was the only signal: items it hit were useful 38–43% of the time, items only
the semantic arm found 15%.

Dropping every semantic-only item (a keyword-evidence gate) would have cut
injection 59% and raised precision to 39% — but lost 37% of the useful items,
and 19 of the 66 prompts that had any useful memory would have lost all of
them. It abstained on only 3 of the 14 prompts that had nothing to recall. Not
a gate; a different way of being wrong.

## Three models, same labels

Re-embedded the same 1,208 (prompt, item) pairs offline. AUC is the
probability that a useful item outscores a useless one.

| model | dims | AUC, all | AUC, Korean | mean AUC within a prompt |
|---|---|---|---|---|
| `bge-small-en-v1.5` (was) | 384 | **0.507** | 0.508 | 0.516 |
| `paraphrase-multilingual-MiniLM-L12-v2` | 384 | 0.624 | 0.618 | 0.643 |
| **`multilingual-e5-small`** | 384 | **0.715** | 0.713 | 0.740 |
| keyword hit, as a yes/no score | — | 0.649 | | |

The old semantic arm was a coin flip. E5 beats the keyword arm on its own and
keeps the 384 dimensions, so `vec_nodes` keeps its schema. Embedding the whole
3,971-node `memgarden` bank took 35 s against bge's 44 s on the same CPU.

Selection bias, stated: every item was retrieved by the old pipeline, so this
is "which model ranks what bge+FTS found", not "which model finds more".

## Thresholds that were bge numbers

Both were matched on the live `memgarden` bank (3,971 nodes), embedded under
both models from the same text — e5 with the `passage: ` prefix on both sides,
as production stores it, and without `augment_for_embedding`'s date suffix on
either. The second is the one gap: after the switch, compare the bank's edge
count with the 55,513 below.

| | bge | e5 | how matched |
|---|---|---|---|
| semantic link (`links::SEMANTIC_LINK_THRESHOLD`) | 0.70 → 54,196 edges | **0.87** → 55,513 | same edge count over top-20 same-type neighbours |
| consolidation dedup (`consolidation.dedup_threshold`) | 0.97 → 850 pairs | **0.98** → 1,066 pairs, 779 of bge's 850 | most of bge's pairs; it only gates an LLM call |

The two models agree on only ~22% of semantic edges (Jaccard 0.20–0.22 at
every e5 threshold) — expected, since one of them could not read most of the
text. Edge weights are rescaled from `[0.87, 1]` onto `[0.7, 1]` so the graph
arm, which takes each neighbour's max over semantic, temporal (0.3–1) and
causal (1.0) edges, weighs them against each other as before.

## The switch

Migration 0015 nulls every vector not tagged `fastembed:intfloat/multilingual-e5-small`
and deletes `semantic` edges. The embed backlog re-embeds and redraws edges;
`POST /v1/banks/{bank}/relink` once per bank after `unembedded` reaches 0
fills in the edges early-drained nodes could not see.

Two things have no backlog and are handled separately: mental models are
re-embedded once when the embedder finishes loading, and a bank's
consolidation round waits while more than 10% of its nodes are unembedded — a
round run mid-drain would pool and dedup against a fraction of its
observations and create duplicates that outlive the drain.

The offline preview below came first; the calibration that set the gate is the
next section.

| e5 floor | items kept | useful kept | precision | prompts that abstain (of 14 truly empty) |
|---|---|---|---|---|
| 0.82 | 60% | 82% | 35.0% | 3 (0) |
| 0.83 | 45% | 67% | 38.0% | 5 (2) |
| 0.84 | 33% | 56% | 43.7% | 12 (5) |

## After the switch (2026-10-15)

15,882 nodes re-embedded in 121 s; four mental models re-embedded at start;
every bank relinked.

**Edge density.** Counted the same way on the stored vectors (top-20
same-type neighbours, date suffix included): bge at 0.70 gives 60,189 edges in
the `memgarden` bank, e5 at 0.87 gives 73,866 (+23%; 0.88 would give 64,359).
The suffix was the gap named above. Left at 0.87: a +58% density change scored
exactly 0 on the gold set before (CE-7).

**The gate, calibrated live.** The same 80 prompts against the re-embedded
store: 1,112 items, 353 carried their earlier label, 759 new ones labelled the
same way.

- Precision with no gate: **25.6% → 32.2%**, from the model swap alone.
- Cosine AUC on live results: 0.636 — below the offline 0.715, because e5 now
  chooses the candidates, so the negatives are harder.
- Only 6 of 80 prompts had nothing useful. Abstaining on a whole prompt
  catches almost none of them (at 0.84: 7 abstentions, 1 of them empty), so
  the gate is a **per-item floor**, not a per-prompt abstention.

| floor | items kept | useful kept | precision |
|---|---|---|---|
| none | 100% | 100% | 32.2% |
| 0.82 | 72% | 78% | 35.0% |
| **0.83** | 62% | 72% | 37.0% |
| 0.84 | 51% | 64% | 39.9% |
| 0.85 | 38% | 52% | 44.0% |
| 0.86 | 23% | 37% | 52.6% |

Shipped at 0.83 (`[recall] min_semantic`), on the HTTP route only. Items with
no semantic score (keyword or graph only: 154, 34 of them useful) fall below
any floor, as they did in this table. The labeller is generous on the useful
side, so the useful items lost are mostly the weakly useful ones.
