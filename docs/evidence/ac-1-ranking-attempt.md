# The ranking fix that measurement did not support

`ac-1-memcompare.md` closed with a diagnosis: four of the five `worse`
verdicts came from MemGarden ranking *"a command was executed"* records too
highly, and the recommendation was to fix that first. This is what happened
when that was measured. **Nothing was shipped.** The diagnosis is
downgraded from a cause to a hypothesis that the evidence does not carry.

## What the scores actually look like

Score breakdown for the query MemGarden lost worst
(*"agentmemory 관련해서 어떤 문제가 있었고 어떻게 해결했나?"*), twelve
returned items:

```
final   1.051 … 1.093      ← the entire spread is 0.042
recency 0.97 on every item
proof   0.50 (neutral) on all but one
rrf     0.028 … 0.035
semantic 0.730 … 0.899     ← the only signal with any spread
```

`combined = base * recency_boost * temporal_boost * proof_boost`, and `base`
is `passthrough_base(n, rank0)` — a linear map of RRF rank onto `[0.1, 1.0]`.
With several hundred candidates, twelve adjacent ranks differ by 0.03 in
`base`, and every multiplicative boost is identically neutral because the
bank's facts all fall inside a two-week window against a 365-day recency
decay.

**So ordering is decided by semantic similarity alone, and the embedding
scores "agentmemory doctor was executed" at 0.750 against a genuinely
relevant item at 0.752.** That is the real finding, and it is not the
finding the memcompare note recorded.

## Candidate 1 — narrow the recency window

`RECENCY_WINDOW_DAYS = 365` is a legacy constant (`reranking.py:32`) ported
for fidelity. Measured on the AX-2 gold harness, same database, same labels:

| window | r@1 | r@5 | r@10 | MRR | nDCG@10 |
|---|---|---|---|---|---|
| **365d (shipped)** | 0.035 | **0.218** | **0.379** | 0.516 | 0.317 |
| 30d | 0.040 | 0.211 | 0.372 | 0.546 | 0.322 |
| 7d | 0.044 | 0.192 | 0.370 | 0.567 | 0.308 |
| 3d | **0.048** | 0.192 | 0.374 | **0.649** | **0.327** |

MRR climbs by up to +0.133 — comparable to what CE-11's cross-encoder buys —
but `recall@5` falls 0.218 → 0.192. It is the same trade CE-11 documents:
rank 1 improves, coverage degrades. And 3 days is suspiciously close to the
gold corpus's own 3-day span, so the best-looking row is the one most likely
to be fitting the corpus rather than the problem.

**Not shipped.** A constant that wins MRR by losing recall needs a decision
about which the injection is optimising for, and that decision is not this
note's to make.

## Candidate 2 — penalise action records

A text test for "the subject is a tool and the verb is *was executed*",
verified at 20/20 precision on a random sample of what it catches, matching
72 of 3,200 nodes in the live bank and 43 of 2,718 in the gold corpus.
Applied as a multiplier on `final`:

| penalty | r@1 | r@5 | r@10 | MRR | nDCG@10 |
|---|---|---|---|---|---|
| 1.0 (none) | 0.035 | 0.218 | 0.379 | 0.516 | 0.317 |
| 0.9 / 0.8 / 0.5 / **0.0** | 0.035 | 0.219 | 0.382 | 0.522 | 0.321 |

**Every penalty strength produces the same numbers, including 0.0 — deleting
the matches outright.** The gain is +0.003 to +0.006, which is noise. On the
gold corpus this problem barely exists.

On the live bank it does do something: the losing query drops
`doctor --dry-run` and an endpoint-probe record, and gains one real hit
(*"CLAUDE.md의 agentmemory 지침에서 project 경로가 시스템 규약과 다름"* —
one legacy had and MemGarden did not). Four noise items the pattern cannot
see stay put, and MemGarden still trails legacy on that query.

**Not shipped**, for a reason stronger than the thin gain:

> **Five of the gold corpus's 276 labelled-relevant nodes match this
> pattern**, and one of them is `The Bash command 'agentmemory doctor
> --dry-run 2>&1' was executed` — *the exact item this judge called noise in
> memcompare #15.*

The human who labelled the gold set and the judge who ran memcompare
disagree about whether that item answers the question. Shipping a penalty
would encode one of those opinions into the ranker, and it would be the
opinion of the party that also built the system.

## The correction this forces

`ac-1-memcompare.md` states that four `worse` verdicts "turn on" action-record
ranking, and calls it "a ranking problem with a specific shape [that] is
actionable". **Downgrade both claims.** What is established: MemGarden
returns those records and legacy returns fewer. What is *not* established:
that they are the reason the queries were lost, or that suppressing them
recovers the loss. The one measurement that could have shown it — the gold
harness — shows +0.003.

The 6-to-5 memcompare margin stands unchanged. It was never resting on this
fix.

## The defect this run reported, and why it was not one

**Retracted 2026-08-19.** This section originally read "the gold harness no
longer reproduces its own ratified baseline" and recommended fixing it before
the next ranking attempt. It was a misreading, and the correction is more
useful than the claim was.

`ac-1-shadow.md` quotes **0.3881 / 0.5221 / 0.3236** as the baseline the
harness reproduces bit-identically. That was true when it was written — those
figures are `gold/results.jsonl` **line 8**. Two later runs at the same corpus
digest and the same configuration, **lines 11 and 12**, both record
**0.3792 / 0.5162 / 0.3168**, which is what a fresh import benches today. The
number moved for a reason that was already written down: CE-7's semantic-link
fix took the gold corpus from 681 semantic edges to 43,830, and the denser
graph cost recall — `README.md` tabulates it and
`docs/design/mg-1-migration.md` explains it. Line 8 is the *thin-graph* number
and has been superseded since PR #2.

Measured before retracting, because "it is deterministic" is a claim too:

| | |
|---|---|
| two imports of the frozen corpus into separate databases | nodes, links, entities and the raw `vec_nodes` vectors all **hash-identical** |
| benching either database | `0.3791717269658446 / 0.5161564625850340 / 0.3167967967859271` |
| the ledger's newest matching row (line 12, `eadbe0e`) | the same, to all sixteen digits |

**The harness is reproducible. What was missing was that the run never read
the ledger it was being compared against** — the comparison happened in a
person's head, against a figure copied into a document months earlier.
`recall_bench bench` now prints the newest ledger row for the same corpus and
`rerank_top_k`, the delta to it, and `reproduces line N to the digit` when
there is none. It reads the ledger even when the run writes nothing, since
that is exactly the case where a stale figure in a document is the only thing
left to compare against.

The A/B results in this note are unaffected: every one of them was
same-database, and the absolute numbers above are the current baseline rather
than a drift away from it.
