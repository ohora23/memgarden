# MX-4 — the result: no effect larger than the noise

**Read the re-run first** (end of this file). The first run's sandbox hid the
whole home directory; its apparent memory-only win (3–0) was mostly that
artifact, and it disappeared once only the notes were hidden.

## First run (v1) — kept as recorded

Run 2026-10-18 under the rules in [`mx-4-criteria.md`](mx-4-criteria.md),
committed (e736874) before any task was drawn. Two amendments, both recorded
there before any arm ran: the pool was widened to `history.jsonl` because
transcripts older than 30 days are deleted, and a `connect()` shim was added
because the live daemon answers recall on loopback without a token.

**Headline, by the rule fixed in advance: not shown.** Two of the three
conditions hold and the third fails — and the third is the one this run's
harness contaminated most. Both halves matter.

## The sample

37 candidates survived rules 1–3, 5, 6; 22 survived rule 4 (three classifiers,
unanimous on all 37). All 22 were used. 17 had a reference answer, 5 did not.

Stratified before the arms ran, by an agent in arm A's sandbox:

| findable | memory-only | neither |
|---|---|---|
| 13 | 4 | 5 |

That is 4 memory-only tasks against MX-3's 1 — the shift this design aimed for,
and still small.

## The verdicts

Blind, three judges per task (`claude-opus-5-5`, full read access to the
machine and its notes), majority of three, 0 unparseable.

| | no memory better | memory better | equivalent | neither |
|---|---|---|---|---|
| **all 22** | 7 | **10** | 4 | 1 |
| findable (13) | 4 | 7 | 2 | — |
| **memory-only (4)** | **0** | **3** | — | 1 |
| neither (5) | 3 | 0 | 2 | — |

| condition, fixed in advance | | |
|---|---|---|
| 1. memory better ≥ no-memory better + 3 | 10 ≥ 10 | **holds**, at the boundary |
| 2. memory-only: memory better > no-memory better | 3 > 0 | **holds** |
| 3. memory arm's error flags ≤ no-memory arm's | 31 > 28 | **fails** |

Stall flags (asked the owner, or could not proceed): no memory **33**,
memory **21**.

The one `neither answered` is t22 — *"which session opened the PR on <a project>?"* —
the most memory-shaped question in the sample. The abstain gate dropped all
10 candidates below 0.83 and injected nothing, so both arms had nothing.
*Corrected after the re-run:* the gate was right. The prompt was typed in one
project and asks about a session in another; before the prompt, the typing
project's bank held **0** nodes naming that project and the other project's
bank held 37. Recall is per-project, so there was nothing to find — every
candidate was an unrelated "PR" memory. See "t22, re-attributed" below.

Cost, read after the verdicts: output tokens 78.6k vs 87.3k (+11%), input
4.01M vs 3.57M (−11%), wall clock 1,095 s vs 1,145 s, $3.05 vs $3.01.

## Why condition 3 cannot be read at face value

**The sandbox removed far more than notes.** It replaced `$HOME` with an empty
tmpfs and bound back only toolchains. A user who keeps no notes still has
`~/.claude/plugins`, `~/.config/systemd/user`, their other repositories, and
the memgarden data directory. Judges, reading the real machine, flagged the
consequences as errors in both arms: *"ralph isn't installed"* (t12, both
arms), *"no systemd unit"*, *"hooks not registered"* (t09, t15), *"the plan
does not exist"* (t10, both arms).

The memory arm is hit harder by this, for a mechanical reason: its injected
memories mention those things, so it asserts more about them — and then
reports the sandbox's view of them. The no-memory arm says less and stalls
more (33 vs 21). So the error flags in this run measure, in large part, *how
much an answer commits to claims about a home directory the sandbox emptied*.

**t10's workspace was nearly empty**: at the prompt's time the
`mujoco_sensor_plugin` repository had one commit tracking `.gitignore`; the
code was untracked. A clone cannot recover an untracked working tree.

## The losses, quoted, because they are the informative part

