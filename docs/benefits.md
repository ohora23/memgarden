# What MemGarden actually buys — every measurement in one place

Assembled 2026-08-27, after v1 closed. Each row links to the evidence it comes
from, including the measurements that came out against the system.

**The short version.** MemGarden is **faster than what it replaced and better
at retrieval than what it replaced.** It surfaces knowledge the disk already has
and nobody would think to grep, in about seven milliseconds. It does not save
the user tokens — it spends them.

**Read the "index, not an archive" line below with its correction.** That
phrasing came from a corpus census that could not see the thing this system was
partly built for: **distillation**. Consolidation turns facts into observations
with provenance and runs daily — 2,770 observations, 2,757 with source links —
and **nobody has ever measured whether that is worth anything.** The tier above
it, CE-10's mental models, has **never run at all** (0 rows). Two of the three
purposes behind this project are therefore unmeasured or unbuilt, and this
document read as if they were settled.

## Measured against the system it replaced

| | MemGarden | legacy (Python + embedded Postgres) | |
|---|---|---|---|
| **recall quality**, blind panel, shipping config | **13 better / 5 worse / 1 equivalent**, 0 unjudgeable | — | [AC-1](evidence/ac-1-blind-panel.md) |
| **recall latency** | **7.1ms p50 / 7.8ms p95** at 3,000 nodes, all four arms | 830ms before its own fixes; ~51ms p50 after | [performance](performance.md) |
| **hook cost per turn** | **0.845ms p50** (`recall` + `retain`) | **33ms on its disabled path** — more to do nothing than these cost to work | [AC-2](performance.md) |
| **processes to run** | 1 | 3 (daemon, Postgres, Python hooks) | [architecture](architecture.md) |
| **backup** | `cp memgarden.db` | pg_dump + archive | |

The hook number is the one that changed the experience: an equivalent Python
hook measured 24ms cold, so the <10ms budget was never reachable in that
language. This is not a tuning win over the old system; it is a win the old
system's architecture could not have.

## Measured on its own terms

| layer | what it is | measured |
|---|---|---|
| **Layer 1 — injection cost** | tokens spent putting memories in front of the model | **1,325 tokens / 18.1 memories per turn** against a 1,024-token soft cap. This is a **cost**, and it is reported as one |
| **Layer 2 — extraction input saving** | what the ingest caps save the local LLM | **139,947 of 243,924 tokens, 57.4%**, across 10 sessions in the benefit ledger. Real, but it is **Ollama's** input, not the user's context |
| **Layer 3 — substitution** | does injected memory replace work | **it did not, on the sample available**: 11–7 worse on a blind panel, **+5% tokens**, **−25% wall clock**. [MX-3](evidence/mx-3-result.md) |
| **cost of measuring** | overhead of the metrics themselves | **88ns per request** — 0.00025% of the latency budget |

## The three findings that only make sense together

1. **MX-3** — on questions whose answers are in the repository, the memory arm
   was *worse on quality and faster on the clock*.
2. **The corpus census** — **0 of 60** sampled memories state anything that is
   not already on disk; under ~5% at 95% confidence.
   [Result](evidence/bank-uniqueness-result.md) ·
   [rules committed first](evidence/bank-uniqueness-criteria.md)
3. **A live case, 2026-08-26** — a month-old investigation into intermittent
   SIGSEGVs broke open because recall surfaced a memory that *"cargo test dies
   with SIGSEGV in SQLite's FTS5 index"*. That fact was in
   `book/src/roadmap.md` the whole time. Nobody was going to grep for it.

Read together they say **the value measured so far is retrieval, not storage.**

**That is narrower than it sounds, and narrower than it was first written.** All
three findings are about *raw facts* — whether a fact is on disk, whether
injecting facts substitutes for reading them. None of them touches the
**consolidated** layer, where groups of facts become an observation that exists
nowhere else in that form. The corpus census in particular scored observations
as "on disk" because their constituent terms are, which is backwards for what
they are. So the honest statement is: **the value of raw-fact retrieval is
measured; the value of distillation is not.**

## Quality of what gets stored

| | before | after | |
|---|---|---|---|
| command-log noise in what recall injects | **22%** of injected memories | **0.9%** | [cleanup](evidence/command-log-cleanup.md) |
| command logs from current ingest | — | **0 of 676** nodes since cutover | [correction](command-log-pollution.md) |
| duplicate fact+observation pairs injected | 7.5% | deduplicated at recall | PR #16 |

The 22% → 0.9% change came from deleting 158 historical rows, and the
investigation that led there [was wrong twice](command-log-pollution.md) before
it was right — both corrections are in the record.

## What is not measured, and should be said out loud

- **Whether distillation is worth anything.** Consolidation (CE-9) runs daily
  and has produced 2,770 observations with provenance. Nothing has ever compared
  an observation against the facts it was made from. This is the largest
  unmeasured claim in the project, and it is one of the three reasons the
  project exists.
- **The tier above it has never run.** CE-10's mental models: **0 rows**. The
  due-ness rule and the create path both exist; no ticker calls them, because
  `parity-gaps.md` judged that a scheduler with no consumer was the wrong
  default. Defensible, and also why the periodic-refinement half of the original
  goal has never happened.
- **Whether any of this makes the assistant better at the job.** Every number
  here is latency, token count, or retrieval quality. None is task outcome.
  MX-3 is the only attempt and its sample could not answer the question it was
  built for.
- **Whether the 0% uniqueness generalises.** This operator writes almost
  everything down — an Obsidian vault, dense commit messages, a design note per
  PR. A team that decides things in chat and documents little would plausibly
  measure a very different number, and that is the interesting experiment, not
  a caveat.
- **Extraction wall time.** The one live measurement ran on a contended GPU
  against a pathological fixture and is deliberately not quoted.
- **Whether the deleted 158 rows would ever have been usefully retrieved.** The
  claim is that they crowded the budget, not that any one was harmful.

## Honest summary

If the question is *"does this remember raw facts I would otherwise lose?"* — on
this machine, measurably **no**, because this operator already writes them down.
(If the question is *"does what it distils out of them have value?"* — **nobody
has measured that**, and it is the half the project was built for.)

If the question is *"does it put the right paragraph of the twelve thousand I
have written in front of the model before I finish typing?"* — **yes, in 7ms,
and measurably better than the system it replaced.**

The second question is the one worth asking of a memory layer sitting on top of
a well-documented project. It took a failed measurement, a corpus census and one
accidental live case to work out that it was the right question.
