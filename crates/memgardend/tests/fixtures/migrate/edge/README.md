# `edge/` — synthetic, and deliberately so

Every file under this directory is **hand-written**. Nothing here came off the
live legacy daemon.

It exists for shapes that are **legal per
`hindsight-api-slim/hindsight_api/engine/transfer/schema.py` but absent from
today's corpus**, censused over all 3,540 facts and 1,747 observations in the
four non-empty banks on 2026-08-06:

| bank | shape | why it is not in `real/` |
|---|---|---|
| `edge::legal-but-absent` | `context: ""` | `context` is never `""` or null in any of the 3,540 live facts |
| `edge::legal-but-absent` | `target_fact_index: 999` | every live causal target resolves; the range check is D2's, before any write |
| `edge::legal-but-absent` | duplicate `(document_id, fact_index)` source pairs | **real, but in the wrong bank** — 86 of them, all in `claude-code::bank-b`, none in the `bank-a` slice `real/` is taken from |
| `edge::null-original-text` | `original_text: null` | `schema.py:125` types it `str \| None`; 24/24 live documents are non-null |
| `edge::observation-scopes` | `observation_scopes: "per_tag"` | null in all 1,747 live observations |

The first bank must **pass** `assert_integrity` — legal-but-unusual is not a
refusal. The other two must fail, each on its own named error.

The counterpart is `../real/`, a redacted slice of the live
`claude-code::bank-a` archive, which carries the shapes a generator
would not invent. Keep the two apart: the plan's first draft conflated them and
specified a fact-level `document_id` and a `context: ""` that no live fact has.
