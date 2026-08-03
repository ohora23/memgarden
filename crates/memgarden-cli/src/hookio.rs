//! The hook's stdin payload.
//!
//! One struct for all four events. Claude Code sends a superset per event and
//! adds fields between versions, so every field is `#[serde(default)]` and
//! unknown fields are ignored: a Claude Code upgrade that **adds** a key must
//! never turn into a parse failure, and a parse failure must never turn into
//! anything but exit 0.
//!
//! # `#[serde(default)]` does not cover `null`, and this is the payload where
//! # that matters most
//!
//! **The rule for this crate: a field whose JSON is produced by Claude Code or
//! by `memgardend` is `Option<T>`, not `#[serde(default)] T`.** `default`
//! covers an *absent* key; an explicit `null` against a non-`Option` is a type
//! error that fails the **whole** struct.
//!
//! C4b hit exactly this in `cmd::retain`: the daemon sends `"job_id": null` on
//! every `duplicate` and every `skipped`, a `#[serde(default)] String` refused
//! to parse them, and the two answers the accept table exists for read as
//! transport failures instead — the cursor wedged on the response designed to
//! unwedge it.
//!
//! Here the blast radius is larger and the schema is one Anthropic controls: a
//! future build emitting `"cwd": null` on one event would fail this parse, and
//! **every hook would silently no-op** for anyone on that version. The twelve
//! non-`Option` fields below are a known exposure rather than an oversight;
//! converting them (and adding a null-payload test) is a follow-up, not a C4b
//! change, because it touches every subcommand's field access.
//!
//! Field list from `https://code.claude.com/docs/en/hooks.md`, fetched
//! 2026-08-03.

use serde::{Deserialize, Serialize};

/// Ceiling on how much stdin we will read. The documented payload is a few
/// hundred bytes; `last_assistant_message` is the only field that can be
/// large. This bounds the memory a malformed or hostile writer can make a
/// hook process allocate — an OOM kill exits 137, and while 137 is not 2, it
/// is still a hook that failed for a reason we could have prevented.
pub const MAX_STDIN_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct HookInput {
    // --- common to every event ---
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub prompt_id: String,
    #[serde(default)]
    pub transcript_path: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub permission_mode: String,
    #[serde(default)]
    pub hook_event_name: String,

    // --- per-event ---
    /// `UserPromptSubmit`.
    #[serde(default)]
    pub prompt: String,
    /// The fork accepts both spellings (`recall.py:127`) and so do we; C3
    /// picks whichever is non-empty.
    #[serde(default)]
    pub user_prompt: String,
    /// `SessionStart`: `startup|resume|clear|compact|fork`.
    #[serde(default)]
    pub source: String,
    /// `SessionEnd`: `clear|resume|logout|prompt_input_exit|`
    /// `bypass_permissions_disabled|other`.
    #[serde(default)]
    pub reason: String,
    /// `Stop`.
    #[serde(default)]
    pub last_assistant_message: String,
    /// Reported as a `Stop` field but not found in the docs fetched for the
    /// plan (§Open questions 5). Carried as a default rather than assumed
    /// away; we never block a turn, so nothing reads it yet.
    #[serde(default)]
    pub stop_hook_active: bool,
}

impl HookInput {
    /// `CLAUDE_PROJECT_DIR` when set, else the payload's `cwd`. Set for all
    /// hooks per the docs; preferred because `cwd` follows the model around a
    /// session while the project dir does not.
    pub fn project_dir<'a>(&'a self, claude_project_dir: Option<&'a str>) -> &'a str {
        match claude_project_dir {
            Some(d) if !d.is_empty() => d,
            _ => &self.cwd,
        }
    }
}

/// Parses a payload. `None` for anything we cannot use — empty stdin, invalid
/// JSON, or a JSON value that is not an object. The caller's only correct
/// response is to exit 0 quietly.
pub fn parse(bytes: &[u8]) -> Option<HookInput> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

/// Reads at most [`MAX_STDIN_BYTES`] from stdin and parses it.
pub fn read_stdin() -> Option<HookInput> {
    use std::io::Read;
    let mut buf = Vec::with_capacity(4096);
    std::io::stdin()
        .take(MAX_STDIN_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    parse(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_documented_payload_and_ignores_unknown_fields() {
        let raw = br#"{
            "session_id": "abc",
            "prompt_id": "p1",
            "transcript_path": "/t.jsonl",
            "cwd": "/repo",
            "permission_mode": "default",
            "effort": "high",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "hello",
            "something_claude_code_adds_in_2027": {"nested": true}
        }"#;
        let input = parse(raw).unwrap();
        assert_eq!(input.session_id, "abc");
        assert_eq!(input.hook_event_name, "UserPromptSubmit");
        assert_eq!(input.prompt, "hello");
        // Absent fields are defaults, not errors.
        assert_eq!(input.source, "");
        assert!(!input.stop_hook_active);
    }

    #[test]
    fn unusable_input_is_none_rather_than_an_error() {
        for raw in [
            &b""[..],
            b"   \n\t ",
            b"not json",
            b"{\"session_id\": ",
            // Valid JSON, wrong shape: a bare array cannot be a HookInput.
            b"[1,2,3]",
            b"\"a string\"",
            // A field of the wrong type is still unusable — better to inject
            // nothing than to half-read a payload.
            b"{\"session_id\": 42}",
        ] {
            assert!(parse(raw).is_none(), "{:?} must not parse", raw);
        }
    }

    #[test]
    fn project_dir_prefers_the_env_var_but_not_an_empty_one() {
        let input = HookInput {
            cwd: "/repo/sub".to_string(),
            ..Default::default()
        };
        assert_eq!(input.project_dir(Some("/repo")), "/repo");
        assert_eq!(input.project_dir(Some("")), "/repo/sub");
        assert_eq!(input.project_dir(None), "/repo/sub");
    }
}
