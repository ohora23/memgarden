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

### 3.5 How stale is a ledger the moment it is written?

**Added 2026-09-03 after the first live row, which arrived 107 minutes stale.**
This was not anticipated and is structural rather than a sampling artifact, so
it gets its own group.

The transcript is captured when the hook POSTs the job; the ledger is written
when the job *finishes*. The gap between those is the job's duration, and the
ledger is born exactly that far behind. On an 18-chunk job that was 107 minutes,
during which the work it describes had been finished, reviewed, merged and moved
past.

- [ ] Q11 Distribution of `task_ledger.updated_at − retain_jobs.created_at`.
      Is the lag routine or only on large transcripts?
- [ ] Q12 How often is the `goal` already finished by the time the row lands?
      This is the survey's "stale commitment" arriving through the pipeline
      rather than through a missed update.
- [ ] Q13 Does the staleness check in the (unbuilt) read path catch it? `anchors`
      compares `cwd` and file existence, and **neither moves when a task merely
      finishes** — so the answer is probably no, and that matters more than the
      lag itself.

### 3.6 Does the newest job deserve the row?

**Added 2026-09-03, three minutes after 3.5, when a 1-chunk job overwrote an
18-chunk one and turned a whole session into a stray `/config` exchange.**

`upsert` replaces every content field, by design: a superseded goal that stays
behind is the survey's stale commitment. The cost of that choice only became
visible with two jobs in flight — the row goes to whichever finishes last,
regardless of how much of the work it saw.

- [ ] Q14 How often does a row get replaced by a job covering fewer chunks?
      Compare `retain_jobs.chunks_total` across successive writers for a bank.
- [ ] Q15 When that happens, is the surviving row worse? "Fewer chunks" is a
      proxy; a short job on the right material could be better.
- [ ] Q16 Is `updated_at != created_at` common — i.e. how often is any bank's
      row rewritten at all, versus written once and left?

## 4. Findings

| date | rows | collapsed | hollow | note |
|---|---|---|---|---|
| 2026-09-03 | 1 | 0 | 0 | first live row — no collapse, one fabricated path, 107 min stale |

### 2026-09-03 — the first live row

```
goal         MemGarden 도입 효과 정리 … 코드 검색 낭비 분석
done         "데이터가 다 모였습니다. 노트를 작성하겠습니다."
open         "작성 중입니다."
next_action  Final destination path: `Project/MemGarden/01-Effectiveness_Analysis.md` …
anchors      cwd=…/upgrade_contextswitching · paths=7
```

**Q4 — the collapse did not repeat.** `open` and `next_action` differ here, so
the byte-identical pair seen on the 3-message smoke test looks like an artifact
of that toy input. One row, so this stays open.

**Q3 — one thing is wrong, and it is the expensive kind.**
`Project/MemGarden/01-Effectiveness_Analysis.md` **does not exist**; the note
was saved as `작업장부-스키마v12와-이점정리-그리고-Orca그래프.md`. The model
appears to have built a plausible filename out of a skill preamble's format
rather than from anything that happened. A session injected with this would go
looking for a file that was never written — the same shape as MX-3's t02, where
a retrieved, plausible, wrong memory beat having none.

**Q7 — the anchors are good.** All 7 paths are files the work actually touched
(`0012_task_ledger.sql`, `migrate.rs`, `task_ledger.rs`, `retain/ledger.rs`,
`retain/mod.rs`, `boundary-replay.py`, the memory note). This half is working.

**Q11/Q12 — the row was born 107 minutes stale.** Job created 00:17, ledger
written 02:04, 18 chunks, ended `partial`. In that window the work it describes
was finished, a plan was approved, PR #48 was opened and merged. So `goal`
names a completed task, and `done`/`open` quote the operator's own sentences
verbatim rather than summarising — all three are symptoms of the same cause.

**What this does NOT yet say.** n=1. The fabricated path could be one bad
generation; the lag could be specific to an 18-chunk transcript. Both need the
distribution before they mean anything. What is already structural, regardless
of sample size, is Q13: `anchors` compares `cwd` and file existence, and a task
*finishing* moves neither — so the staleness defence as designed would not have
caught this row.

### 2026-09-03, three minutes later — a worse row replaced it

Re-reading to check the new lag column found a **different** row. The 18-chunk
job (79 facts) had been overtaken by a **1-chunk job** queued 8 minutes later,
and replace-on-write did what it is supposed to do:

```
goal         Determine which configuration setting the user needs and proceed accordingly.
done         User requested configuration settings but did not specify which one.
open         User needs to specify the configuration type to proceed.
next_action  Ask the user to clarify which configuration they need: …
anchors      cwd=…/upgrade_contextswitching · paths=0
```

That is the whole session — a schema migration, four merged PRs, a deployment,
an approved plan — reduced to a stray `/config` exchange, because the last job
to finish happened to carry one chunk covering it.

**This is a design defect, not a sampling accident, and it is mine.** The write
path takes whatever the newest finishing job saw. It has no notion of which job
carried more of the work, and 1 chunk overwrites 18 exactly as readily as the
reverse. The 8-minute-later job also *finished first*, so ordering by
`created_at` would not have saved it either.

Candidate directions, **none chosen** (see §5, and note this is a third lever
distinct from content and timing):

- Do not replace a row from a substantially smaller job — compare `chunks_total`
  or covered byte range and keep the richer one.
- Merge rather than replace, which needs a second model call and reintroduces
  the contradiction-resolution problem `ledger.rs` explicitly avoided.
- Key the ledger by something narrower than the bank so a `/config` aside and a
  migration are not competing for one row — but the bank key is itself a
  measured decision (`boundary-replay.py`), so this one fights earlier evidence.

The honest reading is that "one row per bank, replace on write" was chosen to
avoid stale commitments and, in doing so, made the ledger **lossy against the
most recent small event**. Q14 is now the question that matters most.

## 5. The decision this feeds

One of three, and the third is a real option rather than a polite one:

1. **Ship the read path** — content is worth injecting. Trigger is the phrase
   test (measured F1 0.615 against a 0.124 baseline), gated by re-checking
   `anchors` against the filesystem.
2. **Fix the writer first** — content is the right shape but thin, collapsed, or
   stale. Two different levers, and the first row implicates both:
   - *content* — the prompt in `crates/memgardend/src/retain/ledger.rs`. The
     fabricated path and the verbatim quoting are prompt problems.
   - *timing* — the 107-minute lag is not a prompt problem. Candidates, **none
     chosen and none costed yet**: write the ledger at job *start* rather than
     at the end (the transcript tail is already in hand the moment the job is
     queued, and the extraction it currently waits behind contributes nothing to
     it); or re-read the tail at write time; or spawn the call detached at start
     so it neither delays the flush nor waits for the chunks. Picking one before
     Q11 says how common the lag is would be fitting a fix to a single row.
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
