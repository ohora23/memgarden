# CE-8 follow-up — Korean absolute dates, and the AX-2 re-baseline

Branch `fix/ce-8-korean-absolute-dates`. No migration, no schema change, no new
dependency, no new REST endpoint, no config knob.

**Why a separate note rather than an edit to `ce-8-temporal.md`.** That note is
the record of PR B6 and carries B6's own verification counts and latency runs;
folding a later PR's measurements into it would make neither PR's evidence
readable. Every other change in this phase has its own note. `ce-8-temporal.md`
keeps the decision it made and now points here, with its falsified rationale
struck in place rather than deleted.

## The defect

`temporal::query::fallback_date` accepted only ISO-extended tokens (`len >= 10`
and containing `-`). So `8월 2일` — the most natural way a Korean speaker
writes an absolute date, and the way this project's own queries write one —
produced **no constraint at all**: `extract_constraint` returned `None`, the
temporal arm never ran, and `scores.temporal` stayed `NEUTRAL` for every
candidate.

AX-2's gold query q17 (`8월 2일에 터진 사고`) is a live counterexample from our
own gold set. Before this change it scored nDCG@10 **0.149** with MRR 0.100;
after, **0.628** with MRR 1.000.

## What this adds

`temporal::query::korean_absolute_date`, in the fallback, below the relative
period rules:

| Form | Example | Result |
|---|---|---|
| `N월 N일` | `8월 2일`, `12월 31일`, `8월2일` | single day, year inferred |
| `YYYY년 N월 N일` | `2024년 3월 15일` | single day, year as written |

Both produce the same `start_ms .. start_ms + DAY_MS - 1` window the ISO
fallback already produces, so nothing downstream distinguishes them.

No regex crate and no date-parsing crate: this is two runs of ASCII digits read
off either side of a known character, with `jiff::civil::Date::new` doing the
validation. The workspace pins are frozen and `jiff` was already here.

## Diverged from legacy

**We are day-precise. Legacy is month-only, with the day and the year taken
from the reference date. This divergence is deliberate and we are the correct
one.** It is the same category as CE-7 declining legacy's phantom causal
`+1.0`: a ported behaviour that is not worth having.

`docs/parity-gaps.md` previously recorded that legacy resolves `8월 2일` →
`datetime(2026, 8, 2)`. **That claim was wrong**, and it was wrong in the way
that is hardest to catch: it is true exactly when the reference date happens to
be the 2nd of the month, which it was on the day the check was run. Re-run
against dateparser 1.4.1 with an explicit `RELATIVE_BASE`, which is what
`query_analyzer.py` supplies:

```
RELATIVE_BASE = 2026-08-03          (AX-2's pinned now)
  '8월 2일에 터진 사고'  -> [('8월', datetime(2026, 8, 3))]   <- NOT Aug 2
  '3월 15일'            -> [('3월', datetime(2026, 3, 3))]   <- NOT the 15th
  '2024년 3월 15일'      -> [('3월', datetime(2026, 3, 3))]   <- year ignored too
  '12월 31일'           -> [('12월', datetime(2026, 12, 3))]
```

Identical with and without `languages=['ko']`. dateparser matches only the
`N월` token; **the day and the year come from the reference date.** Legacy then
narrows that to a single day (`query_analyzer.py:283-287`).

Legacy does **not** filter these out on the way through, which was the obvious
escape hatch and it is closed: `_is_cjk_character` is `U+4E00–U+9FFF`, Han
only, so the Hangul `월` is not CJK and `is_embedded_cjk_dateparser_match`
returns `False` at its first check; `_date_match_score('8월')` then scores 100
on the digit.

So at AX-2's pinned `now`, **legacy gives q17 a single-day window of Aug 3 and
filters out exactly the Aug 2 facts the query asks for.** On this query legacy
is *worse* than the behaviour this PR replaces: we returned no constraint and
stayed neutral; legacy returns a confidently wrong one. Reproducing it would be
a regression wearing a parity label.

`we_do_not_take_the_day_or_the_year_from_the_reference_date` pins the
divergence as an assertion, at AX-2's `now`, so it cannot be "fixed" back into
legacy's shape by someone reading the parity list.

## Year inference for the bare `N월 N일` form

**The rule: the most recent occurrence that is not in the future relative to
`now`.** Inclusive of today.

