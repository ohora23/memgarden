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

**Fixed 2026-08-30 (CE-12), by half.** The store now records that fact B
replaces fact A, and every reader honours it:

* `memory_nodes.superseded_by` (schema v11). A column and not an edge, because
  every reader of a fact has to know whether it is live, so the answer travels
  with the row `search::hydrate` already selects. As a link it would be a join
  on the hot recall path, and any caller that forgot the join would serve
  retracted facts.
* **`search::hydrate` is the one filter point.** BM25, KNN, temporal and the
  graph arm all pass through it, and so do `/reflect` and the mental-model
  refresh, which reach recall rather than the store. That is why the fix needed
  no change in `mental/` at all — the synthesiser stops being handed the dead
  version because nothing hands it out any more.
* `memory_nodes.expires_at`, for the smaller sibling problem: a fact that stops
  being true on its own.

**The prediction in the section above held.** Detection is the hard part, and
it is *still* the hard part: the write-time detector was built, measured
against this project's own recorded retractions, and **ships off** — 22 false
positives on seven chunks in its first form, and zero detections in the form
that has zero false positives. All three arms, and the grammar truncation that
killed the fourth, are in
[`supersession-detection.md`](supersession-detection.md).

So the retraction is a first-class, filterable state that today is set **by
hand**:

```
POST   /v1/banks/{bank}/nodes/{id}/supersede   {"by": <node id>}
DELETE /v1/banks/{bank}/nodes/{id}/supersede
```

**Still not done.** Nothing sets it automatically. The three fixes this section
originally listed stand where they stood, with one now measured:

* **Detection at retain time** — built, measured, off. Not a dead end so much
  as an unpriced one: the arm that was never run is a *second* LLM call, and it
  costs roughly double retain wall-clock.
* **Recency weighting inside the refresh prompt.** Cheap, and wrong in the way
  described above. Unchanged.
* **Exclude facts that a later fact cites as withdrawn.** Same detection
  problem, now with a place to write the answer.

## Also found: `trigger` cannot be cleared through the API

**Fixed 2026-08-30**, the same way and for the same reason:

```
DELETE /v1/banks/{bank}/mental-models/{mm_id}/trigger
```

A verb, not a nullable field. `PATCH` is unchanged and still cannot clear
anything — a JSON `null` in its body means what it always meant, which is
nothing, and a test asserts that so the two mechanisms cannot drift into
disagreeing. The store side is `mental_models::Patch::clear_trigger`, matching
the `clear_embedding` flag that was already there for the same reason.

404 when the model does not exist *or* has no trigger: both mean "there is no
schedule here to turn off", and the operator's next move is the same either
way.

CE-12's own column got the identical treatment rather than inheriting the
defect — `POST` to set, `DELETE` to clear.


Turning the model off had to be done with SQL. `PATCH
/v1/banks/{id}/mental-models/{mm_id}` takes `Option<String>` per field where
`None` means *"leave this one alone"* (`mental_models::Fields`), so a JSON
`null` is indistinguishable from an omitted key and the trigger survives.

Creating a model with no trigger works; **un-triggering one does not**. That is
a real gap in the write surface, and it is the operation an operator reaches for
first when a model starts producing something wrong — exactly what happened
here.

## Status

**Half fixed, 2026-08-30.** The store, the recall filter and the write surface
landed as CE-12 (schema v11), and the `trigger` gap this document also recorded
is closed. Automatic detection was built, measured and turned off — that is the
one item still open, and it is in `docs/PRD.md` under the post-v1 list.
The three healthy models keep running; whether the tier earns its place is what
the usage signal from #42 is there to answer, and one wrong model out of four is
part of that answer rather than a reason to stop.
