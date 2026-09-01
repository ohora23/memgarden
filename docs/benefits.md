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
with provenance and runs daily — 2,770 observations, 2,757 with source links.

**Updated 2026-08-30.** Both halves of that gap have since been closed, and one
of them came back positive: distillation was measured (**recall@10 0.291 →
0.370, +27%**, [result](evidence/distillation-value-result.md)) and CE-10's
mental models now run — four exist, three on `@after-consolidation`. The tier
was not merely unbuilt when this was written; it was **100% failing code with
no caller**, which is why nobody noticed for two months. What remains open is
narrower and stated below.

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
| command logs from current ingest | — | **0 of 676** nodes since cutover | [correction](evidence/command-log-pollution.md) |
| duplicate fact+observation pairs injected | 7.5% | deduplicated at recall | PR #16 |

The 22% → 0.9% change came from deleting 158 historical rows, and the
investigation that led there [was wrong twice](evidence/command-log-pollution.md) before
it was right — both corrections are in the record.

## Correctness of what stays stored

| | measured | |
|---|---|---|
| retracted facts hidden from every reader | one filter point, `search::hydrate` | schema v11 |
| retraction detected automatically at write time | **2 of 3** targets, **22** false positives — **ships off** | [detection](evidence/supersession-detection.md) |
| gold recall with the filter live and nothing marked | **0.370 recall@10 / 0.516 MRR / 0.306 nDCG@10** — identical to baseline | parity, not a win |

A fact can now be *retired* rather than only added, which is the first thing in
this project that improves what recall says rather than how fast it says it —
one of the first four mental models cited 17 nodes to assert a gate was
"awaiting signature" ten days after it was signed, and every sentence in it had
been true when written.

The honest half: **the store learned to record a retraction; the extractor did
not learn to notice one.** Detection was built, measured against this project's
own recorded retractions, and turned off. Setting it is a manual `POST` today.

## What is not measured, and should be said out loud

- **~~Whether distillation is worth anything.~~ Measured 2026-08-29 — it is.**
  Three gold arms, one variable: **recall@10 0.291 → 0.370 (+27%)**, nDCG@10
  0.262 → 0.306. It won *against* the grain of a label set that is 71% `world`.
  Two conditions came with it: observations alone **lose** (recall@10 0.177), so
  distillation supplements facts rather than replacing them; and the one stratum
  where it went backwards is **`conclusion`** — exactly where it should help
  most. One graded query, so that is a warning, not a finding.
  [result](evidence/distillation-value-result.md)
- **Whether a synthesis is *right*, as opposed to well-retrieved.** The +27% is
  a retrieval number. The one audit of content found one of four mental models
  confidently asserting three superseded facts. CE-12 removes the input that
  caused it; nothing yet scores the output.
- **How often a stored fact actually goes stale.** CE-12 can hide a retracted
  fact, but nothing counts how many of the 7,831 are already dead. The census
  cannot answer it — it asks whether a fact is *on disk*, not whether it is
  *still true* — and until something does, the value of the filter is a
  mechanism rather than a number.
- **~~The tier above it has never run.~~ It runs now — and whether it earns its
  keep in daily use is still open.** CE-10 was repaired in #39–#42: four mental
  models exist, three triggered by consolidation rather than a clock. All four
  have `cited_count = 0`, because `/reflect` is on no hook path — so the usage
  signal a promotion rule would need has no values yet, and writing the rule
  before it does is the shape that left CE-10 broken for two months.
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
