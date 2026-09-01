# Write-time supersession detection: measured, and shipping off

CE-12 gives a fact a lifecycle — `superseded_by`, `expires_at`, a filter in
`search::hydrate`, an HTTP verb to set them. This document is about the half
that was supposed to *fill* those columns automatically, and does not work.

**Measured 2026-08-30**, against a copy of the live bank
(`claude-code::memgarden`, 2,103 nodes of 7,831) on a daemon at `:9111`. The
live daemon on `:9100` was never touched and is still on schema v10.

## What was built

Extraction is shown the bank's nearest existing facts and may declare that a
new fact retracts one of them:

```
chunk → embed (bge-small, in-binary) → KNN k=12 → hydrate → KNOWN FACTS block
      → the one extraction call that was already happening
      → facts + `supersedes` positions → nodes::mark_superseded
```

No second LLM call. The candidate lookup costs one embedding and one
brute-force KNN — single-digit milliseconds against an extraction that costs
seconds — which is why it rides the existing call rather than adding one:
AC-1 already found retain to be throughput-bound.

## The eval set

Seven chunks. **Every one is verbatim project record** — a retraction entry
from the operator's own memory vault, or the text of a stored fact — because a
measurer who writes the inputs is measuring their own prose. The four positive
cases are this project's own documented retractions:

| case | the chunk says | the stored fact it makes false |
|---|---|---|
| P1 | the gold harness reproduces its baseline exactly | "골드 하네스가 자기 baseline을 재현 못 한다" ×5 |
| P2 | AC-1's 6/2/5 was discarded for a knob defect | "AC-1은 6 대 5로 성립" ×4 |
| P3 | AC-1 was signed 2026-08-20, 13/5/1 | "사용자 서명을 기다리고 있다" ×4 |
| P4 | PR #36 rejected the CPU-3 hypothesis, control ran 25h27m | "CPU 3 결함 가능성 재검토 — 3/3 크래시 모두 CPU 3" |

Three negative cases carry no retraction at all: a progress note (PRs merged),
an additive measurement (recall p50/p95), and an unrelated one (an Obsidian
index edit).

A target only counts as reachable if the KNN actually put it in the twelve
candidates. **3 of 4 positives were reachable** — P2's targets never surfaced,
which is a retrieval miss, not a judgement one, and is scored as such.

## Three arms

| arm | prompt / schema | detected (of 3 reachable) | false positives | negatives clean |
|---|---|---|---|---|
| **A** | free list, up to 3 positions | 2 | **22** | 0 / 3 |
| **B** | + "same topic is not retraction", 1 position | 2 | **14** | 0 / 3 |
| **C** | + a quote of the false span, checked in code | **0** | **0** | 3 / 3 |

Arm A's worst chunk is the one that matters: given P3 — AC-1 was signed — the
model named **all twelve candidates it was shown**, including `AC-3 무손실
마이그레이션 완료`, which the chunk does not mention. In production that single
chunk would have retired twelve facts.

Arm B halves the false positives and changes nothing about the conclusion.

Arm C is the interesting one. Requiring the model to copy the span it claims is
now false — and *checking* the copy against the candidate in Rust, rather than
trusting the instruction — drives false positives to zero. It drives detections
to zero as well. Probing the raw field shows why: across the two positive
chunks, `superseded_quote` came back **0 times**. Asked to justify a
retraction, the model stops claiming one.

## Then a worse finding

The three new fields were **optional** properties in the decoding grammar. On
this model that means they are never produced at all: `expires_at` appeared in
**0 of 11** extracted facts across a separate seven-chunk expiry set (three
genuinely temporary facts, four durable ones — 0 detected, 0 false positives).

The ported string fields are all `required` with `"N/A"` as the absence marker,
and `parse::get_value` already reads `"N/A"` as absent. Matching that idiom
made the model fill every field correctly:

```
{"facts":[{"expires_at":"N/A","fact_type":"world","superseded_quote":"N/A",
           "supersedes":[],"what":"User has an exam tomorrow ...
```

and then the reply degenerated —

```
... "}}]}}]}}]}}]}}]}}]}}]}}]}}]}}]}}]  (×80)
done_reason="length"  eval_count=8192  truncated=true
```

— hit `num_predict`, and the chunk's facts were **all lost**, 502. That is the
24,525-character runaway `MAX_FACTS_PER_CHUNK`'s comment describes, brought
back by three extra required properties. The optional form is inert; the
required form is destructive. No third setting was found.

## What ships

`[retain] detect_supersession = false`, and with it off the extraction prompt is
**byte-identical to the pre-CE-12 prompt** — asserted, not asserted-to:
`system_prompt(false, false)` is 6,999 characters and `system_prompt(true,
false)` is 7,615, the two numbers the snapshot test carried before this change.
`expires_at` rides the same switch even though it needs no candidate list,
because its only working form is the one that truncates.

The gold harness confirms the read path is unaffected: **0.035 / 0.209 / 0.370
r@1/@5/@10, MRR 0.516, nDCG@10 0.306** — identical to ledger row
`8da575e6`. It has to be: nothing is marked, so the filter matches every row.
That is a parity check, not a win, and it is the only claim it supports.

The mechanism ships and is used by hand:

* `POST /v1/banks/{bank}/nodes/{id}/supersede {"by": <node>}` and `DELETE` on
  the same path. Two verbs, because one `PATCH` with an `Option` field is what
  made a mental model's `trigger` unclearable through the API
  ([`mental-model-supersession.md`](mental-model-supersession.md)).
* Guards live in `nodes::mark_superseded`'s `WHERE` clause — same bank, not
  already retracted, replacement strictly newer, not itself — so a refusal is a
  409 rather than a 200 that wrote nothing. Each one is mutation-tested.

## Do not re-propose

* **Weighting recency in the prompt.** Rejected before this work and unchanged
  by it: a correction can itself be corrected.
* **More prompt words telling the model what is not a retraction.** That is arm
  B. It is worth 8 false positives out of 22 and no detections.
* **A quote requirement as currently written.** That is arm C. Zero and zero.
* **Making the lifecycle fields required.** That is the truncation above.

## What has not been tried

* A **second, dedicated call** — one new fact, N candidates, "which are now
  false" — instead of folding the judgement into extraction. It is the obvious
  next arm and it was skipped on cost: roughly double retain wall-clock on a
  path AC-1 already found throughput-bound. That trade is worth re-opening only
  with a number attached.
* A larger extraction model. Untested, and this machine's GPU already belongs
  to one 14B model.
* Detection at **consolidation** time rather than retain time, where an LLM
  pass over related facts already happens and already emits `deletes` for
  superseded observations.

## Reproducing

The eval scripts are in the session scratchpad, not the repository: their
inputs are real memories. What is committed is the route they use —
`POST /v1/banks/{id}/dry-run-extract` returns `candidates` (the list the model
was shown, in order) and each fact's `supersedes` and `superseded_quote`
**as answered**, including when verification then rejects them. A retraction
dropped with no way to see why is the invisible-failure shape that left CE-10
broken for two months.
