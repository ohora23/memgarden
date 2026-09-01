//! Server-side transcript normalization: memory-tag stripping, the two fork
//! tool-input caps, the `tool_result` cap, the initial-backfill cap, and
//! `file:` tag extraction.
//!
//! All of this is client-side in the hindsight fork
//! (`hindsight-integrations/claude-code/scripts/lib/content.py`,
//! `scripts/retain.py`). MemGarden moves it into the daemon (plan decision
//! #4) for two reasons: the `retain_cap_saving` ledger row is a store
//! concern, and the PRD budgets the Phase C hook at <10ms total — so the
//! hook posts a raw transcript and does nothing else.
//!
//! Two deliberate non-ports, both from `content.py` and both out of §PR B3's
//! file list:
//!   * `_is_channel_message_tool` / `_MESSAGE_TEXT_FIELDS` — extracting an
//!     outgoing Telegram/Slack message out of an MCP `tool_use` block. No
//!     channel plugin is wired into this system; such blocks are retained as
//!     ordinary (capped) tool calls instead of being re-labelled as text.
//!   * the last-turn slicing branch of `prepare_retention_transcript`
//!     (`retain_full_window = False`). Which messages are new is the hook's
//!     delta bookkeeping (Phase C); the daemon retains exactly what it is
//!     sent, capped.

use serde_json::{Map, Value, json};

/// legacy fork: `lib/content.py:413`.
pub const TOOL_INPUT_FIELD_MAX: usize = 300;
/// legacy fork: `lib/content.py:417`.
pub const TOOL_INPUT_TOTAL_MAX: usize = 1500;
/// legacy fork: `lib/content.py:299-300`.
pub const TOOL_RESULT_MAX: usize = 2000;

/// Keys carrying the most durable signal about a tool call; when the
/// serialized whole busts the total budget, only these survive.
/// legacy fork: `lib/content.py:421-430`.
pub const PRIORITY_INPUT_KEYS: [&str; 8] = [
    "file_path",
    "notebook_path",
    "path",
    "command",
    "description",
    "pattern",
    "query",
    "url",
];

/// Tool base names whose calls modify a file. Read-only tools (Read, Glob,
/// Grep) are deliberately excluded — tagging every file a session merely
/// looked at would drown the signal of what it changed.
/// legacy fork: `lib/content.py:472`.
const FILE_MUTATING_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// Substring stand-in for the fork's `_OPERATIONAL_TOOL_PATTERN` regex
/// (`lib/content.py:528-531`), matched case-insensitively against the last
/// `__`-separated segment of an `mcp__…` tool name. This is what keeps
/// MemGarden's own MCP calls out of the transcript it retains — without it,
/// every recall injection feeds itself back in (`lib/content.py:278-279`).
const OPERATIONAL_TOOL_MARKERS: [&str; 10] = [
    "recall", "retain", "reflect", "search", "extract", "create_", "delete_", "update_", "get_",
    "list_",
];

/// Memory-injection wrappers stripped before retaining. `hindsight_memories`
/// matters during the parallel-run transition: without it we re-retain our
/// predecessor's injections (`lib/content.py:47-48`).
const MEMORY_TAGS: [&str; 3] = [
    "memgarden_memories",
    "hindsight_memories",
    "relevant_memories",
];

/// The size caps applied while normalizing. `Caps::none()` produces the
/// uncapped baseline the `retain_cap_saving` ledger measures against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub tool_input_field_max: usize,
    pub tool_input_total_max: usize,
    pub tool_result_max: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            tool_input_field_max: TOOL_INPUT_FIELD_MAX,
            tool_input_total_max: TOOL_INPUT_TOTAL_MAX,
            tool_result_max: TOOL_RESULT_MAX,
        }
    }
}

impl Caps {
    /// Every cap saturated: normalization runs identically but truncates
    /// nothing. `usize::MAX` rather than an `Option` so the comparisons stay
    /// branch-free and the two passes share one code path — the raw and
    /// capped token counts must differ ONLY by the caps.
    pub fn none() -> Self {
        Caps {
            tool_input_field_max: usize::MAX,
            tool_input_total_max: usize::MAX,
            tool_result_max: usize::MAX,
        }
    }
}

