//! Fact-extraction prompts, ported verbatim from legacy
//! `hindsight-api-slim/hindsight_api/engine/retain/fact_extraction.py`.
//!
//! Only "concise" mode is ported (legacy's default, `:1103`) — B2 scope.
//! `verbose` / `custom` / `verbatim` modes are not needed here.

/// legacy: `fact_extraction.py:680-739` (`_BASE_FACT_EXTRACTION_PROMPT`).
/// `{retain_mission_section}` / `{extraction_guidelines}` / `{examples}` are
/// filled in by `system_prompt()` via `.replace()`, not `str::format!` — the
/// literal `{`/`}` in the JSON-shape section appended by `system_prompt()`
/// would otherwise need escaping.
const BASE_FACT_EXTRACTION_PROMPT: &str = r#"Extract SIGNIFICANT facts from text. Be SELECTIVE - only extract facts worth remembering long-term.

LANGUAGE: MANDATORY — Detect the language of the input text and produce ALL output in that EXACT same language. You are STRICTLY FORBIDDEN from translating or switching to any other language. Every single word of your output must be in the same language as the input. Do NOT output in a different language under any circumstance.

{retain_mission_section}{extraction_guidelines}

══════════════════════════════════════════════════════════════════════════
FACT FORMAT - BE CONCISE
══════════════════════════════════════════════════════════════════════════

1. "what": Core fact - concise but complete (1-2 sentences max)
2. "when": Temporal info if mentioned. "N/A" if none. Use day name when known.
3. "where": Location if relevant. "N/A" if none.
4. "who": People involved with relationships. "N/A" if just general info.
5. "why": Context/significance ONLY if important. "N/A" if obvious.

CONCISENESS: Capture the essence, not every word. One good sentence beats three mediocre ones.

══════════════════════════════════════════════════════════════════════════
COREFERENCE RESOLUTION
══════════════════════════════════════════════════════════════════════════

Link generic references to names when both appear:
- "my roommate" + "Emily" → use "Emily (user's roommate)"
- "the manager" + "Sarah" → use "Sarah (the manager)"

══════════════════════════════════════════════════════════════════════════
CLASSIFICATION
══════════════════════════════════════════════════════════════════════════

fact_kind:
- "event": Specific datable occurrence (set occurred_start/end)
- "conversation": Ongoing state, preference, trait (no dates)

fact_type:
- "world": Objective/external facts, including the user's preferences, rules, corrections, constraints, plans, traits, or context. These stay "world" even when the user states them during an assistant interaction (e.g., "User prefers browser_navigate over web_search", "User corrected the project deadline").
- "assistant": Actions, experiences, or observations the assistant/agent actually performed (e.g., "I changed X", "I discovered Y", "I debugged Z"). Use this for the assistant/agent doing, trying, learning, deciding, recommending, or responding — not merely for user facts mentioned in conversation.

══════════════════════════════════════════════════════════════════════════
TEMPORAL HANDLING
══════════════════════════════════════════════════════════════════════════

Use "Event Date" from input as reference for relative dates.
- CRITICAL: Convert ALL relative temporal expressions to absolute dates in the fact text itself.
  "yesterday" → write the resolved date (e.g. "on November 12, 2024"), NOT the word "yesterday"
  "last night", "this morning", "today", "tonight" → convert to the resolved absolute date
- For events: set occurred_start AND occurred_end (same for point events)
- For conversation facts: NO occurred dates

══════════════════════════════════════════════════════════════════════════
ENTITIES
══════════════════════════════════════════════════════════════════════════

ALWAYS return "entities" as an array of plain strings — never objects, never null.
Correct: entities=["Alice", "Kubernetes", "CKA"]
Wrong:   entities as an array of objects with a "text" key ← never use this form
Use an empty array [] only when the fact truly names nothing.

Include: people names, organizations, places, key objects, abstract concepts (career, friendship, etc.)
Always include "user" when fact is about the user.{examples}"#;

/// legacy: `fact_extraction.py:742-762` (`_CONCISE_GUIDELINES`).
const CONCISE_GUIDELINES: &str = r#"══════════════════════════════════════════════════════════════════════════
SELECTIVITY - CRITICAL (Reduces 90% of unnecessary output)
══════════════════════════════════════════════════════════════════════════

ONLY extract facts that are:
✅ Personal info: names, relationships, roles, background
✅ Preferences: likes, dislikes, habits, interests (e.g., "Alice likes coffee")
✅ Significant events: milestones, decisions, achievements, changes
✅ Plans/goals: future intentions, deadlines, commitments
✅ Expertise: skills, knowledge, certifications, experience
✅ Important context: projects, problems, constraints
✅ Sensory/emotional details: feelings, sensations, perceptions that provide context
✅ Observations: descriptions of people, places, things with specific details

DO NOT extract:
❌ Generic greetings: "how are you", "hello", pleasantries without substance
❌ Pure filler: "thanks", "sounds good", "ok", "got it", "sure"
❌ Process chatter: "let me check", "one moment", "I'll look into it"
❌ Repeated info: if already stated, don't extract again

