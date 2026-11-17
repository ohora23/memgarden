#!/usr/bin/env python3
"""Tests for scripts/usage-audit.py — stdlib only, no network."""

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

# Load usage-audit.py by path (hyphenated filename).
_spec = importlib.util.spec_from_file_location(
    "usage_audit",
    os.path.join(os.path.dirname(__file__), "usage-audit.py"),
)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)

parse_turns = _mod.parse_turns
item_content = _mod.item_content
is_automated = _mod.is_automated
is_known = _mod.is_known
audit = _mod.audit


def _lines(transcript: list[dict]) -> list[str]:
    """Serialize a list of dicts to transcript lines."""
    return [json.dumps(r) for r in transcript]


def _transcript(*records: dict) -> list[str]:
    """Shorthand to build transcript lines from dicts."""
    return _lines(list(records))


# ── item_content ──────────────────────────────────────────────────────────────

class TestItemContent(unittest.TestCase):
    def test_strips_when_metadata(self):
        raw = "- Fix the login bug | When: 2026-10-18"
        self.assertEqual(item_content(raw), "Fix the login bug")

    def test_strips_trailing_world(self):
        raw = "- Update the API docs [world] (2026-10-18)"
        self.assertEqual(item_content(raw), "Update the API docs")

    def test_strips_both(self):
        raw = "- Add rate limiting | When: 2026-10-19 [experience] (2026-10-19 10:00 UTC)"
        self.assertEqual(item_content(raw), "Add rate limiting")

    def test_no_metadata(self):
        raw = "- Simple memory"
        self.assertEqual(item_content(raw), "Simple memory")


# ── is_automated ──────────────────────────────────────────────────────────────

class TestIsAutomated(unittest.TestCase):
    def test_task_notification(self):
        self.assertTrue(is_automated("<task-notification> system check"))

    def test_task_notification_with_leading_whitespace(self):
        self.assertTrue(is_automated("   <task-notification> system check"))

    def test_command_message(self):
        self.assertFalse(is_automated("<command-message> done"))  # typed by a person

    def test_command_name(self):
        self.assertFalse(is_automated("<command-name> npm start"))  # typed by a person

    def test_master(self):
        self.assertTrue(is_automated("[MASTER] deploy"))

    def test_pasted_content(self):
        self.assertFalse(is_automated("<pasted_content> some file"))  # typed by a person

    def test_another_claude_session(self):
        self.assertTrue(is_automated("Another Claude session started"))

    def test_system(self):
        self.assertTrue(is_automated("[SYSTEM] heartbeat"))

    def test_ordinary_prompt(self):
        self.assertFalse(is_automated("Hello, can you help me?"))

    def test_ordinary_prompt_with_whitespace(self):
        self.assertFalse(is_automated("  Hello, can you help me?"))


# ── is_known ──────────────────────────────────────────────────────────────────

class TestIsKnown(unittest.TestCase):
    def test_empty_prior(self):
        self.assertFalse(is_known("- Fix the bug", ""))

    def test_content_already_said(self):
        prior = "the user said fix the login bug earlier in the session"
        item = "- Fix the login bug | When: 2026-10-18"
        self.assertTrue(is_known(item, prior))

    def test_not_mentioned_after_compaction(self):
        # After a compaction summary, prior text is reset.
        # If the item content is not in the new (empty) prior, it is unknown.
        self.assertFalse(is_known("- Some old bug fix | When: 2026-10-18", ""))

    def test_three_tokens_never_known(self):
        # An item with fewer than 4 distinct tokens is never known.
        item = "- Fix bug"  # tokens: "fix", "bug" — only 2
        self.assertFalse(is_known(item, "fix bug somewhere"))


# ── parse_turns ───────────────────────────────────────────────────────────────

