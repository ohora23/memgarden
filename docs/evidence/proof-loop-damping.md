# The `proof_count` loop — damped at the pooling recall, measured at the injection

**Date** 2026-09-05 · **Ledger** `gold/results.jsonl` lines 15 (baseline, reproduces 14) and 16 (treatment)

## The loop

`proof_count` is the one recall signal recall writes back. Consolidation pools
existing observations *by recall* over each new fact, the LLM UPDATEs what it
was shown, and every UPDATE grows the target's `proof_count`
(`store::consolidate::recount_proof_tx`). With `scoring::proof_norm` inside
that pooling recall, a heavily-sourced observation ranks higher, is pooled
more often, is updated more often, and gains more sources. Nothing damped it.
The recorded failure is the first live round's ten-source observation that
dissolved into "Multiple components…" (`consolidate/prompts.rs`,
`observation_entry`).

Live distribution on 2026-09-05, 3,421 observations: 3,015 at one source, 264
at two, and a tail of 26 / 24 / 21 whose three texts are all legacy-era.

## The fix

`RecallParams.proof_alpha`. The consolidation pooling recall passes `0.0`;
the injection, reflect and the mental-model refresh keep
`scoring::PROOF_COUNT_ALPHA` (0.1). The pool is decided by relatedness alone;
the count stays a read-only signal for what a person sees. An update cap was
the alternative and was not taken: it changes what the consolidator may do
and creates a second observation for the same subject once the cap is hit.

## Does the injection-side boost earn anything?

One arm, one variable, same frozen corpus (`baee3f40…`), `semantic=0.1` on
both. The gold set can see the term: 16 of the 57 relevant observations carry
more than one source.

| arm | recall@5 | recall@10 | MRR | nDCG@10 |
|---|---|---|---|---|
| line 15 `proof=0.1` (production) | 0.2379 | 0.4027 | 0.7095 | 0.3729 |
| line 16 `proof=0.0` | 0.2379 | 0.4027 | 0.7095 | **0.3752** |

Identical to the digit on three metrics; nDCG@10 +0.0023, from two adjacent
swaps in the identifier stratum (one query up, one down). The boost never
moved a relevant item across the cut. It is kept at 0.1 for parity, and the
number says removing it would cost nothing — which is the point: the loop was
feeding a term that was not paying for itself.

## Two one-line fixes shipped alongside

* `store::consolidate::unconsolidated` / `count_unconsolidated` now apply the
  same liveness predicate as `search::hydrate` (`superseded_by IS NULL`,
  `expires_at` in the future). A retracted fact was consolidation input.
* `retain::all_failed` is `chunks_done == 0 && chunks_failed > 0`, not
  `facts_written == 0`: a transcript of empty chunks beside one 500 was
  failed, its cursor rewound, and every empty chunk re-extracted.