CONSOLIDATE related statements into ONE fact when possible."#;

/// legacy: `fact_extraction.py:765-794` (`_CONCISE_EXAMPLES`). The two
/// leading blank lines are part of the string — do not strip them (see the
/// port brief §3.3).
const CONCISE_EXAMPLES: &str = r#"

══════════════════════════════════════════════════════════════════════════
EXAMPLES (shown in English for illustration; for non-English input, ALL output values MUST be in the input language)
══════════════════════════════════════════════════════════════════════════

Example 1 - Selective extraction (Event Date: June 10, 2024):
Input: "Hey! How's it going? Good morning! So I'm planning my wedding - want a small outdoor ceremony. Just got back from Emily's wedding, she married Sarah at a rooftop garden. It was nice weather. I grabbed a coffee on the way."

Output: ONLY 2 facts (skip greetings, weather, coffee):
1. what="User planning wedding, wants small outdoor ceremony", who="user", why="N/A", entities=["user", "wedding"]
2. what="Emily married Sarah at rooftop garden", who="Emily (user's friend), Sarah", occurred_start="2024-06-09", entities=["Emily", "Sarah", "wedding"]

Example 2 - Professional context:
Input: "Alice has 5 years of Kubernetes experience and holds CKA certification. She's been leading the infrastructure team since March. By the way, she prefers dark roast coffee."

Output: ONLY 2 facts (skip coffee preference - too trivial):
1. what="Alice has 5 years Kubernetes experience, CKA certified", who="Alice", entities=["Alice", "Kubernetes", "CKA"]
2. what="Alice leads infrastructure team since March", who="Alice", entities=["Alice", "infrastructure"]

══════════════════════════════════════════════════════════════════════════
QUALITY OVER QUANTITY
══════════════════════════════════════════════════════════════════════════

Ask: "Would this be useful to recall in 6 months?" If no, skip it.

IMPORTANT: Sensory/emotional details and observations that provide meaningful context
about experiences ARE important to remember, even if they seem small (e.g., how food
tasted, how someone looked, how loud music was). Extract these if they characterize
an experience or person."#;

/// legacy: `fact_extraction.py:945-957` (`CAUSAL_RELATIONSHIPS_SECTION`),
/// appended at `:1115` when causal extraction is enabled. The two leading
/// blank lines are part of the string.
const CAUSAL_RELATIONSHIPS_SECTION: &str = r#"

══════════════════════════════════════════════════════════════════════════
CAUSAL RELATIONSHIPS
══════════════════════════════════════════════════════════════════════════

Link facts with causal_relations (max 2 per fact). target_index must be < this fact's index.
Type: "caused_by" (this fact was caused by the target fact)

Example: "Lost job → couldn't pay rent → moved apartment"
- Fact 0: Lost job, causal_relations: null
- Fact 1: Couldn't pay rent, causal_relations: [{target_index: 0, relation_type: "caused_by"}]
- Fact 2: Moved apartment, causal_relations: [{target_index: 1, relation_type: "caused_by"}]"#;

/// `/api/chat` does NOT enforce the `format` schema it's given (verified —
/// see `ollama.rs`'s `chat_json` doc comment and the plan's Verified
/// Environment Facts). This section is appended to the system prompt so the
/// shape is also stated in prose, which measurably worked in the plan's
/// verification runs. Kept separate from the ported legacy sections above.
const OUTPUT_SHAPE_SECTION: &str = r#"

══════════════════════════════════════════════════════════════════════════
OUTPUT FORMAT — REQUIRED
══════════════════════════════════════════════════════════════════════════

Output ONLY a JSON object of exactly this shape, no other text:
{"facts":[{"what":"...","when":"...","where":"...","who":"...","why":"...","fact_kind":"event"|"conversation","fact_type":"world"|"assistant","occurred_start":"...","occurred_end":"...","entities":["..."]}]}"#;

/// Assembles the full system prompt: base + concise guidelines + concise
/// examples (+ causal section when `causal` is set) + the literal JSON
/// output shape. `{retain_mission_section}` is deliberately left as a
/// literal, empty placeholder in the *system* prompt — the mission rides in
/// the user message instead (`user_message`, `mission_preamble`), matching
/// legacy's prompt-cache rationale (`fact_extraction.py:1072-1077`, brief
/// gotcha #3): the system prompt must stay bank-agnostic so Ollama's prefix
/// KV cache serves every bank.
pub fn system_prompt(causal: bool) -> String {
    let mut prompt = BASE_FACT_EXTRACTION_PROMPT
        .replace("{retain_mission_section}", "")
        .replace("{extraction_guidelines}", CONCISE_GUIDELINES)
        .replace("{examples}", CONCISE_EXAMPLES);
    if causal {
        prompt.push_str(CAUSAL_RELATIONSHIPS_SECTION);
    }
    prompt.push_str(OUTPUT_SHAPE_SECTION);
    prompt
}

