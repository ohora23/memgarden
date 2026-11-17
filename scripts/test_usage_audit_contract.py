#!/usr/bin/env python3
"""Contract tests for scripts/usage-audit.py, written by the reviewer.

These pin the behaviours the first implementation got wrong: prior text that
leaked from later turns, overwritten assistant records, transcripts that were
never found, sessions merged into one stream, a crashing append, and a
zero-length default window.
"""

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("usage_audit", HERE / "usage-audit.py")
ua = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ua)

INJ = "<memgarden_memories>\n\nCurrent time - x\n\n- {item} [world] (2026-11-01)\n</memgarden_memories>"


def user(text, ts):
    return {"type": "user", "timestamp": ts, "message": {"content": text}}


def inject(item):
    return {"type": "attachment", "attachment": {"type": "hook_additional_context",
                                                  "content": [INJ.format(item=item)]}}


def asst(text=None, tool=None):
    blocks = []
    if text:
        blocks.append({"type": "text", "text": text})
    if tool:
        blocks.append({"type": "tool_use", "input": tool})
    return {"type": "assistant", "message": {"content": blocks}}


def lines(*recs):
    return [json.dumps(r, ensure_ascii=False) for r in recs]


FACT = "the gauge calibration offset is 4.25 millimetres on bench seven"


class Prior(unittest.TestCase):
    def test_prior_never_contains_later_turns(self):
        t = parse(user("hello there", "2026-11-17T00:00:00Z"), inject(FACT), asst("ok"),
                  user("second prompt " + FACT, "2026-11-17T00:01:00Z"))
        self.assertFalse(ua.is_known(t[0]["items"][0], t[0]["prior"]))
        self.assertNotIn("second prompt", t[0]["prior"])

    def test_assistant_text_and_tool_input_earlier_in_session_count(self):
        t = parse(user("first", "2026-11-17T00:00:00Z"), asst("note: " + FACT),
                  user("next", "2026-11-17T00:01:00Z"), inject(FACT), asst("done"))
        self.assertTrue(ua.is_known(t[0]["items"][0], t[0]["prior"]))
        t = parse(user("first", "2026-11-17T00:00:00Z"), asst(tool={"command": "echo " + FACT}),
                  user("next", "2026-11-17T00:01:00Z"), inject(FACT), asst("done"))
        self.assertTrue(ua.is_known(t[0]["items"][0], t[0]["prior"]))

    def test_compact_summary_content_is_prior_and_older_text_is_not(self):
        t = parse(user("first " + FACT, "2026-11-17T00:00:00Z"), asst("x"),
                  {"type": "user", "isCompactSummary": True, "message": {"content": "summary without it"}},
                  user("next", "2026-11-17T00:01:00Z"), inject(FACT), asst("y"))
        turn = [x for x in t if x["items"]][0]
        self.assertFalse(ua.is_known(turn["items"][0], turn["prior"]))
        self.assertIn("summary without it", turn["prior"])
        t = parse(user("first", "2026-11-17T00:00:00Z"),
                  {"type": "user", "isCompactSummary": True, "message": {"content": "summary: " + FACT}},
                  user("next", "2026-11-17T00:01:00Z"), inject(FACT), asst("y"))
        turn = [x for x in t if x["items"]][0]
        self.assertTrue(ua.is_known(turn["items"][0], turn["prior"]))


class Records(unittest.TestCase):
    def test_multiple_assistant_records_accumulate(self):
        t = parse(user("go", "2026-11-17T00:00:00Z"), inject(FACT),
                  asst("part one", {"command": "ls"}), asst("part two", {"command": "pwd"}))
        self.assertIn("part one", ua_resp(t[0]))
        self.assertIn("part two", ua_resp(t[0]))
        self.assertEqual(len(t[0]["tools"]), 2)