pub struct NormalizeOpts<'a> {
    /// Roles to retain; the fork's default is `["user", "assistant"]`.
    pub roles: &'a [String],
    /// JSON transcript (tool calls included) vs. the legacy
    /// `[role: x]…[x:end]` text format. The `coding` profile turns this on.
    pub include_tool_calls: bool,
    pub caps: Caps,
}

/// Initial-backfill cap: keeps the **last** `max_initial` messages, and only
/// on a session's first retain. `0` disables it; delta retains are never
/// touched. legacy fork: `retain.py:141-147`, `lib/config.py:40`.
///
/// This exists because a 102MB legacy transcript blew the server's retain
/// wall-clock limit (observed: 3600s exceeded -> batch cancelled). Recent
/// context is what recall needs.
pub fn apply_backfill_cap(messages: &[Value], is_initial: bool, max_initial: usize) -> &[Value] {
    if !is_initial || max_initial == 0 || messages.len() <= max_initial {
        return messages;
    }
    &messages[messages.len() - max_initial..]
}

/// Formats `messages` into the transcript actually handed to the extractor.
/// Returns `None` (nothing worth retaining) for an empty message set or a
/// transcript under 10 trimmed characters — legacy fork:
/// `content.py:371-373, 399-401`.
pub fn normalize(messages: &[Value], opts: &NormalizeOpts) -> Option<(String, usize)> {
    if messages.is_empty() {
        return None;
    }
    if opts.include_tool_calls {
        json_transcript(messages, opts)
    } else {
        text_transcript(messages, opts)
    }
}

fn role_allowed(msg: &Value, roles: &[String]) -> Option<String> {
    let role = msg
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    roles.contains(&role).then_some(role)
}

/// legacy fork: `_prepare_json_transcript`, `content.py:351-374`.
fn json_transcript(messages: &[Value], opts: &NormalizeOpts) -> Option<(String, usize)> {
    let mut structured: Vec<Value> = Vec::new();
    for msg in messages {
        let Some(role) = role_allowed(msg, opts.roles) else {
            continue;
        };
        let blocks = message_blocks(
            msg.get("content").unwrap_or(&Value::Null),
            &role,
            &opts.caps,
        );
        if blocks.is_empty() {
            continue;
        }
        structured.push(json!({ "role": role, "content": blocks }));
    }
    if structured.is_empty() {
        return None;
    }
    let count = structured.len();
    let transcript = serde_json::to_string(&Value::Array(structured)).ok()?;
    (transcript.trim().chars().count() >= 10).then_some((transcript, count))
}

/// legacy fork: `_prepare_text_transcript`, `content.py:377-402`.
fn text_transcript(messages: &[Value], opts: &NormalizeOpts) -> Option<(String, usize)> {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        let Some(role) = role_allowed(msg, opts.roles) else {
            continue;
        };
        let raw = text_content(msg.get("content").unwrap_or(&Value::Null));
        let content = strip_channel_envelope(&strip_memory_tags(&strip_private(&raw)))
            .trim()
            .to_string();
        if content.is_empty() {
            continue;
        }
        parts.push(format!("[role: {role}]\n{content}\n[{role}:end]"));
    }
    if parts.is_empty() {
        return None;
    }
    let count = parts.len();
    let transcript = parts.join("\n\n");
    (transcript.trim().chars().count() >= 10).then_some((transcript, count))
}

