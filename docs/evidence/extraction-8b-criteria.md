# Extraction quality, 14B vs 8B — criteria fixed before either arm ran

The README's *What GPU it needs* table has an 8 GB row (`qwen3:8b`) marked
"fits; extraction quality not yet measured". This file fixes, before any
result exists, what "similar quality" means and how it is judged. The commit
that adds this file precedes the commits that add results; `git log` is the
order.

## The arms

Same daemon binary (`memgardend` at master `d9bc6e7`, the one deployed),
same config apart from the model, fresh SQLite file per arm, the task ledger
and background consolidation off so the only Ollama traffic is extraction.

| arm | model | quant | measured VRAM |
|---|---|---|---|
| A | `qwen3-14b-nothink` (Qwen3-14B) | Q6_K | 12.2 GB |
| B | `qwen3:8b` (Qwen3-8B) | Q4_K_M | 5.6 GB |

The arms run sequentially on the same card; Ollama swaps the model. No other
retain traffic is intentionally sent during a run, but the live daemon is not
stopped, so a live session's retain can share the GPU. That affects wall time,
which is reported but not judged.

## The corpus

Five real Claude Code transcripts from this machine, chosen **before** either
arm ran, replayed through the real `Stop` hook (`memgarden` with a
`hook_event_name: Stop` payload and a fresh state dir) so both arms receive
byte-identical requests. The daemon's initial-backfill cap keeps the **last
300 messages** of a transcript; the questions below are written against that
window.

| id | project bank | size | messages in window |
|---|---|---|---|
| c1 | memgarden (mg-1b-import worktree) | 4.4 MB | 300 of 821 |
| c2 | memgarden (upgrade_contextswitching) | 6.2 MB | 300 of 1062 |
| c3 | PrInter_Improve | 1.5 MB | 212 of 212 |
| c4 | physicalAI_Mujoco | 217 KB | 38 of 38 |
| c5 | memgarden (upgrade_contextswitching) | 74 KB | all |

Transcript content is private and not committed; session ids and paths live
only in the scratch directory that ran the arms.

## What is counted

Mechanical, from each arm's `retain_jobs`, `memory_nodes` and daemon log:

- M1 **chunk failure rate** — `chunks_failed / chunks_total`.
- M2 **parse failures and truncations** — `ollama response failed to parse`
  and `truncated=true` lines. This is the schema-compliance signal the 14B has
  been bitten by three times.
- M3 **facts per chunk**, and the `fact_type` distribution. Reported, not
  judged: more facts is not better.
- M4 **dated facts** — share of nodes with `event_date` or `occurred_start`.
  Reported.
- M5 **wall time per chunk**. Reported.

Judged, by the AC-1 rubric (`ac-1-criteria.md`), on recall:

- Q1–Q15 in `extraction-8b-questions.jsonl`, written from the corpus windows
  before either arm ran. Each is asked of both arms' `/recall` (same daemon
  build, same budget) against the bank the transcript belongs to.
- Each returned item is **적중** (answers the question or is its key
  evidence), **주변** (same topic, does not answer) or **잡음** (unrelated,
  background command noise, duplicate).
- Per question: **better / equivalent / worse** for arm B, by the AC-1 rules
  (a tie is equivalent; more hits wins; equal hits and clearly less noise
  wins).

## Blinding

A script shuffles which arm is shown left or right per question and writes the
mapping to a sealed file. The judge labels left/right only, then the mapping
is opened. The judge is the person who built the system and wrote the
questions, as in AC-1; that is a stated limit, not a hidden one.

## The decision, fixed now

Arm B is **"similar enough for the 8 GB row"** only if all four hold:

1. total 적중 over the 15 questions: `hits_B ≥ 0.8 × hits_A`;
2. per-question tally: `worse ≤ better + 3`;
3. M1: `rate_B ≤ 2 × rate_A` and `rate_B ≤ 15%`;
4. M2: parse failures + truncations for B ≤ 2 × A (with A ≥ 1, else B ≤ 2).

If any fails, the README row changes to "12 GB minimum" and says why. If all
hold, the row's caveat is replaced by the measured numbers. Either way the
numbers go in `extraction-8b-result.md`.

## Limits, stated in advance

- n = 5 transcripts, 15 questions, one judge. This decides a README row, not
  a model choice for everyone.
- The corpus is Korean-heavy and about this project; a different corpus could
  rank the models differently.
- Recall parity measures whether the *facts a session would ask for* were
  extracted and are retrievable. It does not measure whether every fact is
  true; a plausible wrong fact is scored 적중 if it answers the question. M2
  is the only signal for malformed output.
