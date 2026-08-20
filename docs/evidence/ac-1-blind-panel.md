# AC-1 — re-measured blind, after finding the first measurement unfair

The 2026-08-12 judgement was mine, on a query set that turned out to be a third
unjudgeable, against a comparison whose knobs were wrong. This is the redo:
the same gate, judged by panels that did not know which system was which, on
both systems' actual shipped settings.

## The measurement defect, first, because it invalidates the earlier run

The two APIs name the same knob differently and **neither server rejects a
field it does not know**:

| | injection cap | fact types |
|---|---|---|
| `memgardend` | `maxTokens` | `recallTypes` |
| legacy | `max_tokens` | `types` |

The harness sent `max_tokens` to both. Legacy received 512; MemGarden silently
ignored it and ran on its 1024 default. On one query that is **legacy 2 items
against MemGarden 12** — the comparison was between one system on half the
budget and the other on all of it.

`ac-1-memcompare.md` also records `budget=low, max_tokens=512` as "what both
hooks send under the `coding` profile". That is wrong for both. `512`/`low` is
`ConsolidationConfig`, a different struct on a different path. As deployed:

* **legacy** — `~/.hindsight/claude-code.json` sets neither knob, so
  `recall.py` falls back to `budget="mid"`, `max_tokens=1024`;
* **memgardend** — `Config::defaults` gives `recall.max_tokens = 1024` and
  `profile.recall_budget = "mid"`.

Both ship **mid / 1024 / all three fact types**, which is what this run used.

The knobs are not equivalent between the systems in any case. Legacy's
`max_tokens` binds hard — on one query 512 → 2 items, 1024 → 9, 4096 → 57 —
while `budget` barely moves it. MemGarden's returned count is insensitive to
both over the same range. Matching the *numbers* is not matching the
*behaviour*, and this run matches what each system actually ships.

## Method

* **Query set, 20.** The five distinct queries from the legacy A/B log
  verbatim, plus fifteen the *bank* chose: its fifteen most-mentioned entities
  through one fixed template. The judge does not pick the topics, and the six
  shadow prompts from the earlier set are gone — five of six were unjudgeable
  there, and the shadow log stores `prompt_chars`, not the prompt, so they
  could not be recovered anyway.
* **Frozen.** Both systems answered once; the answers are a snapshot. The bank
  gains memories daily, so a judgement re-run against the live systems is a new
  measurement, not a check of an old one — which is why the 2026-08-12 verdicts
  cannot be reproduced at all.
* **Blind.** Each query became a file with the two lists as `A` and `B`, sided
  by a per-query hash. Three judges per query, each with a different lens (the
  asker; a strict item-by-item counter; an adversary told to argue against
  whichever side looks better). Majority of three. The salt changed between
  rounds, so 12 of 20 sides swapped.
* **Same criteria**, unchanged: `ac-1-criteria.md`, committed before the first
  query of the original run.

## Result

| | better | worse | equivalent |
|---|---|---|---|
| my solo judgement, 2026-08-12 (unfair knobs, old set) | 6 | 5 | 2 + 7 unjudgeable |
| blind panel, **unfair knobs** | 16 | 3 | 1 |
| **blind panel, as deployed** | **12** | **4** | **4** |

**The gate condition (`worse ≤ better`) holds 4 to 12**, with no query
unjudgeable — the new set is answerable in a way the old one was not.

**Correcting the knobs moved four queries away from MemGarden.** That is the
number worth keeping: the 16-to-3 was a system with six times the budget
winning, and reporting it would have been reporting the defect as a result.

## The four losses, in the judges' words

* **ab5** — *"업스트림 PR의 최종 상태(#3086 OPEN)를 아예 놓침"*, filling the
  budget with this project's own CE PR stack instead. Topic drift.
* **en9** — *"problems stated, fixes missing"*: the symptom retrieved, the
  remedy not.
* **en15** — *"command-execution records crowding out fixes"*. This is the one
  place the 2026-08-12 diagnosis survives independent judging.
* **en14** — *"both missed the fact entirely (no problem/fix records in the
  bank)"*. A corpus gap; no ranking change reaches it.

## What this cost on the labelled benchmark, exactly

The recall dedupe shipped alongside costs **recall@10 −0.0087, nDCG@10
−0.0105, MRR unchanged** against ledger line 12 (now line 13).

Two queries moved and no others: **q01 −0.0556** and **q19 −0.0667**. Both are
queries where two *labelled-relevant* corpus nodes carry byte-identical text,
so the dedupe drops one and `recall@k`, which counts uuids, scores it as a
memory not retrieved. Seven of the 132 labelled-relevant nodes are in an
exact-duplicate group; three of them duplicate another labelled node.

The reader receives the same sentences either way. The drop is the metric
counting a copy, and it is recorded here so the ledger row is not read later
as a quality regression.

## What was checked and is not a defect

`passthrough_base` uses `n = merged.len()` — the whole fused candidate pool,
286-309 on live queries — so the base spread across the returned window is
0.03-0.06, and with all three boosts degenerate the final order equals the RRF
order. That looks like a porting bug and is not one: legacy computes
`n = len(scored_results)` after capping at `reranker_max_candidates = 300`
(`memory_engine.py:5070`, `config.py:925`, not overridden in the live profile),
which is the same magnitude. **Changing it would be a divergence from the
reference, not a repair of a deviation from it** — and the reference is what
AC-1 measures against.

Worth recording anyway, because it is the mechanism behind the flat scores
measured on 2026-08-12 and it is now explained rather than open:

```
candidates n=309, returned=12
final span   0.0477       (base span predicted from n: 0.0321)
temporal     0.50 .. 0.50   constant, and so cannot reorder
recency      0.943 .. 0.946 a 365-day window against a two-week bank
proof        0.50 .. 0.50   constant
semantic     0.715 .. 0.866 the only signal with real spread — and it enters
                            only as an ordinal, through RRF
final order == RRF order? True
```

## Limits

1. **The panel judges what the criteria describe**, and the criteria put hits
   before noise. A system returning more items has more chances to hit, and
   MemGarden returns more on 16 of 20 queries even at matched budgets. The
   judges were told a longer list is not automatically better; that is a
   mitigation, not a control.
2. **Three judges is a thin panel**, and five of the twenty verdicts were 2-1.
3. **One bank, one operator, one language mix.** Nothing here generalises past
   this corpus.
4. The panel is independent of the author, which the 2026-08-12 run was not.
   That is the one limit this run removes.