A memory query asks about the past — there is nothing to recall from next
December. The tempting alternative, *always the current year*, is wrong in the
one case that decides between them: `12월 31일` asked on 5 January would
resolve eleven months into the future, into a region of the bank that is empty
by construction, and the arm would return nothing while looking like it worked.
That failure is silent, which is the worst kind here.

Tested in both directions, because a rule exercised only inside one calendar
year is indistinguishable from "always the current year":
`a_bare_month_day_resolves_to_the_most_recent_past_occurrence` asserts
`12월 31일` → 2025-12-31 at a 2026-08-02 `now` **and** → 2026-12-31 at a
2027-01-05 `now`, plus the today/tomorrow boundary either side of the
inclusive comparison.

An **explicit** year is honoured as written, future or not — the query said it.

Implementation is a walk over five candidate years, not a search. Only
`2월 29일` ever needs more than one step back, and never more than four; the
`ponytail:` comment on `most_recent_occurrence` names that ceiling and says
what to do if a query ever needs older (write the year).

## Invalid dates are rejected, not clamped

`13월 45일`, `2월 30일`, `0월 5일`, `8월 0일`, `8월 32일` all yield **no
constraint**. `Date::new` rejects them and the scan moves on.

This is the whole point of the change restated: a clamped date is a
confidently wrong single-day window that filters out everything the query asked
for. That is the failure mode being fixed, so reintroducing it at the
validation step would be self-defeating. `an_impossible_date_is_no_constraint_rather_than_a_clamped_one`
pins it.

## Ordering: the relative rules keep winning

The scan lives in the fallback, which runs *after* the `RULES` table and after
the `Unconstrainable` short-circuit, so nothing that already matched changes:

* `지난주 8월 2일 회의` → last week's range, not Aug 2.
* `매주 8월 2일` → `Unconstrainable`, as before.
* `2026-07-15 아니면 8월 2일` → the ISO token, as before. ISO runs first so the
  pre-existing fallback is byte-identical when it matches at all.

`relative_expressions_still_beat_an_absolute_date_in_the_same_query` pins all
three. No existing temporal test changed.

## Out of scope, with the criterion

Both are declared rather than built, and both have a test asserting they parse
to nothing so the omission is visible:

* **Bare `N월`** (`12월에 있었던 일`). A month is a *range*, not a day — a
  different shape that would need its own `Period`-style resolution and its own
  year-inference argument (the most recent *past month* is not the same rule).
  Build it when a query in the banks asks for a month without a day.
* **Numeric `8/2`.** Ambiguous between two field orderings, and
  indistinguishable from a fraction, a ratio or a path fragment in exactly the
  agent-transcript text this system stores. Build it if a bank ever
  demonstrates the form in a query, with a decision on the ordering recorded.

Legacy resolves neither correctly either, so neither is a parity gap.

Still absent for the same reason as before: English month names (`July 2026`),
full-width digits, and NFKC normalization generally.

## Verification

`cargo fmt --all -- --check` clean. `cargo clippy --workspace --all-targets
-- -D warnings` clean. `cargo test --workspace --no-fail-fast`: **429 passed,
0 failed, 15 ignored** — up from 423 on `master`, which is exactly the six
tests below.

Six new tests, all in `temporal::query::tests`:
`korean_absolute_dates_are_single_day_windows` (both forms, whitespace
tolerance either side of `월`, the full-millisecond window shape, and q17 at
AX-2's `now`); `a_bare_month_day_resolves_to_the_most_recent_past_occurrence`
(both sides of the year boundary, the inclusive-today boundary, and the leap
day); `an_impossible_date_is_no_constraint_rather_than_a_clamped_one`;
`bare_month_and_numeric_slash_forms_are_out_of_scope`;
`relative_expressions_still_beat_an_absolute_date_in_the_same_query`;
`we_do_not_take_the_day_or_the_year_from_the_reference_date`.

## The AX-2 re-baseline

Shipped in this PR, not deferred, because a stale baseline is worse than none —
every future recall delta is measured against it.

All three AX-2 arms were re-run at the recorded configuration exactly: corpus
`baee3f40…4bda868` (2718 nodes, `sha256sum -c` verified), `now =
1785715200000`, `limit = 20`, `max_tokens = 8192`, budget `mid`, all three
`recallTypes`. The import reproduced AX-2's own structure counts exactly (2718
nodes, 2129 entity rows from 1471 facts, 54 012 temporal links). Appended to
`gold/results.jsonl` as lines 5-7, stamped `33d49519`.