* **t02** (unanimous for no memory) — the memory arm said the daemon was
  *"started by hand, no systemd unit"*. That came from a memory of
  2026-08-09; the cutover on 08-21 gave it a unit. **A stale memory, repeated
  as fact.** This is the failure MX-3's t02 named, met again. *Corrected
  after the re-run:* it happened here because this sandbox hid
  `~/.config/systemd/user`, so the unit could not be checked — see "t02,
  replayed" below.
* **t09, t14, t15** — the memory arm recognised it was sandboxed and declined
  to conclude; the no-memory arm reported what it saw. On t14 the no-memory
  arm was right (a real GPU fault); on t09 the judges preferred the arm that
  did *not* trust the sandbox. These are harness interactions.
* **t01, t20** — both arms found the same facts; the no-memory arm gathered
  more context (crash dumps, memory peaks).
* **t04** — judges found no errors in either; the no-memory answer matched the
  reference's three recommendations more closely.

## The wins in the memory-only stratum

t10, t11, t12 — all unanimous. Asked *"how do I run ralph?"* with no notes,
the memory arm named the owner's actual tool (oh-my-claudecode's ralph) and
how they had used it; the no-memory arm had no way to know. This is the
case the experiment exists for, and the bank answered it 3 of 3 times it
injected anything.

## Sensitivity

Excluding the 9 tasks where either arm *attempted* port 9100 (all refused,
no response reached any transcript — checked by content, not by keyword):
4 / 4 / 4, plus t22. The criteria said such runs would be discarded; that rule
was written before the shim made touching the port impossible, so the primary
table keeps them and this line reports the other reading.

## What this run supports, and what it does not

* **Supported:** where the answer exists only in past conversation, the bank
  recovers it — 3 wins, 0 losses, unanimous, and the stall gap (33 vs 21)
  points the same way.
* ~~Supported: stale memories still mislead (t02).~~ Only where the state
  cannot be checked — see "t02, replayed".
* **Not supported:** a net claim that MemGarden helps a notes-less user.
  The pre-registered gate fails on errors, and the error count is confounded
  by the harness.

## What a re-run needs

1. **Hide notes, not the home directory**: keep `~/.claude/plugins`,
   `~/.config`, other repositories; hide the vault, `MEMORY.md` files,
   transcripts, the memgarden DB, and repo notes.
2. Reconstruct the working tree, not only the last commit, where a transcript
   allows it — or drop tasks whose repository was empty at the time.
3. Report abstentions as their own count. (t22's turned out to be correct —
   see "t22, re-attributed".)

Raw data: `~/.local/share/memgarden/mx4-run-20261018/` (tasks, injections,
workspaces, transcripts, judge outputs). Not committed: it holds personal
prompts.

---

## Re-run (v2) — notes hidden, home visible

Registered in the criteria (5a3a620) before it ran. Same tasks, frozen
injections, workspaces, instruction, model and judges; only the sandbox
changed. Arm assertions 22/22 per arm, 0 daemon responses in any transcript;
7 tasks with a refused port-9100 attempt.

Stratified again in the new sandbox: findable 13, memory-only **6**, neither 3.

| | no memory better | memory better | equivalent | neither |
|---|---|---|---|---|
| **all 22** | 6 | **8** | 7 | 1 |
| findable (13) | 3 | 6 | 4 | — |
| memory-only (6) | **2** | **1** | 2 | 1 |
| neither (3) | 1 | 1 | 1 | — |

| condition, fixed in advance | | |
|---|---|---|
| 1. memory better ≥ no-memory better + 3 | 8 < 9 | **fails** |
| 2. memory-only: memory better > no-memory better | 1 < 2 | **fails** |
| 3. memory arm's error flags ≤ no-memory arm's | 23 ≤ 26 | holds |

Stall flags 20 vs 22. Cost $2.81 vs $2.88; output tokens 79.8k vs 76.7k.

**Headline: not shown.** MemGarden did not measurably help a notes-less user
on this sample.

### What changed between the runs, and what that says

* **9 of 22 majority verdicts changed** between v1 and v2. One run per arm
  per task is too noisy for 22 tasks to resolve an effect of this size; the
  overall margin in both runs (+3, +2) is inside that noise.
