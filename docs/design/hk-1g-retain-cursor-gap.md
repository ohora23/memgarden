# HK-1g — the retain cursor gap

The two-part defect `docs/design/c4b-hook-retain.md` §Known limits recorded and
deliberately deferred. It stopped being theoretical on **2026-08-05**, on the
first real retain of the shadow run.

Branch `fix/retain-cursor-gap`. **711 tests in the workspace** (+12), schema
**v8**.

---

## What happened

The first forced retain of the shadow run, against the real daemon:

```
FINAL done · chunks done/failed/skipped: 2 1 0 · facts: 12
error: failed to parse ollama response as JSON after retries:
       expected `,` or `}` at line 1 column 4681
```

One chunk's Ollama reply was malformed JSON after every retry. The job
finished **`done`** — correctly, per C4b's reasoning: 12 facts *were* written,
and failing the job would duplicate them on every re-send and wedge the session
permanently when the chunk fails deterministically.

The session row afterwards:

| | |
|---|---|
| `byte_offset` (optimistic) | 3,459,173 |
| `confirmed_offset` (durable) | **0** |
| `inflight_bytes` | 3,459,173 |

That gap is the design working. What was broken is what happens *next*.

---

## The defect: a clean job confirmed over a gap it never carried

The worker's clean-run block wrote `confirmed_offset = task.byte_offset`
**unconditionally**, and `store::sessions` merges that column with `MAX`. So
the next clean job — covering 3459173..N — would have set the durable cursor to
N, erasing every trace of the 0..3,459,173 that nothing had settled.

The instrument would then read **zero outstanding bytes over permanently lost
ones**, which is the one reading it must never produce: `inflight_bytes` is the
number Phase F's AC-3 claim is built on.

C4b named the fix and, more usefully, named the **wrong** fix:

> `confirm_if_settled`'s guard is `confirmed_offset >= byte_offset` on the
> pre-update row, and the *request* path has already written
> `byte_offset = this job's end` before the worker runs. So an ordinary clean
> job reads `0 >= 5000` → false and **never confirms**: `confirmed_offset`
> freezes at 0 forever.

`the_range_guard_confirms_where_the_settled_guard_would_freeze` asserts both
halves of that paragraph on one row, so the trap is pinned rather than
remembered.

---

## The fix, in the four pieces C4b specified

| # | piece | where |
|---|---|---|
| 1 | `offset_from` on `RetainRequest` | `routes/retain.rs` |
| 2 | persisted on the job row | migration `0008`, `store::retain_jobs` |
| 3 | a **third** `SessionUpdate` channel, guarded on `confirmed_offset >= offset_from` | `store::sessions` |
| 4 | one line in the hook | `cmd/retain.rs` |

The guard needs the job's **start**, and no persisted row had one. That is the
whole reason this is a migration rather than a store-only edit.

```sql
confirmed_offset = max(
                     confirmed_offset,
                     CASE WHEN confirmed_offset >= ?11   -- offset_from
                          THEN ?17 ELSE 0 END,           -- offset_to
                     CASE WHEN confirmed_offset >= byte_offset
                          THEN ?16 ELSE 0 END)           -- confirm_if_settled
```

Two guarded channels into one column, and — the change that matters —
**neither is unconditional any more**. The unconditional channel was not kept
"for the worker": it was removed, because a channel that can erase a gap is a
channel someone will eventually route through.

### Four decisions inside it

**`confirm_range` is a pair, not two fields.** A start without an end cannot
guard anything and an end without a start is the defect. The type makes half a
range unrepresentable.

**`None` does not confirm.** A caller that cannot name its start — an older
hook, a `curl`, anything that is not this hook — leaves the durable cursor
alone rather than advancing it on the assumption that it started at 0. That
over-reports outstanding work, which is the safe direction for an instrument
whose whole job is to notice loss. It is also what keeps the fix from silently
un-fixing itself against a stale binary.

**The insert branch carries the same guard.** A first write may confirm only a
range starting at 0; a job that starts mid-transcript against a row that does
not exist is a gap by definition, because nobody confirmed 0..from.
`a_first_write_confirms_only_a_range_that_starts_at_zero` covers both
directions.

**`offset_to` is stored even though only `offset_from` guards anything.** A job
row that cannot answer "which bytes did you carry?" forces every reader back to
`sessions.inflight_bytes`, which is only a lower bound. `GET /v1/retain/{id}`
now reports both, so a shadow run that sees a gap can name it.

---

## Live verification

The unit tests pin the logic; this pins the wiring, against the real daemon on
a throwaway bank (deleted afterwards, HTTP 204).

| step | request | job | session row |
|---|---|---|---|
| 1 | `offset_from 1000, byte_offset 2000` | `done`, 0 failed | `confirmed 0` — **did not confirm over the unverified 0..1000** |
| 2 | `offset_from 0, byte_offset 1000` | `done`, 0 failed | `confirmed 1000` — the guard opened |

Before this change step 1 would have set `confirmed_offset = 2000` and the gap
would have been unrecoverable. The residual `inflight 1000` after step 2 is
step 1's range, now reachable: a re-send settles it.

Migration `0008` applied to the live database on restart — `schema_version 8`,
`HEALTHY` — and the real gap survived it unchanged (`byte_offset 3459173 /
confirmed 0`). The job that produced it reports `offset_from 0, offset_to 0`,
which is the migration default and reads as **"unknown"**: it predates the
column, and a historical row must not be able to claim a range it never
recorded.

---

## Diverged from legacy

Nothing to diverge from. Legacy commits its cursor on the HTTP response
(`retain.py:262-263`) because its server ingests synchronously; it has one
cursor, no queue, and therefore no gap to guard. Every part of this file exists
because ours returns `202`.

---

## Known limits

**A failed chunk's bytes are still lost.** This PR fixes the *evidence*, not
the loss: a `done` job with `chunks_failed > 0` still leaves its range
unsettled, and nothing re-sends it. The cursor now reports that honestly
instead of erasing it. The re-send needs a **per-chunk** byte range in
`retain_jobs` so a re-POST can carry the failed chunk alone — C4b's stated
re-entry criterion, unchanged and still open.

**The guard is per-session, and one job deep.** Under the hook's protocol —
one in-flight job per session — that is the whole space. A future queue of
concurrent jobs on one session would need the ranges ordered, not just
compared.

**Historical rows read as "unknown".** Every `retain_jobs` row written before
migration `0008` has `0, 0`. That is deliberate: `0` as a *start* means
"begins at the beginning", which for a row that never recorded one would be a
confirmation nobody made.

---

## What this unblocks

`inflight_bytes` is now an upper-bounded measurement rather than a lower bound,
which is what **AC-3** and any Phase F claim built on it require. AC-3 itself is
still not met — it needs Phase D — and the loss above is still real; what
changed is that the number can no longer read zero over bytes nobody ingested.
