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
- [x] Q5 Does `done` ever carry something `memory_nodes` does not already have?
      If never, `done` is duplicated storage and should go. **Never, 5 of 5 —
      dropped by migration `0013` (§7).**
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
| 2026-09-03 | 1 | 0 | 0 | third writer — 18 chunks retook the row from the 1-chunk job; 116 min stale; `next_action` orders a merge that happened 26 s *before* the job was queued |
| 2026-09-03 | 1 | 0 | 0 | fourth writer — 7 chunks replaced 18; the first resumable row, 63 min stale, `open` holds two transient API errors; a following 0-fact job did **not** overwrite it |
| 2026-09-03 | 1 | 0 | 0 | fifth writer — 3 chunks replaced 7; `next_action` orders a merge done 13 s *before* the POST (second time); 19 min stale; 0 anchor paths |

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
  or covered byte range and keep the richer one. **Refuted by the fourth row:**
  the 7-chunk job that replaced 18 was the better row.
- Merge rather than replace, which needs a second model call and reintroduces
  the contradiction-resolution problem `ledger.rs` explicitly avoided.
- Key the ledger by something narrower than the bank so a `/config` aside and a
  migration are not competing for one row — but the bank key is itself a
  measured decision (`boundary-replay.py`), so this one fights earlier evidence.

The honest reading is that "one row per bank, replace on write" was chosen to
avoid stale commitments and, in doing so, made the ledger **lossy against the
most recent small event**. Q14 is now the question that matters most.

### 2026-09-03, two hours later — the 18-chunk job took the row back