* **The v1 memory-only wins were the sandbox.** t11/t12 (*"how do I run
  ralph?"*) went B, B in v1 and A, A in v2: with `~/.claude/plugins` visible,
  the no-memory arm read the installed oh-my-claudecode ralph skill itself.
  In v2 the memory arm lost them by asserting that the official `ralph-loop`
  plugin was installed — it is only cloned in the marketplace.
* **t02 lost unanimously in both runs, for different reasons.** In v1 the
  memory arm repeated the stale *"no systemd unit"* memory; in v2 all three
  judges cleared it of that (`error_B: false`). The v2 memory-arm answer was an
  off-topic note about a background job; the preferred answer had its own
  error (it said `docs/` was absent) but answered the question. See "t02,
  replayed".
* **t22 abstained in both runs**, correctly: the answer lived in another
  project's bank (see below).

### Defects of this harness, still present in v2

* Hiding the memgarden data directory is itself visible: both arms reported
  *"the data dir holds only models/"* on t09 and judges flagged it. A
  notes-less user still has a database; there is no way to give arm A one
  without giving it memory.
* **Blinding leak.** Judges read the whole home directory, which includes the
  run directory whose paths name the arm. 5 of 66 v1 judges and 1 of 66 v2
  judges opened it; one in each run listed both arms' workspaces. No verdict
  changes if those judges are dropped (t04 v2: the other two say no-memory
  better). A re-run must hide the run directory from judges.

### What this supports

* Not supported: a net benefit for notes-less users. Two runs, opposite
  memory-only results, both inside the noise.
* Not established: whether stale state memories mislead. On t02 the stale
  claim appeared in 2 of 6 runs where the state could not be checked (v1's
  original and 1 of 5 replays) and in 0 of 20 where it could. Five-run cells
  cannot separate those, and the two sandboxes differ in more than this.
* Found: recall cannot answer a question about another project's sessions
  (t22) — a scope limit of per-project banks, not a gate defect.
* Learned about measurement: a with/without comparison here needs several
  runs per arm per task, and a sandbox that removes memory without removing
  the things a memory system's own install leaves on disk.

### t22, re-attributed

Checked after both runs, on the live database restricted to nodes created
before the prompt:

| bank | nodes naming the asked-about project | nodes mentioning "PR" |
|---|---|---|
| the project the prompt was typed in | **0** | 100 |
| the project where that work happened | 37 | 217 |

The gate dropped ten unrelated "PR" memories, which is what it is for. What
failed is scope: the hook recalls from the bank of the directory the prompt
was typed in, and the question was about work done from another directory.
Answering it needs cross-bank recall — a design decision (noise, privacy
between projects), not a threshold change, and left open here.

### t02, replayed

The memory arm of t02 re-run with its frozen injection, one variable at a
time, answers labelled by three blind no-tool classifiers (majority) for
*"states as current that memgardend has no unit / is started by hand"*. The
classifier says yes 3/3 on v1's original answer and no 3/3 on v2's.

| sandbox | shipped rendering | + preamble | + age suffix | + both |
|---|---|---|---|---|
| notes hidden, state checkable (v2) | 0 / 5 | 0 / 5 | 0 / 5 | 0 / 5 |
| home hidden, state uncheckable (v1) | 1 / 5 | — | — | 0 / 5 |

The preamble tried: *"Dated notes from past sessions, not a live view: a
fact about what runs, is installed or is configured may have changed since
its date — check before stating it as current."* The age suffix:
`(2026-08-09, 69 days ago)`.

When the agent could look, the stale memory did no harm in 20 of 20 runs.
When it could not, the stale claim appeared in 1 of 5 replays (2 of 6 with
v1's original run). 1/5 against 0/5 is not evidence that either rendering
change helps, and five-run cells are not evidence that checkability is the
cause, so **neither ships** and the cause stays a hypothesis. The
newer contradicting fact (created 08-25, *"a systemd user service now"*) was
in the bank and was not recalled — that is the retrieval side of the open
supersession problem, not something a rendering change reaches.
