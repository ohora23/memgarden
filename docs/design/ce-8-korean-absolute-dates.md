# CE-8 follow-up — Korean absolute dates, and the AX-2 re-baseline

Branch `fix/ce-8-korean-absolute-dates`. No migration, no schema change, no new
dependency, no new REST endpoint, no config knob.

## Changelog

Two rounds of correction have landed on this note. Each is recorded in place,
next to the claim it changes, per this repo's convention; this table exists so a
reader can tell at a glance which figures are current without reconstructing the
order from the prose.

| Round | What changed | Where |
|---|---|---|
| **1** — 2026-08-03, PR #19 (merged) | The `N월 N일` fix itself, the correction of `parity-gaps.md`'s legacy-dateparser claim, and the AX-2 re-baseline of all three arms. | whole note |
| **2** — 2026-08-03, `fix/q17-labels` | q17's 15 ungraded pool entries graded and **ratified by the corpus owner**; `\|R\|` 4 → 5. All three arms re-run at the same pinned configuration; **every figure in this note refreshed**. One wrong mechanism claim — that `recall@10 = 1.000` was an artifact of a frozen `\|R\|` — struck and corrected. | *The defect*, the three re-baseline tables, *q17's pool*, *Follow-ups* |

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
after, **0.638** with MRR 1.000. (Round 1 recorded 0.628 against a pool that
was three-quarters ungraded; the ratified labels moved it to 0.6378 — see
*q17's pool* below.)

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

Legacy resolves neither correctly either, so neither is a parity gap. Both fail
**safe**: no constraint, `scores.temporal` stays `NEUTRAL`.

**The one declined shape that fails *unsafe*, named because omitting it would
invert this section's usefulness:**

* **`이전` / `까지` on an absolute date** (`8월 2일 이전에 있었던 일`). The
  fallback path never consults the before/since markers — `marker_before` is
  called only inside the `find_period` branch — so this yields a single-day
  **Aug 2** window where the period form (`지난주 이전`) correctly yields
  `Unconstrainable`.

  The cost is not hypothetical. A dated node outside the window takes
  `temporal_proximity = 0.0` → **0.9×** in `combined`, while a dateless node
  keeps `NEUTRAL` → 1.0×. A wrong window therefore *penalises correctly-dated
  candidates by 10% relative to undated ones* and contributes nothing from the
  arm — precisely the "a confidently wrong window is worse than none" failure
  this whole function exists to prevent.

  **Pre-existing and not a regression**: ISO dates behave identically on
  `master` (`2026-07-15 이전에` has always produced a single-day window). What
  this PR changes is that the natural Korean phrasing now reaches it, and
  `이전`/`까지` on an absolute date is common. Declared rather than fixed here
  to keep the change to one behaviour, pinned by
  `before_markers_do_not_reach_the_fallback_path`, and marked with a
  `ponytail:` comment on `korean_absolute_date` naming the upgrade path: hoist
  the `marker_before` check above the fallback so both paths share it.

Still absent for the same reason as before: English month names (`July 2026`),
full-width digits, and NFKC normalization generally. Two-digit years
(`99년`) are deliberately *not* read as years — see `year_prefix`.

## Verification

`cargo fmt --all -- --check` clean. `cargo clippy --workspace --all-targets
-- -D warnings` clean. `cargo test --workspace --no-fail-fast`: **431 passed,
0 failed, 15 ignored** — up from 423 on `master`, which is exactly the eight
tests below.

Eight new tests, all in `temporal::query::tests`:
`korean_absolute_dates_are_single_day_windows` (both forms, whitespace
tolerance either side of `월`, the full-millisecond window shape, and q17 at
AX-2's `now`); `a_bare_month_day_resolves_to_the_most_recent_past_occurrence`
(both sides of the year boundary, the inclusive-today boundary, and the leap
day); `an_impossible_date_is_no_constraint_rather_than_a_clamped_one`;
`a_two_digit_year_is_inferred_not_read_as_year_ninety_nine`;
`before_markers_do_not_reach_the_fallback_path`;
`bare_month_and_numeric_slash_forms_are_out_of_scope`;
`relative_expressions_still_beat_an_absolute_date_in_the_same_query`;
`we_do_not_take_the_day_or_the_year_from_the_reference_date`.

The re-baselined figures below were re-verified after the `gold/queries.jsonl`
note correction: the harness reproduces every number in this note
digit-for-digit. Round 2 re-ran them against the ratified labels; the
fourteen-query aggregate moved 0.3229 → **0.3236** nDCG@10, entirely from q17.

## The AX-2 re-baseline

Shipped in this PR, not deferred, because a stale baseline is worse than none —
every future recall delta is measured against it.

All three AX-2 arms were re-run at the recorded configuration exactly: corpus
`baee3f40…4bda868` (2718 nodes, `sha256sum -c` verified), `now =
1785715200000`, `limit = 20`, `max_tokens = 8192`, budget `mid`, all three
`recallTypes`. The import reproduced AX-2's own structure counts exactly (2718
nodes, 2129 entity rows from 1471 facts, 54 012 temporal links).

Round 1 appended lines 5-7, stamped `33d49519`. **Round 2 re-ran all three arms
against the ratified labels** — same corpus sha, same pinned `now`, same
`limit`/`max_tokens`/`budget`/`recallTypes`, so the three stay mutually
comparable — and appended lines 8-10, stamped `73ba3b2c`. The tables below carry
the round-2 figures.

**Only q17 moved, in both rounds.** Every other query in every arm reproduces
the previously recorded run digit-for-digit, including the retrieved uuid lists
— which is the isolation claim this change needs, measured rather than
asserted. Round 2 changed no retrieval at all: `gold/results.pool.json` is
byte-identical to round 1's, because only the labels moved.

### The shipped configuration (reranker off), 13 queries, conclusion excluded

`before` is the pre-fix baseline (lines 1-2, `d6165560`); `after` is round 2
(line 8, `73ba3b2c`), with round 1's figure alongside where the ratification
moved it.

| metric | before | after (r1) | **after (r2, ratified)** | Δ vs before |
|---|---|---|---|---|
| recall@1 | 0.0222 | 0.0414 | **0.0375** | +0.015 |
| recall@5 | 0.1969 | 0.2546 | **0.2431** | +0.046 |
| recall@10 | 0.3449 | 0.4026 | **0.4026** | +0.058 |
| MRR | 0.4821 | 0.5513 | **0.5513** | +0.069 |
| nDCG@10 | 0.3021 | 0.3390 | **0.3398** | +0.038 |

recall@1 and recall@5 came *down* between r1 and r2 without any ranking change:
q17's `|R|` went 4 → 5, and both metrics are `1/|R|`-bounded floors. See the
mechanism correction two sections down.

### Per stratum, reranker off

Only the temporal stratum moves; the other three are unchanged to four decimal
places, which is the same isolation claim from the other direction.

| stratum | q | nDCG@10 before | after | Δ | MRR before | after | Δ |
|---|---|---|---|---|---|---|---|
| **temporal** | 2 | 0.0745 | **0.3189** | **+0.244** | 0.0500 | **0.5000** | **+0.450** |
| identifier | 4 | 0.4163 | 0.4163 | — | 0.6875 | 0.6875 | — |
| memcompare | 5 | 0.2032 | 0.2032 | — | 0.3833 | 0.3833 | — |
| graph | 2 | 0.5489 | 0.5489 | — | 0.7500 | 0.7500 | — |
| conclusion | 1 | excluded — structurally unmeasurable (AX-2) | | | | | |

**Temporal improved, and by a lot.** q17 alone: recall@10 0.250 → **1.000**,
MRR 0.100 → **1.000**, nDCG@10 0.149 → **0.638**. The recall@10 figure is
**robust rather than an artifact of the label set** — see below, where the
opposite claim is struck.

The mechanism: `Some(window)` **adds** a third retrieval arm, so the merged
candidate set strictly grows — the temporal arm injects all five relevant nodes
into RRF's top 10, where the lexical and semantic arms had reached one. It is
candidate injection, not filtering; nothing is narrowed away.

### q17's pool was three-quarters ungraded — now fully graded and ratified

**Ratified 2026-08-03 by the corpus owner.** The temporal arm firing had turned
q17's labelling pool over almost completely; all 15 ungraded entries have since
been reviewed and graded. **q17's whole 20-entry pool is graded**, `|R|` is 5,
and `gold/queries.jsonl` carries `labels_status: ratified-2026-08-03` on q17
alone.

| | pre-fix | post-fix (r1) | **ratified (r2)** |
|---|---|---|---|
| top-20 entries carried over | — | 3 of 20 (**17 new**) | 3 of 20 |
| top-20 entries with a grade | 20 of 20 | **5 of 20** | **20 of 20** |
| scored top-10 entries with a grade | 10 of 10 | **5 of 10** | **10 of 10** |

The grading found **one** new relevant node — `9c3e3f69` at rank 9, the FTS
implicit-AND defect of 2026-08-02, graded **1** because it was caught in review
before shipping — and fourteen zeroes: ten pieces of agent-lifecycle
boilerplate, three Phase B work logs, and a CE-8 progress report whose alarming
p99 was later resolved as a harness artifact rather than a regression.

Two consequences were recorded here. **The first stands. The second was wrong,
and the mechanism it named was wrong** — struck rather than edited away,
because it was load-bearing for a caveat that propagated into
`ax-2-recall-quality.md`, `ce-11-reranker.md` and q17's own gold-set note:

1. **nDCG@10 was a lower bound, not a point estimate** — correct, and it moved
   **up**: `0.6285 → 0.6378` on the off arm (`+0.0093`). Up in the other two
   arms as well: `top_k = 10` `0.1083 → 0.1709`, `top_k = 20`
   `0.6939 → 0.7044`.

2. ~~"**`recall@10 = 1.000` is an artifact of `|R|` frozen at 4** by the
   *pre-fix* pool. Grade one of the five new documents as relevant and `|R|`
   becomes 5, and recall@10 falls below 1.000 **with no code change at all.**
   So '0.250 → 1.000' is not the clean sweep it reads as, and should not be
   quoted as one."~~

   **That mechanism cannot work, and the arithmetic says so without needing the
   labels.** All four already-relevant nodes sit at ranks 1, 4, 5 and 6 —
   *inside* the top 10 — and all five ungraded entries sat at ranks 3, 7, 8, 9
   and 10, also inside it. recall@10 is `|relevant ∩ top-10| / |R|`, so grading
   any top-10 entry relevant increments the numerator **and** the denominator
   together: 4/4 → 5/5. It stays exactly 1.000. The only entries that could
   have lowered it were the ungraded ones at **ranks 11-20**, where relevant
   would have raised `|R|` without raising the numerator.

   Those ten are graded now, and **all ten are 0**. So the corrected conclusion
   inverts the struck one, and it is a *stronger* claim than this note
   originally made: **`recall@10 = 1.000` for q17 is robust, not soft.** It
   survived the only test that could have broken it — ten chances at ranks
   11-20 for a relevant node to inflate `|R|`, taken and found empty. The one
   grade that was added landed at rank 9 and moved 4/4 to 5/5, exactly as the
   corrected mechanism predicts.

What the struck item overlooked is where the `|R|` denominator *does* bite: the
**shallower** recalls. With `|R|` 4 → 5 and the new relevant node at rank 9,
recall@1 goes 0.250 → **0.200** and recall@5 goes 0.750 → **0.600**. Neither is
a ranking regression — nothing was retrieved differently — and both are
`1/|R|`-bounded floors rather than precision, per AX-2's *Reading these
numbers*. They fall purely because the denominator grew.

`ce-11-reranker.md`'s "a labelling error largely cancels between arms" did not
cover this pool — that argument is about symmetric error against a shared label
set, and this was labelling *absence* in a pool that had shifted under one arm.
It is moot now: the pool is fully graded, and the direction turned out to be up
on nDCG@10 in all three arms.

The stratum-level conclusion was never at risk — a stratum that scored 0.0745
with the arm dead does not reach 0.3189 by mislabelling — and q17's per-query
figures are no longer provisional at all.

### The identifier guardrail

Unmoved, at 0.4163 nDCG@10 / 0.6875 MRR. Nothing in this change touches the
lexical arm, and the measurement says so rather than assuming it.

### q15 is not a bug, and should not be "fixed"

`지난주` at the pinned `now` resolves to 2026-07-27..08-02, which contains 2697
of the corpus's 2718 facts. **On the shipped configuration (reranker off) it
still scores 0.000 on everything**, and it does at `top_k = 20` too — but not
at `top_k = 10`, where it is 0.1356 nDCG@10 / 0.1111 MRR (unchanged by the
ratification — q15's labels did not move). That non-zero value is load-bearing
in `ce-11-reranker.md`'s `0.1533` temporal-stratum arithmetic, so the blanket
phrasing would misread that table.

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
| re-baselined (r1) | 0.3142 | 0.1220 | 0.3470 |
| **ratified (r2)** | **0.3189** | **0.1533** | **0.3522** |

At `top_k = 10` the reranker now **regresses** the stratum by 0.166 instead of
improving it by 0.251. The cross-encoder scores query-document text pairs and
knows nothing about the date window, so on a query whose answer is selected by
a date it demotes three of the four relevant nodes below rank 10.

One honest caveat, in the direction that weakens the finding: the stratum is
two queries wide and q17 has only five relevant nodes, so this is thin. And
CE-11's decision does not turn on it, because that decision was settled on
latency unconditionally and the reranker remains off by default.
`ce-11-reranker.md` carries the updated tables and the corrected caveat. The
*other* caveat this section used to carry — that q17's labels were mostly
absent — is retired: the pool is ratified.

## Follow-ups

* ~~**Grade q17's new pool — highest priority, and a precondition for quoting
  its per-query numbers.**~~ **DONE, ratified 2026-08-03** (`fix/q17-labels`).
  All 20 pooled documents are graded, `|R|` is 5, and the figures above are the
  re-run. The claim attached to this item — that `recall@10 = 1.000` was an
  artifact of a stale `|R|` — was itself wrong; see the corrected mechanism in
  *q17's pool* above.
* **`temporal_proximity` scores an exact-day hit at 0.0 — a latent defect,
  recorded not fixed.** The kernel is triangular around the window
  **midpoint**, so for a single-day window the midpoint is noon and a fact
  stamped `00:00` on exactly the requested day scores **0.0** — identical to an
  out-of-window fact, and *below* the 0.5 a dateless fact receives. Memory
  facts are overwhelmingly date-only, so this is the common case, and this PR
  makes day-precise windows common.

  **q17's win therefore comes entirely from the temporal arm's candidate
  injection into RRF** (`recall/mod.rs:248-255`), not from the proximity score
  — which is worth knowing before anyone attributes the gain to the boost.

  **It is not a port infidelity.** Legacy computes `total_days = (end -
  start).total_seconds() / 86400`, a float, so a single-day window is
  `0.99999…` there too and legacy takes the same kernel branch rather than its
  `else 1.0` shortcut. Ported faithfully; the quirk is legacy's. Fixing it
  would be a CE-7-style deliberate divergence — a faithful port that produces
  backwards ranking — and would need its own re-baseline.
* **The temporal stratum is still two queries wide**, and the gold set should
  grow a Korean absolute-date query whose answer lies *outside* the window —
  the case that would catch an over-narrow constraint, which nothing currently
  does.
* **A re-snapshotted corpus makes q15 measurable**, and only that.
* **Bare `N월` and `8/2`**, on the criteria above. **`이전`/`까지` on an
  absolute date** is the one that fails unsafe and should be ranked above
  them.