class Cli(unittest.TestCase):
    def test_reads_star_star_jsonl_per_session_appends_and_default_window(self):
        now = datetime.now(timezone.utc)
        recent = (now - timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%SZ")
        old = (now - timedelta(days=5)).strftime("%Y-%m-%dT%H:%M:%SZ")
        with tempfile.TemporaryDirectory() as d:
            proj = Path(d) / "projects" / "-home-x-repo"
            proj.mkdir(parents=True)
            # session A says the fact; session B is injected with it -> not known (sessions are separate)
            (proj / "a.jsonl").write_text("\n".join(lines(user("A " + FACT, recent), asst("fine"))) + "\n")
            (proj / "b.jsonl").write_text("\n".join(lines(
                user("B prompt", recent), inject(FACT), asst("ok"),
                user("<task-notification>x</task-notification>", old), inject(FACT), asst("ok"))) + "\n")
            out = Path(d) / "deep" / "audit.jsonl"
            for _ in range(2):
                r = subprocess.run([sys.executable, str(HERE / "usage-audit.py"), "--no-judge",
                                    "--projects", str(Path(d) / "projects"), "--out", str(out)],
                                   capture_output=True, text=True, timeout=60)
                self.assertEqual(r.returncode, 0, r.stderr)
            rows = [json.loads(x) for x in out.read_text().splitlines()]
            self.assertEqual(len(rows), 2)
            row = rows[-1]
            self.assertEqual(row["turns"], 1)          # default window = last 24h, old turn excluded
            self.assertEqual(row["items"], 1)
            self.assertEqual(row["known_items"], 0)    # session A's text must not leak into B
            self.assertEqual(row["automated_turns"], 0)
            self.assertEqual(row["judged_items"], 0)
            self.assertEqual(json.loads(r.stdout.strip().splitlines()[-1]), row)

    def test_judge_samples_only_injected_turns(self):
        turns = parse(user("no injection here", "2026-11-17T00:00:00Z"), asst("a"),
                      user("injected", "2026-11-17T00:01:00Z"), inject(FACT), asst("b"))
        calls = []
        res = ua.audit(turns, judge=lambda t: calls.append(t) or {"0": "I"}, max_turns=1, seed=0)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["prompt"], "injected")
        self.assertEqual(res["judge_irrelevant"], 1)


class InjectionUnit(unittest.TestCase):
    """One injection attachment is one unit, whatever kind of user record preceded it."""

    def test_list_text_prompt_starts_a_turn(self):
        t = parse({"type": "user", "timestamp": "2026-11-17T00:00:00Z",
                   "message": {"content": [{"type": "text", "text": "typed with an image"}]}},
                  inject(FACT), asst("ok"))
        turns = [x for x in t if x["items"]]
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0]["prompt"], "typed with an image")

    def test_two_injections_between_prompts_are_two_units(self):
        other = "a completely different note about the dashboard colour palette update"
        t = parse(user("go", "2026-11-17T00:00:00Z"), inject(FACT), asst("working", {"command": "ls"}),
                  {"type": "user", "message": {"content": [{"type": "tool_result", "content": "x"}]}},
                  {"type": "user", "timestamp": "2026-11-17T00:00:30Z",
                   "message": {"content": [{"type": "text", "text": "queued message"}]}},
                  inject(other), asst("second answer"))
        turns = [x for x in t if x["items"]]
        self.assertEqual(sum(len(x["items"]) for x in turns), 2)
        self.assertEqual(len(turns), 2)
        self.assertIn("second answer", ua_resp(turns[1]))
        self.assertNotIn("second answer", ua_resp(turns[0]))
        self.assertEqual(turns[1]["prompt"], "queued message")

    def test_origin_kind_marks_automated(self):
        rec = user("plain looking text", "2026-11-17T00:00:00Z")
        rec["origin"] = {"kind": "task-notification"}
        peer = {"type": "user", "isMeta": True, "timestamp": "2026-11-17T00:01:00Z",
                "origin": {"kind": "peer"}, "message": {"content": "a peer message"}}
        t = parse(rec, inject(FACT), asst("a"), peer,
                  inject("another fact about the build cache directory and its eviction policy"), asst("b"))
        turns = [x for x in t if x["items"]]
        self.assertEqual(len(turns), 2)
        res = ua.audit(turns)
        self.assertEqual(res["automated_turns"], 2)
        human = parse(user("plain question", "2026-11-17T00:00:00Z"), inject(FACT), asst("a"))
        self.assertEqual(ua.audit(human)["automated_turns"], 0)