/// Text-only extraction for the non-tool-call format: `text` blocks joined by
/// newlines. `thinking`, `tool_use` and `tool_result` are excluded — legacy
/// fork: `_extract_text_content`, `content.py:565-608` (minus the channel-
/// message branch, see the module doc).
fn text_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// legacy fork: `_extract_message_blocks`, `content.py:239-303`.
fn message_blocks(content: &Value, role: &str, caps: &Caps) -> Vec<Value> {
    if let Value::String(s) = content {
        let cleaned = strip_channel_envelope(&strip_memory_tags(&strip_private(s)))
            .trim()
            .to_string();
        return if cleaned.is_empty() {
            vec![]
        } else {
            vec![json!({ "type": "text", "text": cleaned })]
        };
    }
    let Value::Array(items) = content else {
        return vec![];
    };

    let mut blocks = Vec::new();
    for block in items {
        let Some(obj) = block.as_object() else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let raw = obj.get("text").and_then(Value::as_str).unwrap_or("");
                let text = strip_channel_envelope(&strip_memory_tags(&strip_private(raw)))
                    .trim()
                    .to_string();
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            // legacy fork: `content.py:265` — tool calls only from the
            // assistant. A `tool_use` block on a user message is malformed
            // input, not something to retain.
            "tool_use" if role == "assistant" => {
                let name = obj.get("name").and_then(Value::as_str).unwrap_or("unknown");
                if is_operational_mcp_tool(name) {
                    continue;
                }
                let input = obj.get("input").unwrap_or(&Value::Null);
                blocks.push(json!({
                    "type": "tool_use",
                    "name": name,
                    "input": compact_tool_input(input, caps),
                }));
            }
            "tool_result" => {
                let raw = obj.get("content").unwrap_or(&Value::Null);
                let text = match raw {
                    Value::String(s) => s.trim().to_string(),
                    // Agent results arrive as a content-block array.
                    Value::Array(items) => items
                        .iter()
                        .filter(|i| i.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|i| i.get("text").and_then(Value::as_str))
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                if text.is_empty() {
                    continue;
                }
                blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": obj.get("tool_use_id").and_then(Value::as_str).unwrap_or(""),
                    "content": truncate_chars(&text, caps.tool_result_max, "... (truncated)"),
                }));
            }
            _ => {}
        }
    }
    blocks
}

fn is_operational_mcp_tool(name: &str) -> bool {
    if !name.starts_with("mcp__") {
        return false;
    }
    let suffix = name
        .rsplit("__")
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    OPERATIONAL_TOOL_MARKERS.iter().any(|m| suffix.contains(m))
}

/// The two-tier `tool_use` input cap, ported from `_compact_tool_input`
/// (`lib/content.py:433-462`).
///
/// Tier 1: every **string** field longer than `tool_input_field_max` becomes
/// `value[:max] + "... (+N chars)"`. Non-strings are left alone — a `Write`
/// of a whole file was previously retained verbatim and dominated the payload.
/// Tier 2: if the serialized whole still exceeds `tool_input_total_max`, keep
/// only the priority keys plus `_truncated_fields` listing what was dropped
/// (this is the MultiEdit / many-fields case).
///
/// Two documented divergences from the Python original, neither behavioral in
/// practice:
///   * `serde_json` emits compact separators where Python's `json.dumps`
///     defaults to `", "` / `": "`, so our serialized length runs a couple of
///     chars per key shorter — the effective total budget is marginally more
///     permissive. Tier 1 does the heavy lifting; the exact 1500 boundary is
///     not load-bearing.
///   * `serde_json::Map` is a BTreeMap, so surviving keys and the
///     `_truncated_fields` list come out sorted rather than in insertion
///     order.
pub fn compact_tool_input(input: &Value, caps: &Caps) -> Value {
    // Non-dict input -> {} (legacy `content.py:441-442`).
    let Some(map) = input.as_object() else {
        return Value::Object(Map::new());
    };

    let mut compact = Map::new();
    for (key, value) in map {
        match value {
            Value::String(s) if s.chars().count() > caps.tool_input_field_max => {
                let total = s.chars().count();
                let head: String = s.chars().take(caps.tool_input_field_max).collect();
                let dropped = total - caps.tool_input_field_max;
                compact.insert(
                    key.clone(),
                    Value::String(format!("{head}... (+{dropped} chars)")),
                );
            }
            other => {
                compact.insert(key.clone(), other.clone());
            }
        }
    }

    let serialized_len = serde_json::to_string(&Value::Object(compact.clone()))
        .map(|s| s.chars().count())
        .unwrap_or(usize::MAX);
    if serialized_len <= caps.tool_input_total_max {
        return Value::Object(compact);
    }

    let mut kept = Map::new();
    for key in PRIORITY_INPUT_KEYS {
        if let Some(v) = compact.get(key) {
            kept.insert(key.to_string(), v.clone());
        }
    }
    let dropped: Vec<Value> = compact
        .keys()
        .filter(|k| !kept.contains_key(*k))
        .map(|k| Value::String(k.clone()))
        .collect();
    if !dropped.is_empty() {
        kept.insert("_truncated_fields".to_string(), Value::Array(dropped));
    }
    Value::Object(kept)
}

