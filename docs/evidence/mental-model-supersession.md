# The synthesiser cannot tell a retracted fact from a current one

Observed 2026-08-30, on the first three mental models created after CE-10 was
repaired (#39, #40, #41, #42). One of the three came out **confidently wrong**,
and the reason is structural rather than a bad prompt.

## What happened

Four models now exist on `claude-code::memgarden`. Three synthesised well. The
fourth — *"수용 기준과 전환 게이트"*, source query `AC-1 AC-2 AC-3 AC-7 게이트
서명 …` — produced this, citing 17 nodes:

| the synthesis says | the record says |
|---|---|
| "AC-1 게이트는 … **사용자 서명을 기다리고 있다**" | signed 2026-08-20 |
| "AC-1 결과는 **6승 / 2무 / 5패 / 7건 판정불가**" | that is the **discarded** first measurement; the blind re-run was 13/5/1 |
| "**AC-7 게이트는 관련 정보가 제공되지 않았다**" | signed 2026-08-26 |

Every one of those statements was true when it was written. All three have
since been superseded, and the bank holds both versions.

## Why it is not a prompt problem

MemGarden tracks **when** a fact entered (`created_at`), and `refresh_watermark`
records the newest `created_at` a refresh has already folded in — so a refresh
knows what is *new*. Nothing anywhere records that fact B **replaces** fact A.

Retraction in this project is a first-class event: the first AC-1 measurement
was discarded for a knob defect, the 64× semantic-link claim was withdrawn, the
CPU-3 conclusion was withdrawn twice, the gold-harness "cannot reproduce"
finding was retracted a day after it was filed. Each retraction is a *new fact
that contradicts an old one*, and to the store they are two rows with different
timestamps and no edge between them.

So the synthesiser receives both, has no signal that one is dead, and averages.
Asking it to prefer recency in the prompt is the wrong fix twice over: the
newest fact is not always the correct one (a correction can itself be
corrected), and a prompt instruction is exactly the kind of guard this project
has watched a 14B model ignore — the `coding` profile's retain mission already
says "ignore routine tool output" and command logs were 6.4% of one ingest era
anyway.

## Why this matters more than one bad model

**The distillation tier is the one place where being wrong compounds.** A stale
raw fact is one bad row among 7,683 and the ranking may never surface it. A
stale *synthesis* is a document that reads as authoritative, gets cited by
`/reflect`, and is refreshed on a schedule — so it keeps asserting the dead
version until someone reads it closely.

This is also the shape of the failure that ran through the whole of the
2026-08-26..30 work: **reading a stored value as the current one.**
`include_tool_calls` was called `true` because a bank `mission` string said so;
the census scored syntheses as on-disk because their constituent terms were.
Same error, now made by the model instead of by the author.

## What was done, and what was not

**Done.** The model's `trigger` is cleared, so it no longer refreshes and no
longer reports due. Its content is left in place rather than deleted, because
it is the evidence for this document. The other three keep
`@after-consolidation`.

**Not done.** No fix. There is no measurement yet that says what a fix should
optimise, and the candidates all have real costs:

* **A `supersedes` edge**, written when a retraction is retained. The graph
  already has typed links, so the schema can carry it. The hard part is
  *detection*: recognising "this retracts that" is the same problem as the
  entity-merge one, where three measured attempts failed and character
  similarity was found unable to separate the cases.
* **Recency weighting inside the refresh prompt.** Cheap, and wrong in the way
  described above.
* **Exclude facts that a later fact cites as withdrawn.** Requires the citation
  to exist, which is the same detection problem.

## Also found: `trigger` cannot be cleared through the API

Turning the model off had to be done with SQL. `PATCH
/v1/banks/{id}/mental-models/{mm_id}` takes `Option<String>` per field where
`None` means *"leave this one alone"* (`mental_models::Fields`), so a JSON
`null` is indistinguishable from an omitted key and the trigger survives.

Creating a model with no trigger works; **un-triggering one does not**. That is
a real gap in the write surface, and it is the operation an operator reaches for
first when a model starts producing something wrong — exactly what happened
here.

## Status

Recorded, not fixed. Both items are in `docs/PRD.md` under the post-v1 list.
The three healthy models keep running; whether the tier earns its place is what
the usage signal from #42 is there to answer, and one wrong model out of four is
part of that answer rather than a reason to stop.
