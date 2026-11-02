# FTS query terms split where Hangul meets Latin — a correctness fix, measured

Found while checking the cross-bank fallback end to end
([`cross-bank-fallback.md`](cross-bank-fallback.md)): a question naming a
project with a Korean particle attached scored **0 on keywords** in a bank
where that name appears 37 times.

## The defect

`fts_query_string` split the prompt only on non-alphanumeric characters.
Hangul is alphanumeric, so `<name>에` and `<server>서버` each became one
prefix term, `"<name>에"*`, which matches nothing stored as `<name>`. The same
prompt with a space before the particle found 16 items; `<server>서버` found 5
where `<server> 서버` found 10.

It is common: **76 of 121** real prompts typed since the abstain gate shipped
contain such a glued token.

## Candidates, measured

On those 121 prompts, each recalled from its own bank on a scratch daemon
built with a measurement-only switch (never committed):

| | prompts with 0 keyword hits | keyword hits, median (cap 100) |
|---|---|---|
| shipped | 11 | 24 |
| split at the script boundary | 9 | 100 |
| + strip a list of Korean particles | 6 | 100 |

The split alone left one-character fragments: `B할` became `"B"*`, which
matches every token starting with *b* and injected an unrelated memory about
"arm B". The shipped fix drops terms under two characters in every script.

**What reached the injection** (shipped settings, abstain gate on): the same
total — the budget binds — with 51 of 121 prompts injecting a different set,
115 items in and 107 out. Labelled blind by a local model, shipped vs fix on
those prompts: precision 22.7% → 24.0%, net +5 useful items, per prompt 11
better / 8 worse / 32 tied. A strict hand check of 40 swapped items: 2 of 20
useful on the fix's side, 1 of 20 on the shipped side.

That is noise. The abstain gate withholds keyword-only items, so on the
hook's path the keyword arm only reorders semantic candidates.

## Why ship it anyway

* It makes the keyword arm do what its documentation says, at the cost of a
  few lines and no language list.
* The paths without the gate — consolidation, reflection, mental models, and
  recall while the embedder is loading or off — depend on the keyword arm
  more. Not measured: the graded query set used for `recall_bench` is not on
  this machine any more, and it was English.
* Stripping particles was measured and **not** shipped: no gain over the
  split, and a list of particles to maintain.

Claimed as: a correctness fix. Not claimed: better injections.