/// legacy: `_retain_mission_preamble`, `fact_extraction.py:1177-1194`.
/// Returns `""` when unset. No brace-escaping needed — the user message is
/// used verbatim, never `str::format!`-ed (`fact_extraction.py:1183-1184`).
pub fn retain_mission_preamble(retain_mission: Option<&str>) -> String {
    match retain_mission {
        None | Some("") => String::new(),
        Some(mission) => format!(
            "══════════════════════════════════════════════════════════════════════════\n\
             FOCUS — What to retain for this bank (takes priority over the general guidelines)\n\
             ══════════════════════════════════════════════════════════════════════════\n\n\
             {mission}\n\n"
        ),
    }
}

/// `unix_ms` -> `"{Weekday}, {Month} {day}, {year} ({iso})"`, UTC.
/// legacy: `fact_extraction.py:1220` —
/// `f"{event_date.strftime('%A, %B %d, %Y')} ({event_date.isoformat()})"`.
pub fn event_date_str(unix_ms: i64) -> String {
    let Ok(ts) = jiff::Timestamp::from_millisecond(unix_ms) else {
        return "Unknown".to_string();
    };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::UTC);
    let human = jiff::fmt::strtime::format("%A, %B %d, %Y", &zoned).unwrap_or_default();
    // jiff's ISO-8601 Display for Zoned includes the zone annotation
    // (`[UTC]`); legacy's isoformat() does not, so this trims it rather
    // than adding a jiff format-string dependency for one field.
    let iso = zoned.timestamp().to_string();
    format!("{human} ({iso})")
}

/// legacy: `_build_user_message`, `fact_extraction.py:1197-1252`. Metadata
/// and narrator sections are omitted here — `dry-run-extract`'s request
/// contract (`{text, event_date?, mission?}`) has no metadata/agent_name
/// inputs to carry them; B3's full retain path adds them when it exists.
pub fn user_message(
    mission_preamble: &str,
    event_date_ms: Option<i64>,
    context: Option<&str>,
    chunk: &str,
) -> String {
    let event_date_str = match event_date_ms {
        Some(ms) => event_date_str(ms),
        None => "Unknown".to_string(),
    };
    let context = context.filter(|c| !c.is_empty()).unwrap_or("none");
    format!(
        "{mission_preamble}Extract facts from the following text chunk.\n\
         \n\
         Chunk: 1/1\n\
         Event Date: {event_date_str}\n\
         Context: {context}\n\
         \n\
         Text:\n\
         {chunk}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_mission_section_stays_empty() {
        // Load-bearing for prompt caching (brief gotcha #3): the mission
        // must never appear in the system prompt.
        let prompt = system_prompt(false);
        assert!(!prompt.contains("{retain_mission_section}"));
        assert!(!prompt.contains("FOCUS —"));
        assert!(prompt.starts_with("Extract SIGNIFICANT facts from text."));
    }

    #[test]
    fn system_prompt_causal_toggles_section() {
        let without = system_prompt(false);
        let with = system_prompt(true);
        assert!(!without.contains("CAUSAL RELATIONSHIPS"));
        assert!(with.contains("CAUSAL RELATIONSHIPS"));
        assert!(with.contains("target_index must be < this fact's index"));
    }

    #[test]
    fn system_prompt_always_carries_output_shape() {
        let prompt = system_prompt(false);
        assert!(prompt.contains("\"facts\":[{"));
    }

    #[test]
    fn mission_preamble_empty_when_unset() {
        assert_eq!(retain_mission_preamble(None), "");
        assert_eq!(retain_mission_preamble(Some("")), "");
    }

    #[test]
    fn mission_preamble_present_has_trailing_blank_line() {
        let preamble = retain_mission_preamble(Some("Focus on bug fixes."));
        assert!(preamble.contains("FOCUS —"));
        assert!(preamble.contains("Focus on bug fixes."));
        assert!(preamble.ends_with("\n\n"));
    }

    #[test]
    fn user_message_mission_present_vs_absent() {
        let without = user_message("", Some(1_718_000_000_000), Some("chat"), "hello");
        let with = user_message(
            &retain_mission_preamble(Some("Focus on X.")),
            Some(1_718_000_000_000),
            Some("chat"),
            "hello",
        );
        assert!(without.starts_with("Extract facts from the following text chunk."));
        // legacy preamble opens with the ═══ box line, FOCUS — is line 2
        assert!(with.starts_with("══"));
        assert!(with.contains("FOCUS —"));
        assert!(with.contains("Extract facts from the following text chunk."));
    }

    #[test]
    fn user_message_no_context_defaults_to_none() {
        let msg = user_message("", None, None, "hi");
        assert!(msg.contains("Context: none"));
        assert!(msg.contains("Event Date: Unknown"));
    }

    #[test]
    fn event_date_str_matches_legacy_format() {
        // 2024-06-10T00:00:00Z is a Monday.
        let ms = 1_718_006_400_000;
        let s = event_date_str(ms);
        assert!(s.starts_with("Monday, June 10, 2024 ("));
        assert!(s.ends_with(')'));
    }
}