/// Unique file paths modified by tool calls, in first-touch order. Paths
/// under `cwd` are relativized so tags stay stable across machines and
/// checkout locations; paths outside `cwd` stay absolute. legacy fork:
/// `extract_touched_files`, `lib/content.py:475-513`.
///
/// The caller applies `retain.file_tag_cap` (20) — legacy does the same at
/// its own call site (`retain.py:237-241`).
/// Paths come straight from untrusted tool input and become node tags, so
/// bound them: anything absurdly long or carrying control characters is
/// dropped rather than tagged (security review).
const MAX_FILE_PATH_CHARS: usize = 512;

pub fn extract_touched_files(messages: &[Value], cwd: &str) -> Vec<String> {
    let cwd_prefix = if cwd.is_empty() {
        String::new()
    } else {
        format!("{}/", cwd.trim_end_matches('/'))
    };

    let mut files: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(Value::Array(blocks)) = msg.get("content") else {
            continue;
        };
        for block in blocks {
            let Some(obj) = block.as_object() else {
                continue;
            };
            if obj.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            // Base name handles both plain tools ("Edit") and namespaced
            // variants ("mcp__someserver__Edit") — `content.py:497`.
            let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
            let base = name.rsplit("__").next().unwrap_or(name);
            if !FILE_MUTATING_TOOLS.contains(&base) {
                continue;
            }
            let Some(input) = obj.get("input").and_then(Value::as_object) else {
                continue;
            };
            let raw = input
                .get("file_path")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    input
                        .get("notebook_path")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                });
            let Some(raw) = raw else { continue };
            let path = raw.trim();
            if path.chars().count() > MAX_FILE_PATH_CHARS || path.chars().any(|c| c.is_control()) {
                continue;
            }
            let path = match path.strip_prefix(cwd_prefix.as_str()) {
                Some(rel) if !cwd_prefix.is_empty() => rel,
                _ => path,
            };
            // Vec keeps first-touch order; the set keeps the dedup O(1).
            if seen.insert(path.to_string()) {
                files.push(path.to_string());
            }
        }
    }
    files
}

/// The opt-out marker a user can put around anything they do not want
/// remembered. Borrowed from `claude-mem`, which is the only idea in it this
/// project did not already have.
const PRIVATE_TAG_OPEN: &str = "<private>";
const PRIVATE_TAG_CLOSE: &str = "</private>";

