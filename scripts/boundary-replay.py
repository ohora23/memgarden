#!/usr/bin/env python3
"""Offline boundary replay — does a free signal already mark the work boundaries?

This harness answers ONE question, and it is a prerequisite question: **before
building a task-ledger tier, is there anything cheap that tells us when to write
a ledger entry and when to inject one?**

It exists because the deep-research pass (2026-09-01) returned two results that
together forbid the obvious design:

  1. SOTA dialogue topic-boundary detection tops out at F1 0.699 (Def-DTS +
     GPT-4o on TIAGE), falls to 0.53-0.55 on open-weight 70B models, and
     collapses on 7B. A density-matched RANDOM baseline scores 0.664 on the
     same set, so most of that 0.699 is boundary-density alignment rather than
     understanding. No LLM forward pass fits the 10 ms per-turn hook budget
     anyway; the fastest non-LLM segmenter found runs 31.4 ms/dialogue.
  2. Published operating points are NOT transferable: "for a fixed scoring
     model, sweeping the boundary selection threshold produces larger changes
     in W-F1 than switching between structurally distinct segmentation
     methods" (arXiv:2512.17083). Importing anyone's threshold is meaningless.

So the classifier route is closed and the numbers cannot be borrowed. What is
left is to measure OUR OWN corpus with signals that cost nothing, and find out
how much of the problem they already cover. That is all this script does. It
trains nothing, ships nothing, and writes nothing outside its output file.

# Why the unit of analysis is the PROMPT GAP, not the session boundary

The first draft of the handoff design keyed everything on `SessionStart.source`
(compact|resume|clear|fork) and `SessionEnd.reason`. Reading the live
`sessions` table killed that design before a line of it was written:

    n = 15 sessions, all-time
    source      : startup 11 · resume 4     <- 'compact', 'clear', 'fork': NEVER
    end_reason  : prompt_input_exit 8 · NULL 6 · other 1   <- no 'clear', no 'logout'
    lifetime    : 116.7h / 116.0h / 95.7h / 70.2h / 49.7h ...

Sessions here live for DAYS. The 116-hour session is not one task; the user
context-switches many times inside it, and switches to other banks without
ending it. A design that fires on session boundaries would fire about fifteen
times in a month and miss nearly every real handoff.

The boundaries that matter are therefore INTRA-session, and the corpus for them
is the prompt stream: 640 real user prompts across 22 transcripts = 618
intra-session gaps. Every user record already carries `timestamp`, `cwd`,
`gitBranch` and `isSidechain`, so all candidate signals are readable offline at
zero cost.

# The three stages, and why they are separate

    extract  transcripts -> gaps.jsonl     deterministic, no model, re-runnable
    label    gaps.jsonl  -> adds labels    the weak link, kept auditable
    score    gaps.jsonl  -> sweep + dump   never picks a threshold for you

They are separate so that a disagreement about the LABEL never forces a re-parse
of 100 MB of transcript, and so the label column can be overwritten by hand
without touching the signal columns.

# The honest weakness: the label

There is no ground truth for "this gap was a real handoff". `--label` writes a
`heuristic` field from continuation markers ("이어서", "계속", "아까", "continue",
"back to", ...) which is a PROXY and is wrong in both directions: a prompt can
resume prior work without any marker, and a marker can appear inside new work.

Treat `heuristic` as a first cut only. 618 rows is small enough to label
exhaustively, and the vault already records why that matters: on the command-log
cleanup, full census beat sampling (estimated precision 92% vs. measured 89%,
and that gap was six real memories). `--score` reads the `label` column when it
is present and falls back to `heuristic` otherwise, and it always says which one
it used.

# What the census found (2026-09-01, n=347 after 8 interrupt markers excluded)

    continue 291 (83.9%) · switch 33 (9.5%) · resume 23 (6.6%)

    switch  (WRITE)   idle_gap>15m   P 0.133  R 0.697  F1 0.223   (baseline 0.174)
    resume  (INJECT)  every metadata rule at or under baseline 0.124;
                      best was idle_gap>720m at F1 0.130 — a rounding error.
                      resume_phrase                  P 0.476  R 0.870  F1 0.615

Three conclusions, in descending order of how well they are evidenced.

1. **The resume signal is in the prompt text, not in the metadata.** Every gap
   the metadata missed had a tiny time gap (0m, 1m, 5m, 8m, 21m, 29m) and a
   prompt that said so in words. Three were a fixed English resume phrase; the
   rest named an interruption in Korean — a dropped connection, an editor
   freeze, a reboot — and then asked to carry on. Time, branch, cwd and
   compaction are blind to all of it. This half is a clean measurement: those
   four signals were hidden from the labeller.

2. **A literal substring test is enough, and it fits the budget.** No forward
   pass, no model, microseconds. The research forbids LLM classification in a
   10 ms hook; it says nothing against a regex. Its F1 0.615 is FIVE TIMES the
   baseline, and on `switch` it drops to 0.080 — it discriminates the right
   event rather than firing on change in general. Read the circularity warning
   at its definition before quoting the recall.

3. **Do not build a write trigger.** Nothing predicts `switch` with usable
   precision (best 0.133 at 70% recall), but writing is off-turn and cheap, so
   always-fire is already the right rule at R=1.0 and zero cost. The trigger
   worth having is on the READ side, and finding 1 says what it is.

Nothing here proves the ledger pays for itself. That is a separate A/B with a
task-completion metric, and the local prior is discouraging: MX-3 measured
memory as an 11-7 LOSS at +5% tokens on its sample. A recall@k win does not
imply a carryover win.

Usage:
    scripts/boundary-replay.py extract [--out gaps.jsonl] [--projects DIR]
    scripts/boundary-replay.py label   [--io  gaps.jsonl]
    scripts/boundary-replay.py score   [--in  gaps.jsonl] [--dump-uncovered N]
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import glob
import json
import os
import re
import sys

PROJECTS = os.path.expanduser("~/.claude/projects")
DEFAULT_OUT = "gaps.jsonl"

# Prompt text is carried only so the label stage and the human audit have
# something to read. It is never a signal: anything derived from prompt CONTENT
# is exactly the LLM-shaped work this harness exists to avoid.
PROMPT_CHARS = 240


# --------------------------------------------------------------------------
# stage 1: extract
# --------------------------------------------------------------------------

def user_text(rec: dict) -> str | None:
    """The prompt a human actually typed, or None for everything else.

    Three things wear `type == "user"` in a transcript and only one of them is a
    person: real prompts, tool results echoed back into the turn, and sidechain
    traffic from subagents. In the largest transcript the split is 1016
    tool_result records against 50 real prompts, so filtering wrongly here
    changes the corpus by a factor of twenty.
    """
    if rec.get("type") != "user" or rec.get("isSidechain"):
        return None
    # The post-compact summary ("This session is being continued from a previous
    # conversation...") arrives wearing type "user" but nobody typed it. The
    # first run of this harness counted 6 of them as prompts, which put a fake
    # gap on both sides of every compaction AND tripped the continuation proxy
    # on the literal word "continued" — a false positive manufactured by the
    # measurement. It is the most reliable compact marker in the file, so it is
    # dropped here and re-read as one in `extract`.
    if rec.get("isCompactSummary"):
        return None
    content = rec.get("message", {}).get("content")
    if isinstance(content, list):
        if any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
            return None
        content = "".join(b.get("text", "") for b in content if isinstance(b, dict))
    if not isinstance(content, str) or not content.strip():
        return None
    return content


# Slash-command echoes and their stdout arrive as user records but are not
# prompts. The vault already paid for this once: command logs were 2% of stored
# nodes but 22% of injected context before 158 of them were deleted.
# Not everything wearing `type: "user"` was typed by a user. Slash-command
# echoes, `!`-prefixed bash and its output, skill preambles and background
# task notifications all arrive on the user turn. In this corpus they were
# **31% of gaps** (164 of 524), and task notifications alone were 144 — left
# in, they put a machine-authored "prompt" on both sides of every background
# job and would have dominated the census.
#
# The vault already records this class of error once: command logs were 2% of
# stored nodes but 22% of injected context before 158 of them were deleted.
NOT_A_PROMPT = re.compile(
    r"<(command-name|command-message|local-command-stdout|local-command-caveat"
    r"|task-notification|bash-input|bash-stdout|bash-stderr|system-reminder)>"
    r"|^\s*/[a-z][\w:-]*\s*$"
    r"|^Base directory for this skill:"
)


def parse_ts(s: str) -> float | None:
    try:
        return dt.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    except (ValueError, AttributeError):
        return None


def extract(projects: str, out_path: str) -> None:
    files = sorted(glob.glob(os.path.join(projects, "*", "*.jsonl")))
    rows: list[dict] = []
    stats = collections.Counter()

    for path in files:
        prompts: list[dict] = []
        # Compact boundaries are their own record type and sit BETWEEN prompts,
        # so they are collected as timestamps and attributed to whichever gap
        # they fall inside rather than to a prompt.
        compacts: list[float] = []

        for line in open(path, errors="replace"):
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                stats["malformed_lines"] += 1
                continue

            # Two records mark a compaction and they do not always agree on
            # ordering, so both are collected. The bare `/compact` a user types
            # also lands as a normal prompt with no command wrapper, which is
            # why COMMAND_ECHO alone does not catch it.
            if (rec.get("type") == "system" and rec.get("subtype") == "compact_boundary") \
                    or rec.get("isCompactSummary"):
                ts = parse_ts(rec.get("timestamp", ""))
                if ts:
                    compacts.append(ts)
                continue

            text = user_text(rec)
            if text is None:
                continue
            if NOT_A_PROMPT.search(text):
                stats["not_a_prompt_dropped"] += 1
                continue
            ts = parse_ts(rec.get("timestamp", ""))
            if ts is None:
                stats["no_timestamp"] += 1
                continue
            # The same prompt can appear twice in a row — a queued send, a
            # retry, a re-render. Two identical adjacent texts are one prompt
            # in a transcript artifact's clothing, and left in they manufacture
            # a zero-length "gap" that is trivially a continuation. 22 of the
            # first run's 555 rows were this.
            if prompts and prompts[-1]["text"].strip() == text.strip():
                stats["adjacent_duplicates_dropped"] += 1
                continue
            prompts.append(
                {
                    "ts": ts,
                    "text": text,
                    "cwd": rec.get("cwd", ""),
                    "branch": rec.get("gitBranch", ""),
                }
            )

        stats["files"] += 1
        stats["prompts"] += len(prompts)
        stats["compact_boundaries"] += len(compacts)

        session = os.path.basename(path)[:8]
        bank = os.path.basename(os.path.dirname(path))

        for i in range(1, len(prompts)):
            prev, cur = prompts[i - 1], prompts[i]
            rows.append(
                {
                    "bank": bank,
                    "session": session,
                    "idx": i,
                    "gap_s": round(cur["ts"] - prev["ts"], 1),
                    "branch_changed": prev["branch"] != cur["branch"],
                    "cwd_changed": prev["cwd"] != cur["cwd"],
                    "compact_between": any(prev["ts"] < c <= cur["ts"] for c in compacts),
                    "branch": cur["branch"],
                    "prev_text": prev["text"][:PROMPT_CHARS],
                    "next_text": cur["text"][:PROMPT_CHARS],
                }
            )

    with open(out_path, "w") as fh:
        for row in rows:
            fh.write(json.dumps(row, ensure_ascii=False) + "\n")

    print(f"wrote {len(rows)} gaps -> {out_path}")
    for key, value in sorted(stats.items()):
        print(f"  {key:24} {value}")

    fired = collections.Counter()
    for row in rows:
        for signal in ("branch_changed", "cwd_changed", "compact_between"):
            fired[signal] += bool(row[signal])
    print("\n  signal firing counts (before any threshold):")
    for signal, count in fired.most_common():
        pct = 100.0 * count / len(rows) if rows else 0.0
        print(f"    {signal:18} {count:5d}  ({pct:.1f}% of gaps)")


# --------------------------------------------------------------------------
# stage 2: label
# --------------------------------------------------------------------------

# Markers that a prompt leans on context the model no longer has. Korean first
# because that is what this corpus is written in. Deliberately NOT tuned: a
# tuned proxy would smuggle in the threshold-fitting that `score` exists to keep
# explicit and visible.
CONTINUATION = re.compile(
    r"(이어서|계속|아까|그거|그건|아까그|다시\s|방금|위에서|앞에서|하던|남은|나머지"
    r"|continue|resume|back to|as before|earlier|the rest|where we left|still)",
    re.IGNORECASE,
)


def label(io_path: str) -> None:
    rows = [json.loads(line) for line in open(io_path)]
    for row in rows:
        row["heuristic"] = bool(CONTINUATION.search(row["next_text"]))
    with open(io_path, "w") as fh:
        for row in rows:
            fh.write(json.dumps(row, ensure_ascii=False) + "\n")

    positives = sum(r["heuristic"] for r in rows)
    print(f"labelled {len(rows)} gaps -> {io_path}")
    print(f"  heuristic positives: {positives} ({100.0*positives/len(rows):.1f}%)")
    print(
        "\n  NOTE: `heuristic` is a proxy and is wrong in both directions.\n"
        "  618 rows is small enough to label by hand; add a `label` field\n"
        "  (true/false) to any row and `score` will prefer it over `heuristic`."
    )


# --------------------------------------------------------------------------
# stage 3: score
# --------------------------------------------------------------------------

# Swept, not chosen. The research finding that forbids importing a threshold
# applies just as much to inventing one here.
GAP_THRESHOLDS_S = [300, 900, 1800, 3600, 7200, 21600, 43200, 86400]


def confusion(rows: list[dict], fired, truth) -> tuple[int, int, int, int]:
    tp = sum(1 for r in rows if fired(r) and truth(r))
    fp = sum(1 for r in rows if fired(r) and not truth(r))
    fn = sum(1 for r in rows if not fired(r) and truth(r))
    tn = sum(1 for r in rows if not fired(r) and not truth(r))
    return tp, fp, fn, tn


def prf(tp: int, fp: int, fn: int) -> tuple[float, float, float]:
    precision = tp / (tp + fp) if tp + fp else 0.0
    recall = tp / (tp + fn) if tp + fn else 0.0
    f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
    return precision, recall, f1


def score(in_path: str, dump_uncovered: int) -> None:
    """Score every free signal against BOTH targets, separately.

    The first run of this harness scored boundary detectors against a
    continuity label and concluded, wrongly, that nothing beat the trivial
    baseline. `branch_changed` and `idle_gap` fire when something CHANGED;
    the continuation proxy is true when work CARRIED ON. They are
    anti-correlated by construction, so that comparison could only ever
    produce a null.

    The two events a ledger cares about are genuinely different and need
    separate scores:

        switch  work ends / a different task starts  -> WRITE the ledger
        resume  the same task restarts after a break -> INJECT the ledger

    `resume` is the money case: those gaps, and only those, are where an
    injected ledger would have saved rework. `switch` is where injecting a
    stale ledger would actively hurt — the survey's "false continuity".
    """
    rows = [json.loads(line) for line in open(in_path)]
    if not rows:
        sys.exit("no rows; run `extract` first")

    labelled = [r for r in rows if r.get("label") in ("continue", "switch", "resume")]
    if labelled:
        rows = labelled
        dist = collections.Counter(r["label"] for r in rows)
        print(f"corpus: {len(rows)} gaps · label source = census (3-way)")
        print("        " + " · ".join(f"{k}={v} ({100.0*v/len(rows):.1f}%)"
                                      for k, v in dist.most_common()))
        targets = [("switch  (WRITE trigger)", lambda r: r["label"] == "switch"),
                   ("resume  (INJECT trigger)", lambda r: r["label"] == "resume")]
    else:
        if "heuristic" not in rows[0]:
            sys.exit("no labels; run `label` first, or add a 3-way `label` field")
        print(f"corpus: {len(rows)} gaps · label source = heuristic PROXY")
        print("        WARNING: the proxy marks continuation, while every rule below")
        print("        detects change. Expect a null. Run the census for a real answer.")
        targets = [("heuristic continuation", lambda r: bool(r["heuristic"]))]

    # A literal substring test on the incoming prompt. This is NOT the LLM
    # classification the research rules out — no forward pass, no model, a few
    # microseconds inside the 10 ms hook budget — so it belongs in the same
    # table as the metadata signals.
    #
    # ** CIRCULARITY WARNING, read before citing this row. ** The census
    # labelled `resume` partly by reading resumption language, so a rule that
    # matches resumption language shares a source with its own ground truth.
    # Its score is an UPPER BOUND on what the phrase test can do, not an
    # estimate of it. The metadata rules carry no such defect: gap, branch, cwd
    # and compaction were hidden from the labeller (the worksheet is blind),
    # so their failure is a real measurement and this row's success is not.
    RESUME_PHRASE = re.compile(
        r"continue from where|이어서|이어가|계속 진행|다시 시작"
        r"|끊어졌|프리징|freezing|재부팅|복구하고|핸드오프|아까 말한|남은 ?작업",
        re.IGNORECASE,
    )

    rules: list[tuple[str, object]] = [
        ("branch_changed", lambda r: r["branch_changed"]),
        ("cwd_changed", lambda r: r["cwd_changed"]),
        ("compact_between", lambda r: r["compact_between"]),
        ("resume_phrase (see warning)", lambda r: bool(RESUME_PHRASE.search(r["next_text"]))),
    ]
    for seconds in GAP_THRESHOLDS_S:
        rules.append(
            (f"idle_gap > {seconds//60}m", (lambda s: lambda r: r["gap_s"] > s)(seconds))
        )

    def union(r: dict) -> bool:
        return bool(r["branch_changed"] or r["cwd_changed"]
                    or r["compact_between"] or r["gap_s"] > 1800)

    for target_name, truth in targets:
        positives = sum(truth(r) for r in rows)
        if not positives:
            print(f"\n  === {target_name} === no positives, skipped")
            continue
        base = 100.0 * positives / len(rows)
        print(f"\n  === {target_name} ===  positives={positives}  base rate={base:.1f}%")

        # A rule that cannot beat firing on everything is measuring boundary
        # density, not boundaries. Def-DTS clears a density-matched random
        # baseline on TIAGE by only ~0.035, which is how little that margin
        # can be even at the published state of the art.
        p, r_, f = prf(*confusion(rows, lambda _: True, truth)[:3])
        print(f"  {'BASELINE always-fire':30} {p:6.3f} {r_:6.3f} {f:6.3f} {len(rows):7d}")
        print("  " + "-" * 60)

        for name, fired in rules + [("UNION (any signal, gap>30m)", union)]:
            tp, fp, fn, _ = confusion(rows, fired, truth)
            p, r_, f = prf(tp, fp, fn)
            flag = "  <-- beats baseline" if f > prf(*confusion(
                rows, lambda _: True, truth)[:3])[2] else ""
            print(f"  {name:30} {p:6.3f} {r_:6.3f} {f:6.3f} {tp+fp:7d}{flag}")

        tp, fp, fn, _ = confusion(rows, union, truth)
        print(f"\n  uncovered = {fn} of {positives} ({100.0*fn/positives:.1f}%) "
              f"— {target_name.split()[0]} gaps no free signal caught")

        if dump_uncovered and "INJECT" in target_name:
            missed = [r for r in rows if truth(r) and not union(r)][:dump_uncovered]
            print(f"\n  --- {len(missed)} uncovered (audit the label here) ---")
            for r in missed:
                print(f"\n  [{r['bank'][:28]}/{r['session']} #{r['idx']}] "
                      f"gap={r['gap_s']/60:.0f}m")
                print(f"    prev: {r['prev_text'][:100]!r}")
                print(f"    next: {r['next_text'][:100]!r}")


# --------------------------------------------------------------------------

def main() -> None:
    ap = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)

    e = sub.add_parser("extract", help="transcripts -> gaps.jsonl")
    e.add_argument("--projects", default=PROJECTS)
    e.add_argument("--out", default=DEFAULT_OUT)

    l = sub.add_parser("label", help="add the heuristic proxy label in place")
    l.add_argument("--io", default=DEFAULT_OUT)

    s = sub.add_parser("score", help="sweep thresholds and dump what nothing covers")
    s.add_argument("--in", dest="in_path", default=DEFAULT_OUT)
    s.add_argument("--dump-uncovered", type=int, default=10)

    args = ap.parse_args()
    if args.cmd == "extract":
        extract(args.projects, args.out)
    elif args.cmd == "label":
        label(args.io)
    else:
        score(args.in_path, args.dump_uncovered)


if __name__ == "__main__":
    main()
