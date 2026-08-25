# MX-3 — measuring what memory substitutes for

**Status: design, not built.** The framework's Layer 3, which every earlier
document deferred with the same sentence: *it needs a judge, and a judge needs
its own evaluation.* This is the design for a judge that can be trusted, and
the reason each part of it exists is a specific way an earlier measurement in
this project went wrong.

## The question

Not *"how many tokens does MemGarden inject?"* — that is Layer 1, it is
measured, and the answer is a **cost**: ~680 tokens on a median turn, 16
memories, against a 1,024 cap.

Not *"how much extraction input do the caps save?"* — that is Layer 2, it is
measured, and it is real: **139,610 of 233,531 tokens, 59.8%**, across nine
sessions. But it saves *Ollama's* input, not the user's context.

The question nobody here has answered is the one that motivates the system:
**when the answer is already in the bank, how much work does not happen?**

## The unit is a task, not a query

AC-1 measured *retrieval*: given a question, are the right memories returned?
That is necessary and it is not this. A memory that is retrieved and ignored
saves nothing; a memory that is retrieved and lets the agent skip four file
reads and a bisect saves a great deal.

So the unit is a **task run to an answer**, and the measurement is the cost of
reaching it:

| | |
|---|---|
| **primary** | output tokens the agent spends before answering |
| secondary | tool calls, wall-clock, files read |
| **gate** | the answer must be judged **equivalent or better**, or the saving is not a saving |

The gate is the whole design. Spending zero tokens to answer wrongly is not a
99% saving, and a measurement that cannot say so measures nothing.

## Two arms, one difference

* **A — memory off.** `[hooks] mode = "shadow"`, which injects nothing. The
  agent still has the repository, the shell and the web.
* **B — memory on.** Exactly today's configuration.

Everything else is held: same model, same tools, same repository commit, same
task text, same limits. The one difference is the injected block.

**Arm A is not a strawman.** It is the agent this user had before this project
existed, and it can still succeed — by grepping, by reading design notes, by
re-running a benchmark. That is the point: the measurement is *how much of that
work the memory replaces*, not whether the agent is helpless without it.

## Where the tasks come from, and why not from me

The judge must not pick the topics, and neither must the author. There are 509
real user prompts across 433 transcripts on this machine, **158 of them
question-shaped**. That is the pool.

Selection is mechanical and stated in advance:

1. keep prompts that ask something rather than instruct — a fixed keyword set,
   published before sampling;
2. drop prompts that cannot stand alone, which is the lesson AC-1 paid for:
   *"이게 맞나?"* has no referent outside its conversation, and five of six
   shadow prompts were unjudgeable for exactly this reason;
3. drop prompts whose answer postdates the bank's newest memory, or arm B is
   being asked to recall the future;
4. random sample **30** of what survives, with the seed recorded.

Stratify the sample by whether the answer exists in the repository at all:

* **in-repo** — the answer is in code, docs or git history. Arm A can get
  there; the measurement is how much cheaper arm B is.
* **memory-only** — the answer exists only because someone said it: a
  decision, a retraction, a measurement that was taken and not written down.
  Arm A cannot get there at any price, and the honest report of that is a
  **failure to answer**, not an infinite saving.

The split matters more than the totals. A system that only helps on
memory-only tasks is a different product from one that also makes in-repo work
cheaper, and a single averaged number hides which one this is.

## The judge

Blind, panelled, and not the author — the AC-1 procedure, which exists because
the first AC-1 run was scored by the person who built the thing being scored:

* each task's two answers are frozen and relabelled `A`/`B` by a per-task hash;
* three judges per task, given different lenses (the asker; a strict
  item-by-item counter; an adversary told to argue the other side);
* majority of three, and a tie is a tie;
* the criteria are committed **before the first task is run**, and the commit
  timestamp is the proof.

The judges see the answers. They do not see the token counts — those are
mechanical and must not colour the quality verdict.

## What will go wrong, from what already has

Each of these is a mistake this project made in the last two weeks. They are
listed so the harness can be built against them rather than rediscovering them.

1. **The treatment may not apply.** AC-1's first run gave legacy half the token
   budget because the two APIs spell `maxTokens` differently and neither server
   rejects an unknown field. **Assert the arm before trusting the arm**: arm A
   must be verified to inject zero bytes, arm B a non-empty block, on every
   single run.
2. **The instrument moves.** The bank gains memories daily, so a re-run is a
   new measurement, not a check. Freeze: pin the repository commit, snapshot
   the database, record both hashes in the result.
3. **The metric can be gamed by the thing it measures.** AC-1's criteria put
   hits before noise, so a system returning more items scored better almost
   mechanically. Here the analogue is an agent that answers fast and wrong.
   The quality gate is what stops it, and it must be applied **before** the
   token counts are read.
4. **A plateau is worth more than a peak.** The semantic boost was shipped
   because every value in `0.05..=0.15` beat the baseline, not because 0.1 was
   the argmax. Report the distribution across 30 tasks, not the mean.
5. **The author's diagnosis is not evidence.** Three ranking fixes were
   measured against AC-1's losses on the strength of a plausible story and none
   of them moved anything. Numbers first.

## What this will not answer

* **Whether the memories are true.** Retrieval quality is AC-1's job; a
  confidently wrong memory that saves ten minutes scores well here and is a
  worse outcome than not having it. That gap is real and is not closed by this
  design.
* **Anything about other people's work.** One machine, one operator, one
  repository, one language mix.
* **The cost of being wrong.** A saving measured in tokens says nothing about
  the tail where memory sends the agent down a path the repository would not
  have.

## Cost

30 tasks × 2 arms = 60 agent runs, plus 90 judge runs. The judging half is the
same harness AC-1 already used, unchanged.

## The decisions that need making before anything is built

1. **Task count.** 30 is the smallest sample that can separate the two strata.
2. **Who runs the arms** — a subagent per task, or the same session with the
   hooks toggled. A subagent is cleaner and cannot be contaminated by this
   conversation's context.
3. **Whether a failed answer in arm A counts as an infinite saving or as a
   separate outcome.** This design says: a separate outcome, reported as a
   count, never folded into a token average.
