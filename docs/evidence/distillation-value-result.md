# The distillation layer earns its place — measured, with one loss

Run 2026-08-29 against the rules committed in
[`distillation-value-criteria.md`](distillation-value-criteria.md) before any
arm ran. Per-arm ledger rows in
[`gold/distillation-arms.json`](../../gold/distillation-arms.json).

**Headline: allowing `observation` nodes into recall raises recall@10 from
0.291 to 0.370 (+27%) and nDCG@10 from 0.262 to 0.306 (+17%).** It does so
against a gold key that is 71% raw facts, which is the grain the criteria said
would make a win hard and a loss meaningless.

## The three arms

Frozen AX-2 corpus, 2,718 facts, 20 graded queries, 14 scored (six are
recorded as unanswerable in this corpus and excluded, as always). One variable:
`recall_types`.

| metric | facts-only | **shipped** | observations-only | Δ shipped − facts |
|---|---|---|---|---|
| recall@1 | 0.039 | 0.035 | 0.026 | −0.004 |
| recall@5 | 0.180 | **0.209** | 0.131 | **+0.029** |
| **recall@10** | 0.291 | **0.370** | 0.177 | **+0.079** |
| MRR | **0.538** | 0.516 | 0.500 | −0.022 |
| **nDCG@10** | 0.262 | **0.306** | 0.196 | **+0.044** |

`recall@10` ceiling is 0.8588 in every arm — the labels never move, so the
arms are comparable by construction.

## Why the win counts more than its size

The criteria set this out in advance: **the gold labels are 236 `world` to 94
`observation`.** A key made mostly of raw facts cannot easily show an
observation beating them, because for most queries an observation is not in the
answer key at all.

The shipped arm wins anyway, on both coverage metrics, **against that grain**.
That is the asymmetry the rules named: a facts-only win would have been the
expected artefact of the labelling and worth little; a facts-plus-observations
win has to overcome it.

## Per stratum, and the one place it loses

| stratum | facts-only nDCG@10 | shipped nDCG@10 | Δ |
|---|---|---|---|
| temporal | 0.201 | **0.319** | **+0.118** |
| identifier | 0.316 | **0.416** | **+0.100** |
| graph | 0.372 | **0.465** | **+0.093** |
| **conclusion** | **0.131** | 0.113 | **−0.018** |

**The one loss is `conclusion`, and it is the stratum where distillation should
have helped most.** MRR there goes 0.200 → 0.143. It is a single scored query,
so this is a flag and not a finding — but it is the flag worth keeping, because
"summarise many facts into a conclusion" is exactly what the layer claims to
do, and it is the only place adding the layer made things worse.

There is a known reason it might be structural rather than a defect. AX-2's own
notes record that **conclusion-type answers largely live in the operator's
curated `MEMORY.md`, which this corpus does not cover** — so the stratum has
thin material on both arms and the query may be measuring corpus coverage
rather than ranking. That is a hypothesis, not an excuse; it is checkable by
labelling more conclusion queries, and until someone does, the loss stands as
recorded.

## Observations complement facts, they do not replace them

The third arm is the one that keeps the result honest. **Observations alone
score 0.177 recall@10 and 0.196 nDCG@10 — well below facts alone.** So the
shipped arm's gain is not "the synthesis is better than the sources". It is
"the synthesis is a *second way in* to material the fact ranking was missing".

That distinction matters for what gets built next:

* it argues **for** keeping consolidation running and for surfacing what it
  produces;
* it argues **against** any design that substitutes distilled nodes for their
  sources, or that injects a summary layer *instead of* facts;
* and it sets the shape for CE-10's mental models, which have **never run** (0
  rows): if they are wired, they belong as an additional entry point, not as a
  replacement, and the `conclusion` regression says to measure them on
  conclusion-type queries specifically before trusting them there.

## What this does not establish

1. **A retrieval benchmark is not a usefulness benchmark.** It says the right
   node comes back, not that a reader learns more from it.
2. **This corpus's observations were made by the legacy system**, not by CE-9
   on this bank. Applying the result to MemGarden's own 2,770 observations is an
   inference, and it is named as one. The live bank has no gold labels; giving
   it some is the obvious follow-up.
3. **14 scored queries, one of them the entire `conclusion` stratum.** Per
   stratum this is very small, and the distributions are what is reported.
4. **This says nothing about tokens.** The shipped arm returns *more* material.
   Whether the extra 0.079 of recall is worth the injection budget it consumes
   is a different measurement, and MX-3 is the only attempt so far.

## What changed in the tool

`recall_bench` gained a `types=` flag alongside its existing `semantic=` and
`rerank=` knobs, defaulting to all three fact types so every row already in the
ledger stays comparable. The arms differ by that flag and nothing else.