/// Removes `<private>…</private>` from text before anything reads it.
///
/// **Not `remove_blocks`, and the difference is the whole point.** That helper
/// leaves an unterminated block in place, which is the right conservative
/// default for a memory-injection wrapper the daemon itself emitted: dropping
/// the rest of a transcript over a malformed tag would lose real material.
/// Here the conservative direction is the opposite one. An unclosed
/// `<private>` is a user who started saying something they did not want kept —
/// they may have been interrupted, or the transcript may have been cut at a
/// delta boundary mid-message — so **everything from the marker to the end of
/// that message is dropped**. Failing open would store the secret.
///
/// Known limits, because a redaction control that overstates itself is worse
/// than none:
///
/// * Message text only. Text inside a `tool_use` input or a `tool_result` is
///   not user-authored prose and is not scanned; the caps in `Caps` are what
///   bound those.
/// * Exact, lower-case `<private>`. No attributes, no `<PRIVATE>`, no
///   whitespace inside the tag. A marker that half-works is worse than one
///   with a stated shape.
/// * Retain-time only. It keeps the text out of extraction and therefore out
///   of the store; it cannot remove what a previous retain already wrote.
pub fn strip_private(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find(PRIVATE_TAG_OPEN) {
        out.push_str(&rest[..start]);
        let after_open = start + PRIVATE_TAG_OPEN.len();
        match rest[after_open..].find(PRIVATE_TAG_CLOSE) {
            Some(rel_end) => {
                rest = &rest[after_open + rel_end + PRIVATE_TAG_CLOSE.len()..];
            }
            // Unterminated: everything after the marker is discarded.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Removes `<tag>…</tag>` blocks for all three memory-injection wrappers,
/// non-greedy (nearest closing tag), matching the Python `[\s\S]*?` regex.
pub fn strip_memory_tags(content: &str) -> String {
    let mut out = content.to_string();
    for tag in MEMORY_TAGS {
        out = remove_blocks(&out, &format!("<{tag}>"), &format!("</{tag}>"));
    }
    out
}

fn remove_blocks(text: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        let after_open = start + open.len();
        let Some(rel_end) = rest[after_open..].find(close) else {
            break; // unterminated block: leave the remainder untouched
        };
        out.push_str(&rest[..start]);
        rest = &rest[after_open + rel_end + close.len()..];
    }
    out.push_str(rest);
    out
}

/// Extracts the inner text of a `<channel …>…</channel>` wrapper Claude Code
/// puts around incoming channel messages. legacy fork:
/// `strip_channel_envelope`, `lib/content.py:20-36`.
pub fn strip_channel_envelope(content: &str) -> String {
    let Some(start) = find_channel_open(content) else {
        return content.to_string();
    };
    let Some(gt) = content[start..].find('>').map(|i| start + i + 1) else {
        return content.to_string();
    };
    let Some(end) = content[gt..].find("</channel>").map(|i| gt + i) else {
        return content.to_string();
    };
    content[gt..end].trim().to_string()
}

/// `<channel` followed by a word boundary (the regex's `\b`), so
/// `<channels>` is not a match.
fn find_channel_open(content: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = content[from..].find("<channel") {
        let at = from + rel;
        let next = content[at + "<channel".len()..].chars().next();
        match next {
            Some(c) if c.is_alphanumeric() || c == '_' => from = at + "<channel".len(),
            _ => return Some(at),
        }
    }
    None
}

/// Character-wise truncation with a suffix; a no-op when `max` is
/// `usize::MAX` (uncapped pass) or the text already fits.
fn truncate_chars(text: &str, max: usize, suffix: &str) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles() -> Vec<String> {
        vec!["user".to_string(), "assistant".to_string()]
    }

    fn opts(caps: Caps, include_tool_calls: bool) -> NormalizeOpts<'static> {
        // Leaked once per test process; keeps the fixture a one-liner.
        static ROLES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        NormalizeOpts {
            roles: ROLES.get_or_init(roles),
            include_tool_calls,
            caps,
        }
    }

    // ---- tier 1: per-string-field cap -------------------------------------

    #[test]
    fn string_field_over_300_truncated_with_exact_suffix() {
        let long = "x".repeat(1000);
        let input = json!({ "content": long });
        let out = compact_tool_input(&input, &Caps::default());
        let got = out["content"].as_str().unwrap();
        assert_eq!(got.chars().count(), 300 + "... (+700 chars)".len());
        assert!(got.starts_with(&"x".repeat(300)));
        assert!(got.ends_with("... (+700 chars)"));
    }

    #[test]
    fn short_strings_and_non_strings_untouched() {
        let input = json!({
            "file_path": "src/main.rs",
            "line": 42,
            "flag": true,
            "nested": { "a": [1, 2, 3] },
        });
        let out = compact_tool_input(&input, &Caps::default());
        assert_eq!(out, input, "nothing exceeds either cap");
    }

    #[test]
    fn exactly_300_chars_is_not_truncated() {
        let input = json!({ "command": "y".repeat(300) });
        let out = compact_tool_input(&input, &Caps::default());
        // legacy: `len(value) > _TOOL_INPUT_FIELD_MAX`, strictly greater.
        assert_eq!(out["command"].as_str().unwrap().chars().count(), 300);
    }

    #[test]
    fn non_dict_input_becomes_empty_object() {
        for v in [json!("a string"), json!([1, 2]), json!(null), json!(7)] {
            assert_eq!(compact_tool_input(&v, &Caps::default()), json!({}));
        }
    }

    // ---- tier 2: serialized-total cap -------------------------------------

    #[test]
    fn oversized_serialized_falls_back_to_priority_keys() {
        // Ten 280-char fields: each survives tier 1 (under 300) but the
        // serialized whole is ~2900 chars, busting the 1500 total.
        let mut obj = Map::new();
        obj.insert("file_path".into(), json!("src/auth.rs"));
        obj.insert("command".into(), json!("cargo test"));
        for i in 0..10 {
            obj.insert(format!("edit_{i}"), json!("z".repeat(280)));
        }
        let out = compact_tool_input(&Value::Object(obj), &Caps::default());

        assert_eq!(out["file_path"], json!("src/auth.rs"));
        assert_eq!(out["command"], json!("cargo test"));
        let dropped = out["_truncated_fields"].as_array().unwrap();
        assert_eq!(dropped.len(), 10);
        assert!(dropped.contains(&json!("edit_0")));
        assert!(!out.as_object().unwrap().contains_key("edit_0"));
    }

    #[test]
    fn oversized_with_no_priority_keys_keeps_only_the_marker() {
        let input = json!({ "blob": "q".repeat(299), "blob2": "q".repeat(299),
                            "blob3": "q".repeat(299), "blob4": "q".repeat(299),
                            "blob5": "q".repeat(299), "blob6": "q".repeat(299) });
        let out = compact_tool_input(&input, &Caps::default());
        let obj = out.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert_eq!(obj["_truncated_fields"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn caps_none_truncates_nothing() {
        let input = json!({ "content": "x".repeat(50_000) });
        let out = compact_tool_input(&input, &Caps::none());
        assert_eq!(out, input);
    }

    // ---- tool_result cap ---------------------------------------------------

    #[test]
    fn tool_result_truncated_at_2000() {
        let messages = vec![json!({
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "r".repeat(5000) }],
        })];
        let (transcript, n) = normalize(&messages, &opts(Caps::default(), true)).unwrap();
        assert_eq!(n, 1);
        let parsed: Value = serde_json::from_str(&transcript).unwrap();
        let content = parsed[0]["content"][0]["content"].as_str().unwrap();
        assert_eq!(content.chars().count(), 2000 + "... (truncated)".len());
        assert!(content.ends_with("... (truncated)"));
    }

    #[test]
    fn tool_result_array_content_is_flattened() {
        let messages = vec![json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "content": [{ "type": "text", "text": "line one" }, { "type": "text", "text": "line two" }],
            }],
        })];
        let (transcript, _) = normalize(&messages, &opts(Caps::default(), true)).unwrap();
        let parsed: Value = serde_json::from_str(&transcript).unwrap();
        assert_eq!(
            parsed[0]["content"][0]["content"],
            json!("line one\nline two")
        );
    }

    // ---- memory tags -------------------------------------------------------

    #[test]
    fn strip_memory_tags_removes_all_three_families() {
        let raw = "before <memgarden_memories>a</memgarden_memories> mid \
                   <hindsight_memories>b</hindsight_memories> and \
                   <relevant_memories>c</relevant_memories> after";
        let out = strip_memory_tags(raw);
        assert!(!out.contains("memgarden_memories"));
        assert!(!out.contains("hindsight_memories"));
        assert!(!out.contains("relevant_memories"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn strip_memory_tags_is_non_greedy_and_survives_unterminated() {
        let out = strip_memory_tags(
            "<relevant_memories>a</relevant_memories>keep<relevant_memories>b</relevant_memories>",
        );
        assert_eq!(out, "keep");
        let unterminated = "text <relevant_memories>never closed";
        assert_eq!(strip_memory_tags(unterminated), unterminated);
    }

    #[test]
    fn channel_envelope_unwrapped() {
        let raw = "<channel source=\"plugin:telegram\" chat_id=\"7\">\nhello there\n</channel>";
        assert_eq!(strip_channel_envelope(raw), "hello there");
        assert_eq!(strip_channel_envelope("plain text"), "plain text");
        // Word boundary: <channels> must not match.
        assert_eq!(
            strip_channel_envelope("<channels>x</channels>"),
            "<channels>x</channels>"
        );
    }

    // ---- backfill cap ------------------------------------------------------

    fn numbered(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| json!({ "role": "user", "content": format!("msg {i}") }))
            .collect()
    }

    #[test]
    fn backfill_cap_keeps_the_last_n_on_initial_only() {
        let messages = numbered(500);
        let capped = apply_backfill_cap(&messages, true, 300);
        assert_eq!(capped.len(), 300);
        assert_eq!(capped[0]["content"], json!("msg 200"), "keeps the LAST 300");
        assert_eq!(capped[299]["content"], json!("msg 499"));

        // Delta retains are never capped, however long.
        assert_eq!(apply_backfill_cap(&messages, false, 300).len(), 500);
    }

    #[test]
    fn backfill_cap_zero_disables_and_short_input_is_untouched() {
        let messages = numbered(500);
        assert_eq!(apply_backfill_cap(&messages, true, 0).len(), 500);
        let short = numbered(10);
        assert_eq!(apply_backfill_cap(&short, true, 300).len(), 10);
    }

    // ---- file: tags --------------------------------------------------------

    fn tool_use_msg(name: &str, path_key: &str, path: &str) -> Value {
        json!({
            "role": "assistant",
            "content": [{ "type": "tool_use", "name": name, "input": { path_key: path } }],
        })
    }

    #[test]
    fn touched_files_relativize_dedup_and_respect_tool_set() {
        let cwd = "/home/u/proj";
        let messages = vec![
            tool_use_msg("Edit", "file_path", "/home/u/proj/src/auth.rs"),
            tool_use_msg("Read", "file_path", "/home/u/proj/src/ignored.rs"),
            tool_use_msg(
                "mcp__someserver__Edit",
                "file_path",
                "/home/u/proj/src/mcp.rs",
            ),
            tool_use_msg("NotebookEdit", "notebook_path", "/home/u/proj/nb.ipynb"),
            tool_use_msg("Write", "file_path", "/etc/outside.conf"),
            // Duplicate of the first: first-touch order preserved, no repeat.
            tool_use_msg("Write", "file_path", "/home/u/proj/src/auth.rs"),
            // user-role tool_use blocks never count.
            json!({ "role": "user", "content": [
                { "type": "tool_use", "name": "Edit", "input": { "file_path": "/home/u/proj/nope.rs" } }
            ]}),
        ];
        let files = extract_touched_files(&messages, cwd);
        assert_eq!(
            files,
            vec![
                "src/auth.rs".to_string(),
                "src/mcp.rs".to_string(),
                "nb.ipynb".to_string(),
                "/etc/outside.conf".to_string(), // outside cwd stays absolute
            ]
        );
    }

    #[test]
    fn touched_files_cap_20_at_the_call_site() {
        let messages: Vec<Value> = (0..30)
            .map(|i| tool_use_msg("Write", "file_path", &format!("/w/f{i}.rs")))
            .collect();
        let files = extract_touched_files(&messages, "/w");
        assert_eq!(files.len(), 30, "extraction itself is uncapped");
        let tags: Vec<String> = files.iter().take(20).map(|p| format!("file:{p}")).collect();
        assert_eq!(tags.len(), 20);
        assert_eq!(tags[0], "file:f0.rs");
        assert_eq!(tags[19], "file:f19.rs");
    }

    #[test]
    fn touched_files_rejects_absurd_and_control_char_paths() {
        let messages = vec![
            tool_use_msg("Edit", "file_path", &"a".repeat(600)),
            tool_use_msg("Edit", "file_path", "src/ev\u{1b}[2Jil.rs"),
            tool_use_msg("Edit", "file_path", "src/ok.rs"),
        ];
        assert_eq!(extract_touched_files(&messages, ""), vec!["src/ok.rs"]);
    }

    #[test]
    fn touched_files_empty_without_cwd_stays_absolute() {
        let messages = vec![tool_use_msg("Edit", "file_path", "/a/b.rs")];
        assert_eq!(extract_touched_files(&messages, ""), vec!["/a/b.rs"]);
    }

    // ---- normalize ---------------------------------------------------------

    #[test]
    fn text_mode_uses_the_legacy_role_markers() {
        let messages = vec![
            json!({ "role": "user", "content": "why is recall slow?" }),
            json!({ "role": "system", "content": "ignored role" }),
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "VRAM contention" }] }),
        ];
        let (transcript, n) = normalize(&messages, &opts(Caps::default(), false)).unwrap();
        assert_eq!(n, 2, "the system role is filtered out");
        assert_eq!(
            transcript,
            "[role: user]\nwhy is recall slow?\n[user:end]\n\n\
             [role: assistant]\nVRAM contention\n[assistant:end]"
        );
    }

    #[test]
    fn nothing_to_retain_yields_none() {
        // No messages at all, only filtered-out roles, and content that
        // vanishes once memory tags are stripped — all "nothing to retain".
        assert!(normalize(&[], &opts(Caps::default(), false)).is_none());
        let only_system = vec![json!({ "role": "system", "content": "a long enough system note" })];
        assert!(normalize(&only_system, &opts(Caps::default(), false)).is_none());
        let all_injection = vec![json!({
            "role": "user",
            "content": "<relevant_memories>everything here was injected</relevant_memories>",
        })];
        assert!(normalize(&all_injection, &opts(Caps::default(), false)).is_none());
        assert!(normalize(&all_injection, &opts(Caps::default(), true)).is_none());
    }

    #[test]
    fn short_content_still_retains_because_the_markers_count() {
        // legacy applies its <10-character floor to the FORMATTED transcript
        // (`content.py:399`), and the `[role: …]` markers alone clear it. The
        // floor is therefore a vestige for the marker-less paths; ported as
        // written rather than "fixed".
        let messages = vec![json!({ "role": "user", "content": "hi" })];
        let (transcript, n) = normalize(&messages, &opts(Caps::default(), false)).unwrap();
        assert_eq!(n, 1);
        assert_eq!(transcript, "[role: user]\nhi\n[user:end]");
    }

    #[test]
    fn own_mcp_tools_are_skipped_to_avoid_a_feedback_loop() {
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                { "type": "tool_use", "name": "mcp__memgarden__recall", "input": { "query": "x" } },
                { "type": "tool_use", "name": "mcp__memgarden__retain", "input": { "text": "x" } },
                { "type": "tool_use", "name": "Bash", "input": { "command": "cargo test" } },
            ],
        })];
        let (transcript, _) = normalize(&messages, &opts(Caps::default(), true)).unwrap();
        assert!(!transcript.contains("recall"));
        assert!(!transcript.contains("retain"));
        assert!(transcript.contains("cargo test"));
    }

    #[test]
    fn capped_normalization_is_strictly_smaller_than_uncapped() {
        let messages = vec![json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "name": "Write",
                "input": { "file_path": "src/big.rs", "content": "fn main() {}\n".repeat(500) },
            }],
        })];
        let (raw, _) = normalize(&messages, &opts(Caps::none(), true)).unwrap();
        let (capped, _) = normalize(&messages, &opts(Caps::default(), true)).unwrap();
        assert!(
            capped.len() * 4 < raw.len(),
            "a 6.5KB Write must shrink by far more than 4x: raw={} capped={}",
            raw.len(),
            capped.len()
        );
        assert!(capped.contains("src/big.rs"), "the priority key survives");
    }

    /// The opt-out marker. A redaction control gets its own tests because the
    /// cost of it half-working is a stored secret.
    #[test]
    fn a_private_block_is_removed_and_its_surroundings_are_not() {
        assert_eq!(
            strip_private("keep this <private>drop this</private> and this"),
            "keep this  and this"
        );
        assert_eq!(strip_private("nothing to do here"), "nothing to do here");
        assert_eq!(strip_private(""), "");
    }

    #[test]
    fn several_private_blocks_all_go() {
        assert_eq!(
            strip_private("a<private>x</private>b<private>y</private>c"),
            "abc"
        );
    }

    /// **Fails closed, unlike `remove_blocks`.** An unclosed `<private>` is a
    /// user who started saying something they did not want kept and was cut
    /// off — by an interruption, or by a delta retain slicing the transcript
    /// mid-message. Leaving the tail in, the way the memory-wrapper stripper
    /// does, would store exactly the thing the marker was asking to withhold.
    #[test]
    fn an_unterminated_private_block_drops_the_rest() {
        assert_eq!(
            strip_private("public part <private>my api key is sk-live-"),
            "public part "
        );
        // The contrast, asserted: the wrapper stripper keeps it.
        let unterminated = "public part <memgarden_memories>tail";
        assert_eq!(strip_memory_tags(unterminated), unterminated);
    }

    /// The stated shape is exact and lower-case, so the tests say so rather
    /// than leaving a reader to assume a tolerance that is not there.
    #[test]
    fn the_private_marker_is_exact() {
        let uppercase = "a <PRIVATE>b</PRIVATE> c";
        assert_eq!(strip_private(uppercase), uppercase);
        let attributed = "a <private reason=\"key\">b</private> c";
        // `<private ` never matches the open tag, so nothing is dropped.
        assert_eq!(strip_private(attributed), attributed);
    }

    /// End to end: the marker has to reach `normalize`, because a helper that
    /// is right and unwired is how CE-10 spent two months returning nothing.
    #[test]
    fn normalize_drops_private_text_from_a_real_message() {
        let messages = vec![
            json!({ "role": "user", "content": "deploy failed <private>token ghp_secret</private> on prod" }),
            json!({ "role": "user", "content": [
                { "type": "text", "text": "and <private>my home address</private> is irrelevant" },
            ]}),
        ];
        let (text, _) = normalize(&messages, &opts(Caps::none(), false)).expect("normalized");
        assert!(!text.contains("ghp_secret"), "{text}");
        assert!(!text.contains("home address"), "{text}");
        assert!(text.contains("deploy failed"));
        assert!(text.contains("is irrelevant"));
    }
}
