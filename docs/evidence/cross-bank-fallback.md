# Cross-bank fallback — sized before it was built

Recall is per project: the hook asks the bank of the directory the prompt was
typed in. MX-4's t22 showed the cost — *"which session opened the PR on
&lt;a project&gt;?"* was typed in one project about work done from another, and
the typing project's bank held nothing about it. This file is the measurement
behind `[recall] fallback_banks`.

## Method

* 121 real prompts typed since the abstain gate shipped (2026-10-16), each
  with the bank its session actually used.
* A scratch daemon on a fresh backup — its own data directory and port, so
  the live daemon's token, port and metrics are untouched.
* Each prompt recalled from its own bank **and** from every other bank (20 in
  all) at the shipped settings. Items mentioned at or after the prompt were
  dropped, so later memories of the same work cannot leak back into it.
* Up to five items from the prompt's own bank and five from the others,
  shuffled, source hidden, labelled useful / not by a local model; the 45
  positives that decide the design were then checked by hand against a strict
  rule — *does it supply a fact the answer needs?*

## Results

| | |
|---|---|
| prompts whose own bank returned nothing | **46 / 121 (38%)** |
| other-bank items clearing the shipped 0.83 floor, per prompt | **63** on average — a union would flood the injection |
| model-labelled precision, own bank vs other banks | 24% vs 16% |

A **fallback** — only when the own bank returned nothing, top items from the
others above a stricter floor — checked by hand:

| floor | prompts injected | items | strictly useful | prompts helped | stale or private |
|---|---|---|---|---|---|
| 0.85 | 43 / 46 | 201 | 11 (5%) | 7 | 3 |
| 0.87 | 29 / 46 | 86 | 8 (9%) | 5 | 2 |
| **0.88** | 15 / 46 | 36 | **8 (22%)** | **5** | 2 |
| 0.89 | 5 / 46 | 15 | 4 (27%) | 2 | 0 |

"Strictly useful" was checked only among the model's positives, so it is a
lower bound.

## What the useful items were

Almost all of them were **machine-level infrastructure** rather than project
knowledge: how to check the local inference server, a measured decision about
an inference runtime on that server, the delegation concurrency rule, the
terminal-orchestrator's commands. Facts about the machine get discussed in
whichever project happens to be open, so they end up scattered across banks.

## What went wrong in the sample

* **Stale infrastructure state** — a GPU-driver mismatch long since fixed, an
  old slot layout for the inference server. Cross-bank memories are older on
  average, because the current project's newer ones are exactly what was
  missing.
* **Private content crossing projects** — one item from a personal-notes bank
  carried the owner's employer and title into an unrelated project's session.
  In a session working on a public repository, that is a path to a commit.

## The design that follows

* **Opt-in, by bank**: `fallback_banks` is empty by default. A bank is listed
  only if its contents may reach any project's sessions; a personal-notes bank
  never is.
* **Only when the own bank returned nothing**, and nothing was withheld as
  already injected — if the session already holds what the bank had, there is
  nothing missing to look for.
* **Floor 0.88, at most 3 items** — the knee of the table above.
* **Marked**: each fallback line carries `[from <project>]`, so the reader
  knows it is another project's memory.

Expected effect at these settings: about 1 prompt in 25 gains a useful memory
it would not have had, at ~0.8 extra items per prompt that would otherwise get
none. Small, and the reason it is off by default.

## The question that started this is not answered by it

Checked end to end after implementation, on a scratch daemon with the real
data and five infrastructure banks listed: t22's question still gets
nothing. In the bank that holds the answer, the closest memories score
0.77–0.84 semantically — under the 0.88 fallback floor — and **0 on
keywords**, although the asked-about project's name appears in 37 of them.

The keyword miss is a separate defect, and a wider one: the FTS query
builder splits only on non-alphanumeric characters, and Hangul counts as
alphanumeric, so a Latin name with a Korean particle attached
(`<name>에`) becomes one prefix term that matches nothing stored as
`<name>`. The same query with a space before the particle finds 16 items;
`<server>서버` finds 5 where `<server> 서버` finds 10. It affects every bank,
not only fallback, and is left to its own change and measurement.
