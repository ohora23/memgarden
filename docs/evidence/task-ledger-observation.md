# Observing the task ledger before anything reads it

**Status: open, started 2026-09-03.** Findings go in §4 as they arrive.

Migration `0012` ships a write path and no read path. This is the record of the
stage in between: the rows accumulate, a person reads them, and only then does
anything get injected into a prompt.

## 1. Why this stage exists rather than shipping the read path

The local prior is discouraging and specific. MX-3 measured the memory arm as
**11-7 worse on a blind panel at +5% tokens**, and its own stratification says
why that sample could not settle the question — 24 of 27 answers were already on
disk. So "the extractor writes something worth injecting" is a claim this
project has already been wrong about once in a neighbouring form, and the
cheapest way to be wrong again is to assume it.

There is also a documented shape for the failure this tier could *cause*. A
2026 survey (arXiv:2606.30306 §3.2.1) names it: *"the agent believes a task is
open that has been completed or cancelled, or it resumes an obligation whose
preconditions have since changed"* — and finds the corpus almost never tests
for it. A ledger that reads plausibly and is stale costs more than no ledger,
by the same mechanism MX-3's t02 showed for facts.

## 2. How to read

```bash
scripts/ledger.sh                    # live database
MEMGARDEN_DB=/path/to.db scripts/ledger.sh
```

It prints every row and flags the two failures that can be caught mechanically.
Everything else needs a person, which is the point.

## 3. What to judge — the questions, fixed in advance

Fixed **before** reading so the answers are not fitted to whatever shows up.

### 3.1 Is a row worth injecting at all?

The test is not "is it accurate". It is: **would a session that had forgotten
everything be able to act on this, and would it be faster than re-reading the
repo?** MX-3 says the second half is where memory has lost before.

- [ ] Q1 Could you resume the work from `goal` + `next_action` alone?
- [ ] Q2 Does it name the transcript's own nouns — files, PR numbers, branches —
      or does it paraphrase into generalities?
- [ ] Q3 Is anything in it **wrong**? Count these separately from "vague": a
      retrieved, plausible, wrong memory is the expensive kind.

### 3.2 Do the four fields earn their separation?

The first live row had `open` and `next_action` byte-identical. One toy
transcript proves nothing; a pattern is a prompt defect.

- [ ] Q4 How many rows collapse `open` into `next_action`? (the script counts)
- [ ] Q5 Does `done` ever carry something `memory_nodes` does not already have?
      If never, `done` is duplicated storage and should go.
- [ ] Q6 Does `goal` survive its task finishing, or does the next job replace it?
      A stale `goal` is the survey's failure mode arriving.

### 3.3 Are the anchors any good?

`anchors` is the whole staleness defence. It is currently `cwd` + `paths` only;
branch and HEAD are the Stage 4 gap.

- [ ] Q7 Do `paths` point at files the work actually touched?
- [ ] Q8 Would `cwd` alone have caught a branch switch? (expected: no — this is
      the measurement that justifies Stage 4)

### 3.4 Cost

- [ ] Q9 What did the extra Ollama call add to job wall-clock? Compare
      `retain job finished ... elapsed_ms` before and after `0012`.
- [ ] Q10 Did any job fail *because* of the ledger? (expected: no — errors are
      logged and dropped)

## 4. Findings

*(empty — to be filled as rows accumulate)*

| date | rows | collapsed | hollow | note |
|---|---|---|---|---|
| | | | | |

## 5. The decision this feeds

One of three, and the third is a real option rather than a polite one:

1. **Ship the read path** — content is worth injecting. Trigger is the phrase
   test (measured F1 0.615 against a 0.124 baseline), gated by re-checking
   `anchors` against the filesystem.
2. **Fix the writer first** — content is the right shape but thin or collapsed.
   The prompt in `crates/memgardend/src/retain/ledger.rs` is the lever.
3. **Turn it off** — `retain.write_task_ledger = false`. If the rows say nothing
   a resuming session could not get faster elsewhere, the honest move is to stop
   paying one Ollama call per retain job for them. The tier being architecturally
   correct (every system that solved continuity keeps two) does not make *this*
   implementation of it useful.

## 6. Why this is not a benchmark

No A/B here, deliberately. An A/B needs a task-completion metric and a query set
that this corpus cannot supply — the same wall AC-1 hit when 7 of 20 queries
turned out unjudgeable, and the same one MX-3 hit with 24/27 in-repo. Reading
the rows answers a cheaper question ("is there anything here") that has to be
yes before the expensive one is worth asking.

If §4 says yes, the A/B that follows needs its own design and its own corpus.
