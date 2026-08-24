# MX-3 — selection and judging rules, fixed before any task was drawn

Written before the sample was taken and before either arm was run. The commit
timestamp is what makes that checkable, which is the only reason this is a
separate file from the result.

## How the 30 tasks are drawn

From the 509 real user prompts in this machine's Claude Code transcripts, in
this order, and nothing about the content is decided after seeing it:

1. **question-shaped** — the prompt contains one of:
   `왜 · 어떻게 · 뭐 · 무엇 · 어디 · 언제 · 확인 · 원인 · why · how · what · where · which`
2. **length 20..600 characters** — shorter is usually an instruction, longer is
   usually a paste;
3. **stands alone** — dropped if it contains a demonstrative with no referent
   (`이거 · 그거 · 저거 · 이게 · 그게 · this one · that one`) or opens with a
   conjunction (`그리고 · 근데 · 그럼 · 아니 · and · but · so`). AC-1 paid for
   this rule: five of six shadow prompts were unjudgeable because a live
   prompt refers to the conversation around it;
4. **not about the future of the bank** — dropped if the prompt's transcript
   is newer than the frozen database's newest memory;
5. **deduplicated** by normalised text.

### Amendment, 2026-08-24 — rule 3 was not strong enough

**Recorded rather than quietly applied, and the order matters: this was found
after the first draw and before either arm was run, so no answer had been
seen when it was made.**

The keyword form of rule 3 caught bare demonstratives and dropped none of the
*continuations*. The first 30-task draw contained `GUI에서 HYB 다시 띄워서
확인해볼게`, `00이랑 loop-upgrade도 마저 확인해줘`, `재시작하고 서브에이전트
GPU로 잘 도는지 확인해줘` — prompts whose subject lives entirely in the turn
before them. That is the AC-1 failure reproduced exactly: a live prompt refers
to the conversation around it, and five of six shadow prompts were unjudgeable
for it.

Rule 3 is therefore replaced by a mechanical test rather than a longer keyword
list, because a longer keyword list is the author guessing again:

> **3′.** Each candidate is shown to three independent classifiers, given the
> prompt and the machine but **not** the conversation, and asked whether a
> competent engineer could tell what is being asked. Majority of three. The
> classifiers are told to judge the prompt, not the difficulty of the answer,
> and not to penalise terseness or Korean.

**This amendment cannot favour the memory arm.** Context-dependent prompts are
where a memory system most plausibly looks good — it can supply the missing
referent from the bank while the repository cannot. Removing them takes away
an advantage rather than granting one.

The original `seed = 3` draw is kept at `tasks-seed3-original.json` for audit.
The re-draw uses **`seed = 5`** over the filtered pool, so the two samples
cannot be confused.

---

Then **random sample of 30, `seed = 5`** over the pool surviving rules 1, 2,
3′, 4 and 5, recorded in the result.

## The two arms

| | |
|---|---|
| **A** | `[hooks] mode = "shadow"` — injects nothing |
| **B** | today's shipped configuration |

Held equal: model, tools, repository commit, database snapshot, task text,
and the instruction given to the agent.

**Every run asserts its own arm.** Arm A must be verified to have injected
zero bytes and arm B a non-empty block, per run, recorded beside the result. A
run whose arm cannot be verified is discarded, not interpreted. This exists
because AC-1's first comparison silently gave one side half the token budget.

## What is counted

* **primary — output tokens** the agent spent before its final answer;
* secondary — tool calls, wall-clock seconds;
* **the answer text**, frozen for judging.

Token counts are read **after** the quality verdict is in, never before.

## Judging

Blind, three judges per task, majority of three. Per task the two answers are
relabelled `A`/`B` by a hash of the task id and a salt; judges are told nothing
about which system is which and are instructed not to speculate.

Each judge returns one of:

| verdict | meaning |
|---|---|
| **equivalent** | both answer the question; no material difference in what a reader learns |
| **B better** | one answers materially more of the question, or is materially more correct |
| **A better** | the same, the other way |
| **neither answered** | both failed to answer; the task is reported and excluded from the token comparison |

**A tie is `equivalent`.** Ambiguity is not resolved in either direction.

Judges do not see token counts, timings, or tool-call counts.

## How a saving is reported

A task contributes a token saving **only if its answers were judged
`equivalent` or better for the memory arm**. Concretely:

* `equivalent` or `B better` → the token difference is a saving, reported;
* `A better` → **reported as a loss**, with the task quoted;
* `neither answered` → counted, never folded into an average.

An arm-A failure to answer where arm B succeeded is **not** an infinite
saving. It is reported as its own count: *tasks the repository could not
answer and the bank could.*

## Stratification

Every task is labelled, before the arms run, by where its answer could come
from:

* **in-repo** — present in code, docs, or git history of the pinned commit;
* **memory-only** — exists only because it was said in a session.

The label is assigned by a separate agent that is given the task and the
repository but **not** the bank, and asked only whether the repository
contains the answer. Totals are reported per stratum and never merged into a
single headline.

## Limits, stated in advance

1. **This does not measure whether the memories are true.** A confidently
   wrong memory that saves work scores well here.
2. **One machine, one operator, one repository.** Nothing generalises.
3. **30 tasks is small**, and per-stratum it is smaller. The distribution is
   reported, not just the mean.
4. **The author built the system being measured.** The judges are independent;
   the task selection is mechanical; the rules are in this file, committed
   first. That is the mitigation, and it is not a proof of neutrality.
