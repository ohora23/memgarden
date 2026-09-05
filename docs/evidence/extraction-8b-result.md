# Extraction quality, 14B vs 8B — result

Criteria: `extraction-8b-criteria.md` (committed before either arm ran).
Questions: `extraction-8b-questions.jsonl`. Per-question labels, written
blind and unsealed afterwards: `extraction-8b-labels.json`.

**Decision: the 8 GB row does not hold.** `qwen3:8b` fits an 8 GB card and
is faster and cleaner than the 14B, and it loses on the one thing the
ledger and recall exist for — it extracts what things *are* and drops what
was *decided*. Rules 1 and 2 of §"The decision" fail; the README row now
says 12 GB minimum.

## Mechanical (M1–M5)

Same daemon build (`d9bc6e7`), same five transcripts replayed through the
real `Stop` hook, fresh database per arm, 45 chunks each.

| | A — Qwen3-14B Q6_K | B — Qwen3-8B Q4_K_M |
|---|---|---|
| chunks failed (M1) | 2 of 45 (4.4%) | **0** |
| parse failures + truncations (M2) | 2 (both output-limit truncations at 8,192 tokens; one lost a whole 1-chunk transcript) | **0** |
| facts (M3) | 262 (5.82 / chunk) | 214 (4.76 / chunk) |
| fact types | world 259 · experience 3 | world 158 · experience 56 |
| with `occurred_start` (M4) | 88 of 262 | 206 of 214 |
| job wall time, summed (M5) | 39.9 min | **12.6 min** |

On every mechanical row the 8B is the better citizen: it never overran the
output budget, never produced unparseable JSON, filled the temporal fields
the schema asks for, and ran three times faster.

## Judged (Q1–Q15, AC-1 rubric, blind)

| | A | B |
|---|---|---|
| 적중 total over 15 questions | **29** | 17 |
| B vs A per question | — | better 3 · equivalent 3 · **worse 9** |

Rule 1: `17 ≥ 0.8 × 29 = 23.2` — **fails** (ratio 0.59).
Rule 2: `worse 9 ≤ better 3 + 3 = 6` — **fails**.
Rules 3 and 4 (failure rates) pass, in B's favour.

Where the nine losses come from is the finding. The questions B lost are
the ones that ask what was concluded or decided:

- q04 (how the Orca-graph proposal changed the plan): A had the two plan
  changes as facts; B had only descriptions of what Orca's graph is.
- q08 / q09 (the PrimeIntellect analysis and what to take from prime-agent):
  A stored the conclusion — do not replicate, hook `verifiers` to a local
  endpoint, the "do not build" list, the interception proxy as the reusable
  piece. B stored the platform's layer diagram, commands and class names,
  accurately, and none of the verdicts.
- q12 / q14 (the role split between two memory systems; the status recap
  that answered "what now"): A had the decision and the recap; B had
  neither.

The three B wins are narrow: q13, where A's single chunk was truncated and
the bank was empty; q02 and q11, where B had one more confirming item.

So the smaller model is not worse at extraction in the mechanical sense.
It is worse at *selecting*: given a transcript in which an assistant
analyses something and reaches a verdict, it keeps the analysis and drops
the verdict. For a memory whose value is "what did we decide last week",
that is the wrong half.

## What changes

- README, *What GPU it needs*: the 8 GB row becomes "fits, measured short;
  12 GB minimum", with these numbers.
- The 8B's two mechanical wins are worth keeping in mind for the 14B: two
  of 45 chunks lost to the 8,192-token output cap, and `occurred_start`
  filled on a third of facts against nearly all of B's. Neither is a
  reason to switch models; both are prompt or budget work on the model
  that stays.

## Limits

As stated in advance: five transcripts, fifteen questions, one judge who
built the system and wrote the questions, a Korean-heavy corpus about this
project. The direction of the result (a 2:1 gap on decisions, 9 of 15
worse) is larger than a judge's lean would produce; the exact ratio is not
to be quoted beyond this README row. Not measured: an 8B at Q8 or with a
prompt tuned for it, or a 14B at Q4_K_M — the 12 GB row is the same model
at a smaller quant and inherits the 16 GB validation only by that argument.