class KnownAnchors(unittest.TestCase):
    def test_missing_hash_means_not_known_even_if_words_overlap(self):
        item = "- User completed and committed C6 as 14ea6e6 and the calibration tests passed [world] (2026-09-27)"
        prior = "user completed and committed c6 earlier; the calibration tests passed fine"
        self.assertFalse(ua.is_known(item, prior))
        self.assertTrue(ua.is_known(item, prior + " at 14ea6e6"))

    def test_no_anchor_needs_ninety_percent(self):
        item = "- the reviewer approved the change after reading every file twice"
        self.assertFalse(ua.is_known(item, "the reviewer approved the change after reading"))
        self.assertTrue(ua.is_known(item, "the reviewer approved the change after reading every file twice"))


class ReviewFindings(unittest.TestCase):
    def test_queued_command_is_the_prompt_of_the_injection_it_triggers(self):
        queued = {"type": "attachment", "timestamp": "2026-11-17T00:00:30Z",
                  "attachment": {"type": "queued_command",
                                 "prompt": "<task-notification>job done</task-notification>",
                                 "origin": "{'kind': 'task-notification'}"}}
        t = parse(user("human prompt", "2026-11-17T00:00:00Z"), inject(FACT), asst("a"),
                  queued, inject("another fact about the build cache directory and its eviction policy"), asst("b"))
        self.assertEqual(len(t), 2)
        self.assertEqual(t[1]["prompt"], "<task-notification>job done</task-notification>")
        self.assertTrue(t[1]["automated"])
        self.assertFalse(t[0]["automated"])
        self.assertIn("job done", t[1]["prior"])
        self.assertEqual(t[1]["ts"], "2026-11-17T00:00:30Z")

    def test_human_origin_beats_the_prefix_rule(self):
        rec = user("<command-message>session-handoff</command-message>", "2026-11-17T00:00:00Z")
        rec["origin"] = {"kind": "human"}
        t = parse(rec, inject(FACT), asst("a"))
        self.assertFalse(t[0]["automated"])

    def test_odd_records_do_not_crash(self):
        recs = [{"type": "user", "message": None}, {"type": "assistant", "message": None},
                {"type": "user", "message": "hi"}, {"type": "user", "isCompactSummary": True, "message": None},
                {"type": "user", "timestamp": "2026-11-17T00:00:00Z",
                 "message": {"content": [{"type": "text", "text": None}, {"type": "text", "text": "ok"}]}},
                inject(FACT), {"type": "assistant", "message": {"content": [{"type": "text", "text": None}, "x"]}}]
        t = parse(*recs)
        self.assertEqual(len(t), 1)
        self.assertEqual(t[0]["prompt"], "ok")

    def test_bad_timestamps_are_outside_the_window_not_fatal(self):
        with tempfile.TemporaryDirectory() as d:
            proj = Path(d) / "p" / "s"
            proj.mkdir(parents=True)
            (proj / "x.jsonl").write_text("\n".join(lines(
                user("naive", "2026-11-17T00:00:00"), inject(FACT), asst("a"),
                user("garbage", "garbage"), inject(FACT), asst("b"),
                {"type": "user", "timestamp": 123, "message": {"content": "num"}}, inject(FACT))) + "\n")
            r = subprocess.run([sys.executable, str(HERE / "usage-audit.py"), "--no-judge", "--since", "2026-01-01",
                                "--projects", str(Path(d) / "p"), "--out", str(Path(d) / "o.jsonl")],
                               capture_output=True, text=True, timeout=60)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(json.loads(r.stdout)["turns"], 1)  # only the naive one, read as UTC

    def test_known_matches_whole_tokens_not_substrings(self):
        self.assertFalse(ua.is_known("- apply the new rate limit",
                                     "reapplying thereafter renewal separate limitation"))
        self.assertFalse(ua.is_known("- the offset is 4.25 on bench seven today",
                                     "the offset is 14.250 on bench seven today"))

    def test_judge_keys_outside_the_turn_and_lowercase_labels(self):
        turns = parse(user("q", "2026-11-17T00:00:00Z"), inject(FACT), asst("a"))
        res = ua.audit(turns, judge=lambda t: {"0": "i", "12": "R"})
        self.assertEqual(res["judged_items"], 1)
        self.assertEqual(res["judge_irrelevant"], 1)


def parse(*recs):
    return ua.parse_turns(lines(*recs))


def ua_resp(turn):
    r = turn["resp"]
    return r if isinstance(r, str) else " ".join(r)


if __name__ == "__main__":
    unittest.main()