class TestParseTurns(unittest.TestCase):
    def _make_user(self, text: str, ts: str = "2026-11-18T10:00:00Z") -> dict:
        return {
            "type": "user",
            "message": {"content": text},
            "timestamp": ts,
            "isMeta": False,
        }

    def _make_assistant(self, text: str) -> dict:
        return {
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": text}]},
        }

    def _make_injection(self, items: list[str]) -> dict:
        content = "<memgarden_memories>\n" + "\n".join(f"- {it}" for it in items) + "\n</memgarden_memories>"
        return {
            "type": "record",
            "attachment": {
                "type": "hook_additional_context",
                "content": content,
            },
        }

    def _make_compaction(self) -> dict:
        return {
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "[Compaction summary]"}]},
            "isCompactSummary": True,
        }

    def _make_sidechain(self) -> dict:
        return {
            "type": "user",
            "message": {"content": "sidechain record"},
            "isSidechain": True,
            "timestamp": "2026-11-18T10:00:00Z",
        }

    def _make_meta(self) -> dict:
        return {
            "type": "user",
            "message": {"content": "meta record"},
            "isMeta": True,
            "timestamp": "2026-11-18T10:00:00Z",
        }

    def test_no_injection(self):
        t = _transcript(
            self._make_user("Hello"),
            self._make_assistant("Hi there!"),
        )
        turns = parse_turns(t)
        self.assertEqual(turns, [])  # the unit is an injection; none here

    def test_no_injection_0_turns_with_items(self):
        """A transcript with no injection and no items → 0 turns with items."""
        t = _transcript(
            self._make_user("Just talk"),
            self._make_assistant("Sure!"),
        )
        turns = parse_turns(t)
        self.assertEqual(turns, [])

    def test_injection_items_parsed(self):
        t = _transcript(
            self._make_user("What do you remember?"),
            self._make_injection(["Fix the login bug", "Update docs"]),
            self._make_assistant("I remember those things."),
        )
        turns = parse_turns(t)
        self.assertEqual(len(turns), 1)
        self.assertEqual(len(turns[0]["items"]), 2)

    def test_injection_as_list(self):
        """Injection content as a list with literal \\n sequences."""
        content_parts = [
            "<memgarden_memories>\n- Fix the login bug\\n- Update docs\n</memgarden_memories>",
        ]
        rec = {
            "type": "record",
            "attachment": {
                "type": "hook_additional_context",
                "content": content_parts,
            },
        }
        t = _transcript(
            self._make_user("What do you remember?"),
            rec,
            self._make_assistant("I remember those things."),
        )
        turns = parse_turns(t)
        self.assertEqual(len(turns[0]["items"]), 2)

    def test_sidechain_ignored(self):
        """A sidechain record should not appear in turns."""
        t = _transcript(
            self._make_user("Hello"),
            self._make_injection(["Fix the login bug"]),
            self._make_sidechain(),
            self._make_assistant("Hi!"),
        )
        turns = parse_turns(t)
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0]["prompt"], "Hello")

    def test_malformed_json_line_ignored(self):
        """Lines that fail json.loads must be skipped."""
        lines = [
            json.dumps({"type": "user", "message": {"content": "Hello"}, "timestamp": "2026-11-18T10:00:00Z", "isMeta": False}),
            "this is not json {{{",
            json.dumps({"type": "attachment", "attachment": {"type": "hook_additional_context", "content": "<memgarden_memories>\n- Fix the login bug\n</memgarden_memories>"}}),
            json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hi"}]}}),
        ]
        turns = parse_turns(lines)
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0]["prompt"], "Hello")

    def test_meta_ignored(self):
        """A meta record should not start a new turn."""
        t = _transcript(
            self._make_user("Hello"),
            self._make_meta(),
            self._make_injection(["Fix the login bug"]),
            self._make_assistant("Hi!"),
        )
        turns = parse_turns(t)
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0]["prompt"], "Hello")

    def test_compaction_resets_prior(self):
        """After an isCompactSummary record, prior text should be empty."""
        t = _transcript(
            self._make_user("First prompt"),
            self._make_injection(["Remember this"]),
            self._make_assistant("I remember."),
            self._make_compaction(),
            self._make_user("Second prompt"),
            self._make_injection(["Remember this"]),
            self._make_assistant("After compaction."),
        )
        turns = parse_turns(t)
        self.assertEqual(len(turns), 2)
        # After compaction, prior for second turn should be just the second prompt
        self.assertIn("Second prompt", turns[1]["prior"])
        self.assertNotIn("First prompt", turns[1]["prior"])


# ── audit ─────────────────────────────────────────────────────────────────────

