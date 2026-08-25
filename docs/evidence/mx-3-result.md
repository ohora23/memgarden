# MX-3 — the result, which is not the one the design expected

Run 2026-08-25 against the rules committed in
[`mx-3-criteria.md`](mx-3-criteria.md) before any task was drawn.

**Headline: the memory arm lost.** 11 tasks to 7, on a blind panel, and it
spent 5% more tokens doing it.

That is a real result and it is a narrow one, because the sample turned out
not to contain the thing the measurement was built to find. Both halves of
that sentence matter and the second is easier to skip.

## What was measured

| | arm A — no memory | arm B — memory | |
|---|---|---|---|
| tokens (harness, exact) | 1,577,681 | 1,656,506 | **+78,825 (+5.0%)** |
| tool calls (harness) | 386 | 367 | −19 (−4.9%) |
| wall clock | 410 s | 308 s | **−102 s (−25%)** |
| answered | 27/27 | 27/27 | — |

Blind panel, three judges per task, majority of three:

| | |
|---|---|
| **no-memory better** | **11** |
| equivalent | 9 |
| **memory better** | **7** |
| factual-error flags (judge votes) | memory **32**, no-memory **24** |

`worse ≤ better` — the gate the criteria set — **does not hold**. By the rule
committed in advance, no token saving is reportable from this run, and there
was none to report: the memory arm cost more.

## The arm assertion passed

27 of 27 arm-B runs confirmed a non-empty injection, and arm A received
nothing. This is checked because AC-1's first comparison silently gave one
side half the token budget, and a measurement that cannot prove its treatment
applied is not a measurement.

Self-reported tool calls came to 85% of the harness count in arm A and 87% in
arm B. The undercount is systematic but even, so relative comparison survives;
absolute self-reports do not.

## Why the sample cannot answer the question the design asked

Stratification, done before the arms ran by a labeller with the repository and
**not** the bank:

| | |
|---|---|
| **in-repo** | **24** |
| memory-only | **1** |
| neither | 1 |

**Twenty-four of twenty-seven answers were already on disk** — the labeller
points at files and commits: `findings.md §10`, session notes, commit
`ad58e61`, troubleshooting notes in the Obsidian vault.

So this run measured *"when the answer is written down anyway, does injecting
memories help?"* — and the answer is no, slightly worse, faster. It did not
measure *"when the answer exists only because someone said it, does the bank
recover it?"*, because **the sample contains one such task.** n=1 says nothing.

That is not a defect of the system under test. It is a fact about this
machine's owner: **the knowledge is already in the vault, the docs and the
commit messages.** A memory system's value here is speed, not access — and the
25% wall-clock reduction is the shape of that.

## The clearest loss, because it is the informative one

**t02** — three judges, unanimous for the no-memory arm, on the same grounds.
The memory arm diagnosed a retain failure as an exhausted Ollama context
window. The judges opened the journal and found the failing chunk was **139
tokens with `done_reason=stop`** — nowhere near the window. The no-memory arm
found the actual shape: the model wrote an escaped quote inside
`occurred_start`, so the rest of the object became one unterminated string.

The injected memories for that task were about kernel crashes and GPU faults.
It is one task and the mechanism is not established, but the possibility it
raises is the one this design named as out of scope and could not avoid
meeting: **a memory that is retrieved, relevant-looking and wrong costs more
than no memory at all.** The 32-to-24 error flags point the same way without
proving it.

## What the harness got wrong

**Agents modified the thing being measured.** Task t02 asks the assistant to
*fix* something, and both arms' agents did:

* both edited `crates/memgardend/src/extract/mod.rs`;
* **arm B's agent rebuilt the binary, ran `cargo install`, and restarted
  `memgardend.service`** — the user's live memory system, mid-measurement.

Both trees were restored to the frozen commit and the daemon was rebuilt from
committed code and restarted. Both patches were kept, because the defect they
were chasing is real — see below.

The design said "pin the repository commit" and did; it did not say "and stop
the agents writing to it". Arm A ran for 410 s with one agent's edit visible
to whichever concurrent agents read that file, which is a contamination inside
arm A that cannot be undone retroactively. Any re-run needs the arms sandboxed.

**Per-task token attribution was not available.** The harness reports tokens
per workflow, not per agent, so arm totals are exact and per-task numbers are
not. Tool calls fill the gap only as far as self-reports allow.

## What the run produced that was worth more than the measurement

Chasing t02, both arms independently found that **retain jobs were losing
chunks and reporting success**: of the last twelve jobs, 16 of 95 chunks
failed and four jobs finished `done` having lost some. Verified directly, then
fixed — `JobStatus::Partial`, schema v9, and an escalating retry temperature,
after measuring that four retries at a fixed temperature produce two distinct
outputs and four at escalating temperatures produce four.

A measurement that finds a real defect while failing to measure what it aimed
at is still worth the afternoon. It is not, however, evidence about token
savings.

## What would answer the original question

A sample built from **memory-only** questions rather than from whatever the
transcripts happened to contain — decisions, retractions, numbers measured and
never written down. Those exist in this bank; the mechanical draw simply did
not find them, because the questions people type are mostly about things they
also wrote down.

Until that runs, the honest summary of what MemGarden saves is:

* **measured, Layer 2** — 139,610 of 233,531 extraction-input tokens, 59.8%,
  across nine sessions. Real, and it is Ollama's input, not the user's context.
* **measured, Layer 1** — a median turn injects ~680 tokens, 16 memories,
  against a 1,024 cap. That is a cost.
* **measured here, Layer 3, on a sample that cannot generalise** — 5% more
  tokens, 25% less wall clock, and a quality verdict against.
* **not measured** — what the bank recovers that nothing else holds.
