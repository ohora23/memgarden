# Usage audit — can a local judge tell what an injection was worth?

2026-11-18. Every earlier answer to "did the injected memories help?" was a
hand-labelled sample of 47–60 turns, too small to tell *rare* from *never*
(0 of 47 has a 24% chance at a 3% contribution rate). This note calibrates an
automatic judge against a fresh hand-labelled sample and records what
`scripts/usage-audit.py` measures because of it.

## Calibration

- **Sample.** 30 turns drawn at random (seed 20261118) from the 575 prompts
  with an injection after the abstain gate went live (2026-10-17 → 11-17 UTC),
  129 injected memories, seven project banks. The audit below counts 876 for
  the same window because its unit is the injection, not the prompt: a prompt
  can trigger several, and messages queued mid-turn trigger their own.
- **Reference.** One reader labelled every memory against the prompt, the
  response and the tool calls of its turn: **U** used, **R** relevant but not
  relied on, **I** irrelevant, **M** misleading. A "used" label required that
  the information was not already in the session — checked against the
  transcript for the one case where it looked used. Result: **U 0, R 43, I 86,
  M 0.**
- **Judge.** The local model (hrvl) given the same prompt, response excerpt,
  tool-call excerpt and memories, blind to the reference, in two modes.

| judge | R-or-I agreement | Cohen's κ | irrelevant share (reference 0.67) | "U" labels (reference 0) |
|---|---|---|---|---|
| fast (thinking off) | 0.837 | **0.61** | 0.75 | 12 |
| thinking on | 0.845 | 0.63 | 0.74 | 11 |

**What the judge can measure:** the irrelevant share, as a trend. Its errors
run one way — 15 of the reference's relevant memories called irrelevant
against 5 the other way — so it reads about 7 points high, consistently.

**What it cannot:** "used". Every one of its "U" labels was a memory that
restated the prompt or something the session already showed. Telling *used*
from *already known* needs the transcript before the turn, not the turn.

## What the audit records

`scripts/usage-audit.py` (stdlib only) reads the Claude Code transcripts and
appends one JSON line per run to `~/.local/share/memgarden/usage-audit.jsonl`;
the systemd timer in `scripts/systemd/` runs it daily over a rolling 24-hour
window. A late or missed run leaves a gap or an overlap between lines; read
`window_start`/`window_end` rather than assuming one line per calendar day.

| field | how | trust |
|---|---|---|
| `turns`, `items` | one turn per injection attachment; its prompt is the latest user record or `queued_command` attachment before it | exact |
| `automated_turns`, `automated_items` | the prompt's `origin.kind` is `task-notification` or `peer`; `human` is never automated; with no origin, a machine prefix (`<task-notification>`, `[MASTER]`, a peer-session header) | close — a record with neither origin nor prefix counts as human |
| `known_items` | every identifier-like token (digit, path, hash, dotted or dashed name) of the memory appears as a whole token in the session's prompts, assistant text, tool inputs and compaction summary since the last compaction, and ≥70% of all its tokens do (≥90% with no such token). Tool results, file reads and other hooks' context are **not** included | approximate; undercounts what the model had seen |
| `judge_irrelevant` / `judged_items` | the fast judge on ≤40 sampled turns | reads ~7 points high (above) |
| `judge_labels` | raw counts, including U and M | for inspection only — not calibrated |

## First reading (2026-10-17 → 11-17, no judge)

Window 2026-10-17T00:00Z → 2026-11-17T17:40Z.

| | |
|---|---|
| injections | 876 |
| injected memories | 3,513 |
| on machine-sent prompts | 1,417 (40%) |
| already stated in the session's own prompts, answers or tool inputs | 539 (15%) — a floor, since tool output is not counted |

Two levers follow directly. Four in ten memories were injected into a turn
that a machine sent — a background-task notification, a peer session's
report — answering a prompt no person asked. And at least one in seven
repeated what the session had already said.

## First lever: skip machine-sent prompts (2026-11-18)

The recall hook does not receive `origin` (its input carries the prompt text,
session, cwd and event name only), so it decides from the text: a prompt that
starts with `<task-notification>`, `[MASTER]`, `Another Claude session` or
`[SYSTEM` is skipped when `[hooks] skip_machine_prompts = true` (the default).

Checked against Claude Code's own `origin.kind` on 1,121 non-empty prompts: the
rule matches 489/489 `task-notification`, 63/63 `peer` and 1/569 `human`.

Applied to the same month of injections: **1,389 of 3,535 injected memories
(39.3%) would not have been injected**, against 40.3% by `origin`; 4 memories
on prompts a person sent would have been skipped. In that month none of the
machine-prompt memories in the hand-labelled sample was used, so the expected
cost of the change is the 4, and the saving is four tenths of the injected
tokens plus the slower half of recall latency
([recall-latency-attribution](recall-latency-attribution.md)).