The `/config` row lasted 128 minutes. A third job for the bank (18 chunks, 97
facts, the session that wrote PR #49) finished at 19:13 UTC and replaced it:

```
goal         PR #49를 머지하고 며칠 관측을 기다리자
done         CI 완료
open         덮어쓰기가 얼마나 흔한가(Q14)를 확인해야 함
next_action  PR #49를 머지합니다.
anchors      cwd=…/upgrade_contextswitching · paths=5
```

**Q1/Q3 — not resumable, and wrong in the expensive way again.** PR #49 was
merged at 17:19:08 UTC. The job was queued at 17:19:34 — **26 seconds after
the merge**, so the transcript almost certainly carried the outcome — and the
row was written at 19:15. A session acting on `next_action` would try to merge
a merged PR. This is the second stale-commitment row out of three, and the
first where the staleness is not the pipeline's: the *goal was already done
when the transcript was captured*, and the writer reported the announced
intent rather than the result that followed it.

**Q2/Q7 — the nouns and the anchors are right.** `PR #49`, `CI`, `Q14` are the
transcript's own words. All 5 paths exist, including the Obsidian note whose
filename the first row fabricated — this time it is the real one.

**Q5 — `done` carried nothing new.** `done` says `CI 완료`. The same job wrote
99 `memory_nodes`, one of which reads *"User decided to merge PR #49 after CI
passed … CI completed successfully"*. Three rows in, `done` has never held
anything the nodes did not, and has been shorter every time.

**Q14/Q16 — the bank's row has now had three writers.** In order of finishing:
18 chunks (79 facts) → 1 chunk (1 fact) → 18 chunks (97 facts). Two
replacements, one of them by a smaller job. The row currently holds the
richest version, but only because nothing was queued behind the third job;
"the newest large job wins" is not a policy that produced this, it is the
absence of a competitor.

**Q11 — 116 minutes, and the lag is queue wait as much as job duration.** The
three rows' lags are 107, 102 and 116 minutes. The middle one is the 1-chunk
job, whose own extraction took **3 minutes**; it spent the other 99 waiting
behind the 18-chunk job in the serial worker. So the "write at job start"
candidate in §5 only helps if *start* means the POST, not the moment the
worker picks the task up — the queue is the lag on small jobs.

**Q9 — the ledger call itself.** From the daemon log, `task ledger written`
follows `retain job finished` by 167 s, 34 s and 148 s. `elapsed_ms` does not
include it (it runs after the terminal flush, PR #47), but the worker is
serial, so each call is that long added to the wait of whatever is queued
next — the 1-chunk job above started only after the first row's 167 s.

**Q12 — 2 of 3 rows named a goal that was already finished** when the row
landed (the note in row 1, the merge here). Q13 stands: `cwd` and the 5 paths
would all check out, and the staleness defence would pass this row through.

### 2026-09-03, the same evening — the first row a session could act on

The session that ended 13:34 UTC (the handoff after PR #49) was retained as a
7-chunk job that finished `partial` (6/7, 53 facts) at 14:35 and wrote the
row at 14:37:

```
goal         Answer Q14: Determine how often a row gets replaced by a job covering fewer chunks
done         Obsidian note updated with handoff section, title corrected to PR #45~#49, resume.md written, native memory refreshed, MEMORY.md pointer not duplicated
open         API Error: 500 Internal server error; API Error: 529 Overloaded
next_action  Run `scripts/ledger.sh` to observe the data and analyze the frequency of row replacement by smaller jobs
anchors      cwd=…/upgrade_contextswitching · paths=1
```

**Q1/Q2 — yes, for the first time.** `goal` + `next_action` is exactly what
the next session did: run `scripts/ledger.sh` and answer Q14. The nouns are
the transcript's own (`Q14`, `scripts/ledger.sh`, `resume.md`, `PR #45~#49`),
and nothing in `goal`/`done`/`next_action` is wrong.

**Q3 — `open` is noise promoted to a field.** Two transient API errors from the
client (`500`, `529 Overloaded`) are the whole of "what is outstanding". They
are not wrong as facts — the same job stored each as a `memory_nodes` row — but
a session told these are its open items would waste a turn on them. This is a
different defect from row 1's fabrication: nothing invented, the wrong thing
kept.

**Q12 — 3 of 4 rows landed after their goal was done.** This one was born 63
minutes behind (job 61 min, ledger call 155 s) and in that window the next
session had run the script, written the third-row findings and merged PR #50.
The row was correct and already executed by the time it existed. Q13 holds
again: `cwd` and the one path check out.

**Q5 — `done` is a subset for the fourth time.** Every clause of it (Obsidian
note, `resume.md`, native memory, the `#45~#49` title fix) is a stored node
from the same job.

**Q7 — 1 path, and it is the least important one.** `resume.md` exists, but the
session also wrote the Obsidian note, the memory file and `MEMORY.md`, all of
which `done` names and `anchors` omits. Rows 1 and 3 had 7 and 5 paths; the
writer's anchor coverage is uneven.

**Q14/Q15 — the third replacement, again by a smaller job, and this time the
smaller job was right.** Writers so far: 18 → 1 → 18 → 7 chunks. Two of three
replacements went to fewer chunks. But this 7-chunk row is the best of the
four, and the 18-chunk row it replaced ordered a merge that had already
happened. §4's first candidate — *keep the richer row by `chunks_total`* —
would have kept the wrong one. Chunk count measures how much transcript the
job saw, not how recent or how relevant; Q15's warning was correct and the
candidate is out.

**The 0-fact job did not write.** A 1-chunk job queued 61 s later (the next
session's opening exchange, 0 facts, 110 s of work after 62 min in the queue)
finished at 14:39 and left the row alone: no `task ledger written`, no
`extraction failed`. The only silent path in `ledger.rs` is the guard that
drops a reply with an empty `goal` (or an empty tail), so this is the first
evidence that guard does what it is for — it is the exact shape of the
`/config` overwrite in the second row, and this time nothing was lost. Whether
it skipped on the tail or on the goal is not in the INFO log.

### 2026-09-03, 15:09 UTC — the fifth row, and the third row's shape repeats

A 3-chunk job (18 facts, `done`, 16 min) queued at 14:50:22 replaced the
7-chunk row at 15:09:

```
goal         PR #51 머지
done         PR #50 머지
open         CI가 아직 진행 중입니다.
next_action  CI가 끝나면 PR #51을 머지합니다.
anchors      cwd=…/upgrade_contextswitching · paths=0
```

**Q3/Q12 — the third row's defect is a pattern, not a one-off.** PR #51 was
merged at 14:50:09, **13 seconds before the job was POSTed**. Row 3 had the
same shape at 26 seconds. Both times the transcript tail ended with the
operator announcing an action and the action completing, and both times the
writer kept the announcement. That is 2 of 5 rows with an identical, specific
failure — a stale commitment manufactured at write time from a tail that
already contained the outcome — and 4 of 5 rows landing with the goal done.
This is a **prompt** problem (the content lever in §5), independent of lag: the
lag here was only 19 minutes (job 16 min + ledger call 163 s).

**Q5 — `done` ⊂ `memory_nodes`, 5 of 5.** `PR #50 머지` is the node *"PR #50 was
merged with a squash merge, and the master branch became `ee566db`"*, shorter.

**Q7 — 0 anchor paths, the second time.** Coverage across the five rows is
7 · 0 · 5 · 1 · 0. The transcript this job saw includes edits to
`docs/evidence/task-ledger-observation.md` and to the Obsidian note, and the
nodes from the same job name the document; `anchors` names nothing. When
`paths` is empty the staleness check has only `cwd`, which never moves.

**Q14/Q15 — 18 → 1 → 18 → 7 → 3 chunks.** Four replacements, three by a
smaller job. This time the smaller job's row *is* worse than the one it
replaced (row 4 was actionable; this one orders a finished merge), so Q15's
answer is now "sometimes, and chunk count does not predict which" — the
refutation of the `chunks_total` candidate stands, and no other cheap proxy
has appeared. What separates the good row from the two bad ones is not size or
recency but **whether the tail ended on an announcement**; that is a property
of the transcript, not of the job.

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
     **After the third row:** "start" has to mean the POST. The worker is
     serial and a 1-chunk job spent 99 of its 102 minutes queued, so a ledger
     written when the worker picks the task up is late by nearly as much.
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

## 7. Decision — 2026-09-03, after five rows: fix the writer first

Option 2 of §5, chosen by the operator on the five rows above. Not option 1,
because 4 of 5 rows named a finished goal and one of them would have sent a
session to merge a merged PR. Not option 3 yet, because row 4 was a row a cold
session could have acted on, and the three defects that spoiled the others
each have a cheap, specific fix that the rows themselves point at. Five more
live rows through the same script decide between 1 and 3.

What changed, and the row that justifies each:

1. **Prompt (content).** Rows 3 and 5: the tail ended with an action announced
   and then carried out, and the writer kept the announcement. The prompt now
   says an announced-and-completed action is finished and to look past the
   announcement to its result. Row 4: `open` held two transient API errors;
   the prompt now says `open` is work items only, and that tool or API errors
   are not one unless the work stopped because of them.
2. **`done` is gone** (migration `0013`). Q5: on all five rows it was a shorter
   copy of a `memory_nodes` row the same job had already written. Completed
   steps are facts; the fact tier has them.
3. **Written at POST, detached** (`retain/ledger.rs::spawn`). Q11: 107 · 102 ·
   116 · 63 · 19 minutes stale, and the 102 was 99 minutes of queue. The tail
   is in hand when the job is accepted and the ledger needs nothing from the
   extraction, so the call now starts from the handler as its own task. It
   still takes the single Ollama permit, so on a busy daemon it lands after
   the chunk in flight rather than after the queue. The guard for an empty
   goal now logs at INFO, because the one live skip (the 0-fact job after
   row 4) could not be told from a call still running.

What deliberately did not change:

- **Precedence.** `chunks_total` was refuted by row 4 and nothing cheaper
  replaced it. With the write at POST time, "last job to finish" becomes
  "last transcript POSTed", which is closer to "newest state" than before,
  and that is the whole of the fix for now.
- **Anchor coverage** (7 · 0 · 5 · 1 · 0 paths). That is the Stage 4 work in
  `.omc/plans/magic-context-parity.md`, not a prompt or a timing change.

The re-observation gate: five more rows, same script, same questions. Q1 (could
a cold session act on it) and Q3 (is anything wrong) are the ones that decide.
If the announced-action shape returns after the prompt change, the content
lever is exhausted and option 3 is the honest answer.