**Only q17 moved.** Every other query in every arm reproduces the previously
recorded run digit-for-digit, including the retrieved uuid lists — which is the
isolation claim this change needs, measured rather than asserted.

### The shipped configuration (reranker off), 13 queries, conclusion excluded

| metric | before | after | Δ |
|---|---|---|---|
| recall@1 | 0.0222 | 0.0414 | +0.019 |
| recall@5 | 0.1969 | 0.2546 | +0.058 |
| recall@10 | 0.3449 | 0.4026 | +0.058 |
| MRR | 0.4821 | 0.5513 | +0.069 |
| nDCG@10 | 0.3021 | 0.3390 | +0.037 |

### Per stratum, reranker off

Only the temporal stratum moves; the other three are unchanged to four decimal
places, which is the same isolation claim from the other direction.

| stratum | q | nDCG@10 before | after | Δ | MRR before | after | Δ |
|---|---|---|---|---|---|---|---|
| **temporal** | 2 | 0.0745 | **0.3142** | **+0.240** | 0.0500 | **0.5000** | **+0.450** |
| identifier | 4 | 0.4163 | 0.4163 | — | 0.6875 | 0.6875 | — |
| memcompare | 5 | 0.2032 | 0.2032 | — | 0.3833 | 0.3833 | — |
| graph | 2 | 0.5489 | 0.5489 | — | 0.7500 | 0.7500 | — |
| conclusion | 1 | excluded — structurally unmeasurable (AX-2) | | | | | |

**Temporal improved, and by a lot.** q17 alone: recall@10 0.250 → **1.000**
(all four relevant nodes now inside the measurement window), MRR 0.100 →
**1.000**, nDCG@10 0.149 → **0.628**.

This was not the guaranteed outcome and the PR was written to report the other
one honestly: a correct constraint *narrows* the candidate set, and narrowing
can legitimately lower a score when the labels sit outside the window. Here the
labels sit inside it, so the narrowing is pure gain.

### The identifier guardrail

Unmoved, at 0.4163 nDCG@10 / 0.6875 MRR. Nothing in this change touches the
lexical arm, and the measurement says so rather than assuming it.

### q15 is not a bug, and should not be "fixed"

`지난주` at the pinned `now` resolves to 2026-07-27..08-02, which contains 2697
of the corpus's 2718 facts. It still scores 0.000 on everything.

**That is a property of a four-day corpus, not a defect in the window logic.**
The window is correct; there is simply nothing outside it to exclude. The fix
is a corpus spanning more calendar time (AX-2 already lists the re-snapshot as
a follow-up), not a change to `Period::Week`. Recorded explicitly so the next
reader does not go looking for a bug in `resolve`.

The consequence for the stratum is that it is now **half** valid rather than
wholly invalid: q17 exercises the temporal arm end to end; q15 still does not.

### The reranker's temporal gain inverted, and CE-11's figures move with it

CE-11 recorded the cross-encoder at `top_k = 10` gaining **+0.251** nDCG@10 on
the temporal stratum, with an explicit warning that the number was meaningless
because neither query exercised the arm. Now that q17 does:

| temporal stratum (2q), nDCG@10 | off | `top_k = 10` | `top_k = 20` |
|---|---|---|---|
| CE-11 as recorded | 0.0745 | 0.3254 | 0.1539 |
| re-baselined | **0.3142** | **0.1220** | **0.3470** |

At `top_k = 10` the reranker now **regresses** the stratum by 0.192 instead of
improving it by 0.251. The cross-encoder scores query-document text pairs and
knows nothing about the date window, so on a query whose answer is selected by
a date it demotes three of the four relevant nodes below rank 10.

Two honest caveats, in the direction that weakens the finding: the stratum is
two queries wide and q17 has only four relevant nodes, so this is thin; and
CE-11's decision does not turn on it, because that decision was settled on
latency unconditionally and the reranker remains off by default. `ce-11-reranker.md`
carries the updated tables and the corrected caveat.

## Follow-ups

* **The temporal stratum is still two queries wide**, and the gold set should
  grow a Korean absolute-date query whose answer lies *outside* the window —
  the case that would catch an over-narrow constraint, which nothing currently
  does.
* **A re-snapshotted corpus makes q15 measurable**, and only that.
* **Bare `N월` and `8/2`**, on the criteria above.
