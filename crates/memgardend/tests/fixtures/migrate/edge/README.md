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
| `edge::two-documents` | the **same** entity named with different casing in two documents, plus a near pair that must stay apart | every committed real slice is a **single document**, so `entities::resolve_fact`'s cross-document behaviour — the thing MG-1 step 4 changed from `recall_bench`'s raw-name path — had no coverage at all. With one document `load_resolution_context` returns an empty candidate list and the resolver degenerates to `normalize` |

| `edge::fuzzy-merge-bait` | two documents sharing a date, one naming `CE-1` + `Phase A`, the other `CE-4` + `Phase A` | `edge::two-documents` looked like it guarded step 4's decision and did not: its `Postgres`/`SQLite` pair shares no co-occurring partner, so it scores under the 0.6 gate with **or** without the fuzzy pass, and reverting the fix left the whole suite green. Here `ce-4` scores `0.375 + 0.3 + 0.2 = 0.875` and collapses into `ce-1` — the live corpus's headline failure in three facts |

`edge::legal-but-absent`, `edge::two-documents` and `edge::fuzzy-merge-bait` must **pass**
`assert_integrity` — legal-but-unusual is not a refusal. The other two must
fail, each on its own named error.

The counterpart is `../real/`, a redacted slice of the live
`claude-code::bank-a` archive, which carries the shapes a generator
would not invent. Keep the two apart: the plan's first draft conflated them and
specified a fact-level `document_id` and a `context: ""` that no live fact has.