class TestAudit(unittest.TestCase):
    def _make_turn(self, prompt: str = "Hello",
                   items: list[str] | None = None,
                   resp: str = "Hi",
                   prior: str = "Hello",
                   ts: str = "2026-11-18T10:00:00Z") -> dict:
        return {
            "ts": ts,
            "prompt": prompt,
            "items": items or [],
            "resp": resp,
            "tools": [],
            "prior": prior,
        }

    def test_no_injection_0_turns(self):
        turns = [self._make_turn(items=[])]
        result = audit(turns, max_turns=40, seed=0)
        self.assertEqual(result["turns"], 0)
        self.assertEqual(result["items"], 0)

    def test_judge_failed_items(self):
        """A fake judge returning None (malformed result) counts judge_failed_items."""
        turns = [
            self._make_turn(items=["Fix the bug", "Update docs"]),
        ]

        def bad_judge(turn):
            return None  # simulate parse failure

        result = audit(turns, judge=bad_judge, max_turns=40, seed=0)
        self.assertEqual(result["judge_failed_items"], 2)

    def test_max_turns_limited(self):
        """--max-turns 1 judges exactly one turn."""
        turns = [
            self._make_turn(items=["A"], ts="2026-11-18T10:00:00Z"),
            self._make_turn(items=["B"], ts="2026-11-18T11:00:00Z"),
            self._make_turn(items=["C"], ts="2026-11-18T12:00:00Z"),
        ]

        call_count = 0

        def counting_judge(turn):
            nonlocal call_count
            call_count += 1
            return {"0": "R"}

        result = audit(turns, judge=counting_judge, max_turns=1, seed=42)
        self.assertEqual(call_count, 1)
        self.assertEqual(result["judged_turns"], 1)

    def test_no_judge(self):
        """--no-judge path: judged_items == 0 and no judge call."""
        turns = [
            self._make_turn(items=["Fix the bug"]),
        ]
        result = audit(turns, judge=None, max_turns=40, seed=0)
        self.assertEqual(result["judged_items"], 0)
        self.assertEqual(result["judge_failed_items"], 0)

    def test_judge_irrelevant_count(self):
        turns = [self._make_turn(items=["Fix the bug"])]

        def irrelevant_judge(turn):
            return {"0": "I"}

        result = audit(turns, judge=irrelevant_judge, max_turns=40, seed=0)
        self.assertEqual(result["judge_irrelevant"], 1)

    def test_judge_labels(self):
        turns = [
            self._make_turn(items=["Fix the bug"]),
            self._make_turn(items=["Update docs"]),
        ]

        def mixed_judge(turn):
            if turn["items"][0].startswith("Fix"):
                return {"0": "U"}
            return {"0": "R"}

        result = audit(turns, judge=mixed_judge, max_turns=40, seed=0)
        self.assertEqual(result["judge_labels"].get("U", 0), 1)
        self.assertEqual(result["judge_labels"].get("R", 0), 1)

    def test_automated_turns(self):
        turns = [
            self._make_turn(prompt="<task-notification> check", items=["A"]),
            self._make_turn(prompt="Hello", items=["B"]),
        ]
        result = audit(turns, max_turns=40, seed=0)
        self.assertEqual(result["automated_turns"], 1)
        self.assertEqual(result["automated_items"], 1)

    def test_known_items(self):
        turns = [
            self._make_turn(
                prompt="What do you know?",
                items=["- Fix the login bug | When: 2026-10-18"],
                prior="The user mentioned fix the login bug earlier",
            ),
        ]
        result = audit(turns, max_turns=40, seed=0)
        self.assertEqual(result["known_items"], 1)

    def test_judge_calibration_present(self):
        turns = [self._make_turn(items=["A"])]
        result = audit(turns, max_turns=40, seed=0)
        cal = result.get("judge_calibration", {})
        self.assertEqual(cal.get("date"), "2026-11-18")
        self.assertEqual(cal.get("n"), 129)
        self.assertEqual(cal.get("kappa"), 0.61)
        self.assertEqual(cal.get("irrelevant_bias_pp"), 7)

    def test_version_present(self):
        turns = [self._make_turn(items=["A"])]
        result = audit(turns, max_turns=40, seed=0)
        self.assertEqual(result.get("version"), 1)


# ── integration: full transcript ──────────────────────────────────────────────

class TestIntegration(unittest.TestCase):
    def test_full_transcript_with_injection_and_judge(self):
        """A realistic transcript with injection, response, and fake judge."""
        transcript = [
            {
                "type": "user",
                "message": {"content": "Help me with auth."},
                "timestamp": "2026-11-18T10:00:00Z",
                "isMeta": False,
            },
            {
                "type": "record",
                "attachment": {
                    "type": "hook_additional_context",
                    "content": "<memgarden_memories>\n- Fix the login bug | When: 2026-10-18\n- Update the API [world] (2026-10-18)\n</memgarden_memories>",
                },
            },
            {
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": "Sure, let me check the auth code."}]},
            },
        ]
        lines = [json.dumps(r) for r in transcript]
        turns = parse_turns(lines)
        self.assertEqual(len(turns), 1)
        self.assertEqual(len(turns[0]["items"]), 2)

        def fake_judge(turn):
            return {"0": "R", "1": "U"}

        result = audit(turns, judge=fake_judge, max_turns=40, seed=0)
        self.assertEqual(result["turns"], 1)
        self.assertEqual(result["items"], 2)
        self.assertEqual(result["judged_items"], 2)
        self.assertIn("window_start", result)
        self.assertIn("generated_at", result)


if __name__ == "__main__":
    unittest.main()
