//! Lenient parsing of the LLM's raw JSON fact-extraction response, ported
//! from `hindsight-api-slim/hindsight_api/engine/retain/fact_extraction.py:1447-1636`
//! and `engine/retain/types.py:189-223` (degenerate-text rejection).
//!
//! A non-dict entry in the top-level `facts` array is skipped rather than
//! failing the whole response — legacy's coarser array-level rejection
//! (`_coerce_fact_response`, `:1297-1305`) is subsumed here by
//! `ollama::OllamaClient::chat_json`'s retry-then-hard-error on shape
//! mismatch; once the shape itself parses, an individual bad fact is just
//! skipped, matching the per-item loop's own leniency.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use memgarden_core::types::FactType;

/// Accepts either the documented `{"facts": [...]}` wrapper or a bare
/// top-level array — legacy: `_coerce_fact_response`, `fact_extraction.py:1297-1305`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawFactsResponse {
    // No #[serde(default)] on `facts`: with it, this variant matches EVERY
    // JSON object ({"error":...} → 0 facts, no retry) — the silent-zero-facts
    // class the plan bans (legacy issue #1833). A missing `facts` key must
    // fail deserialization so chat_json retries.
    Wrapped { facts: Vec<Value> },
    Bare(Vec<Value>),
}

impl RawFactsResponse {
    pub fn into_facts(self) -> Vec<Value> {
        match self {
            RawFactsResponse::Wrapped { facts } => facts,
            RawFactsResponse::Bare(facts) => facts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CausalRelation {
    pub target_index: usize,
    pub relation_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParsedFact {
    /// " | "-joined combination of what/when/who/why — legacy:
    /// `fact_extraction.py:1500-1511`. `where` is deliberately excluded.
    pub text: String,
    pub fact_type: FactType,
    /// Normalized to `conversation`/`event`/`other`; not itself stored on
    /// the eventual memory node (B3), kept here for visibility/testing.
    pub fact_kind: String,
    pub occurred_start: Option<String>,
    pub occurred_end: Option<String>,
    #[serde(rename = "where")]
    pub where_field: Option<String>,
    pub entities: Vec<String>,
    pub causal_relations: Vec<CausalRelation>,
}

/// legacy: the loop body of `fact_extraction.py:1447-1636`. Returns only the
/// facts that survive both the `what`-presence check and degenerate-text
/// rejection — an empty result for an all-filler chunk is a valid, expected
/// outcome (legacy logs it and moves on, `:1437-1445`), not an error.
pub fn parse_facts(raw_facts: Vec<Value>) -> Vec<ParsedFact> {
    let parsed: Vec<Option<ParsedFact>> = raw_facts
        .iter()
        .enumerate()
        .map(|(i, llm_fact)| llm_fact.as_object().and_then(|obj| parse_one(obj, i)))
        .collect();

    // legacy: `_remap_causal_relations`, orchestrator.py:625-654 — the LLM's
    // target_index values are raw-array ordinals, but drops compact the
    // output. Rewrite each index to the survivor ordinal; a relation whose
    // target was rejected disappears rather than silently pointing at the
    // next surviving fact.
    let mut new_index = vec![None; parsed.len()];
    let mut next = 0usize;
    for (i, p) in parsed.iter().enumerate() {
        if p.is_some() {
            new_index[i] = Some(next);
            next += 1;
        }
    }
    parsed
        .into_iter()
        .flatten()
        .map(|mut fact| {
            fact.causal_relations.retain_mut(|rel| {
                match new_index.get(rel.target_index).copied().flatten() {
                    Some(idx) => {
                        rel.target_index = idx;
                        true
                    }
                    None => false,
                }
            });
            fact
        })
        .collect()
}

fn parse_one(fact: &serde_json::Map<String, Value>, i: usize) -> Option<ParsedFact> {
    // legacy: fact_extraction.py:1462-1476 — what -> factual_core -> text;
    // missing all three skips the fact entirely.
    let what = get_str(fact, "what")
        .or_else(|| get_str(fact, "factual_core"))
        .or_else(|| get_str(fact, "text"))?;

    let when = get_str(fact, "when");
    let who = get_str(fact, "who");
    let why = get_str(fact, "why");
    let where_field = get_str(fact, "where");

    // legacy: fact_extraction.py:1500-1511 — " | "-joined, `where` excluded.
    let mut parts = vec![what];
    if let Some(when) = &when {
        parts.push(format!("When: {when}"));
    }
    if let Some(who) = &who {
        parts.push(format!("Involving: {who}"));
    }
    if let Some(why) = why {
        parts.push(why);
    }
    let text = parts.join(" | ");

    // legacy: types.py:189-223, enforced at ProcessedFact::from_extracted_fact
    // (types.py:242-247). Zero-information text is dropped, never stored.
    if is_degenerate_text(&text) {
        return None;
    }

    // legacy: fact_extraction.py:1478-1487 — "assistant" -> stored
    // "experience" is a silent rename; everything else is "world", with a
    // fallback through fact_kind (ported as-is, even though legitimate
    // fact_kind values never equal "assistant" — parity over "fixing" it).
    let raw_fact_type = get_str(fact, "fact_type");
    let raw_fact_kind = fact.get("fact_kind").and_then(Value::as_str);
    let fact_type = match raw_fact_type.as_deref() {
        Some("assistant") => FactType::Experience,
        Some("world") => FactType::World,
        _ if raw_fact_kind == Some("assistant") => FactType::Experience,
        _ => FactType::World,
    };

    // legacy: fact_extraction.py:1490-1492.
    let fact_kind = fact
        .get("fact_kind")
        .and_then(Value::as_str)
        .filter(|k| matches!(*k, "conversation" | "event" | "other"))
        .unwrap_or("conversation")
        .to_string();

    // legacy: fact_extraction.py:1513-1529. Only the LLM's own
    // occurred_start/occurred_end are read here — the relative-expression
    // fallback (_infer_temporal_date, e.g. "yesterday" -> an absolute date)
    // needs the temporal module that lands in B6/CE-8 and is not ported yet.
    let (occurred_start, occurred_end) = if fact_kind == "event" {
        let start = get_str(fact, "occurred_start");
        // Point-event default: occurred_end = occurred_start when the LLM
        // only gave one endpoint (fact_extraction.py:1525-1529).
        let end = get_str(fact, "occurred_end").or_else(|| start.clone());
        (start, end)
    } else {
        (None, None)
    };

    let entities = coerce_entity_strings(fact.get("entities"));

    let causal_relations = get_value(fact, "causal_relations")
        .and_then(Value::as_array)
        .map(|relations| {
            relations
                .iter()
                .filter_map(|rel| {
                    let obj = rel.as_object()?;
                    let target_index = obj.get("target_index")?.as_i64()?;
                    let relation_type = obj.get("relation_type")?.as_str()?.to_string();
                    // legacy: fact_extraction.py:1606 — 0 <= target_index < i.
                    if target_index < 0 || target_index as usize >= i {
                        return None;
                    }
                    Some(CausalRelation {
                        target_index: target_index as usize,
                        relation_type,
                    })
                })
                // Max 2 per fact (CAUSAL_RELATIONSHIPS_SECTION prompt).
                .take(2)
                .collect()
        })
        .unwrap_or_default();

    Some(ParsedFact {
        text,
        fact_type,
        fact_kind,
        occurred_start,
        occurred_end,
        where_field,
        entities,
        causal_relations,
    })
}

/// legacy: `get_value`, `fact_extraction.py:1455-1459`. Treats `""`, `[]`,
/// `{}`, case-insensitive `"N/A"`, and JSON `null`/`false`/`0` as absent —
/// matching Python's truthiness check on `value`.
fn get_value<'a>(fact: &'a serde_json::Map<String, Value>, field: &str) -> Option<&'a Value> {
    let v = fact.get(field)?;
    let present = match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty() && !s.eq_ignore_ascii_case("n/a"),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    };
    present.then_some(v)
}

// Deliberately narrower than legacy: `{"what": 42}` is skipped here, while
// Python's get_value would take the number (and then crash in " | ".join).
fn get_str(fact: &serde_json::Map<String, Value>, field: &str) -> Option<String> {
    get_value(fact, field)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// legacy: `_coerce_entity_strings`, `fact_extraction.py:118-144`. Accepts
/// plain strings or the older `{"text": "..."}` object form; anything else
/// (including a non-array `entities` field) is dropped.
fn coerce_entity_strings(entities: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = entities else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("text").and_then(Value::as_str).map(str::to_string),
            _ => None,
        })
        .collect()
}

/// legacy: `ProcessedFact._is_degenerate_text`, `types.py:189-223`.
///
/// Also reused by B3's retain worker as the "is this chunk worth an LLM
/// call at all" gate — a whitespace/punctuation-only chunk must never reach
/// Ollama (CE-5a review carry-over).
pub fn is_degenerate_text(text: &str) -> bool {
    let stripped = text.trim();
    if stripped.is_empty() {
        return true;
    }
    const JUNK: [&str; 14] = [
        "...", "…", "-", "--", "---", ".", "..", "•", "·", "*", "**", "***", "_,_", "_, _, _",
    ];
    if JUNK.contains(&stripped) {
        return true;
    }
    const PUNCT_ONLY: &str = ".,;:!?-–—…\"'`´ \t\n\r";
    if stripped.chars().all(|c| PUNCT_ONLY.contains(c)) {
        return true;
    }
    if stripped.chars().count() <= 2 && stripped.chars().all(|c| !c.is_alphanumeric()) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wrapped(facts: Value) -> Vec<Value> {
        serde_json::from_value::<RawFactsResponse>(json!({ "facts": facts }))
            .unwrap()
            .into_facts()
    }

    #[test]
    fn accepts_wrapped_shape() {
        let raw = wrapped(json!([{"what": "hello world", "fact_type": "world"}]));
        let facts = parse_facts(raw);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].text, "hello world");
        assert_eq!(facts[0].fact_type, FactType::World);
    }

    #[test]
    fn accepts_bare_top_level_array() {
        let raw: RawFactsResponse =
            serde_json::from_value(json!([{"what": "bare array works"}])).unwrap();
        let facts = parse_facts(raw.into_facts());
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].text, "bare array works");
    }

    #[test]
    fn na_fields_are_absent() {
        let raw = wrapped(json!([
            {"what": "fact text", "when": "n/a", "who": "N/A", "why": ""}
        ]));
        let facts = parse_facts(raw);
        assert_eq!(facts[0].text, "fact text");
    }

    #[test]
    fn empty_array_and_object_are_absent() {
        let raw = wrapped(json!([
            {"what": "fact text", "entities": [], "causal_relations": {}}
        ]));
        let facts = parse_facts(raw);
        assert!(facts[0].entities.is_empty());
        assert!(facts[0].causal_relations.is_empty());
    }

    #[test]
    fn missing_what_falls_back_then_skips() {
        let raw = wrapped(json!([
            {"factual_core": "from factual_core"},
            {"text": "from text field"},
            {"why": "no what/factual_core/text at all"},
        ]));
        let facts = parse_facts(raw);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].text, "from factual_core");
        assert_eq!(facts[1].text, "from text field");
    }

    #[test]
    fn assistant_maps_to_experience() {
        let raw = wrapped(json!([
            {"what": "did a thing", "fact_type": "assistant"},
            {"what": "a world fact", "fact_type": "world"},
            {"what": "no fact_type, fact_kind assistant", "fact_kind": "assistant"},
            {"what": "no fact_type at all"},
        ]));
        let facts = parse_facts(raw);
        assert_eq!(facts[0].fact_type, FactType::Experience);
        assert_eq!(facts[1].fact_type, FactType::World);
        assert_eq!(facts[2].fact_type, FactType::Experience);
        assert_eq!(facts[3].fact_type, FactType::World);
    }

    #[test]
    fn combined_text_joins_what_when_who_why_excludes_where() {
        let raw = wrapped(json!([{
            "what": "Alice moved to Boston",
            "when": "last week",
            "where": "Boston",
            "who": "Alice",
            "why": "new job",
        }]));
        let facts = parse_facts(raw);
        assert_eq!(
            facts[0].text,
            "Alice moved to Boston | When: last week | Involving: Alice | new job"
        );
        assert_eq!(facts[0].where_field.as_deref(), Some("Boston"));
    }

    #[test]
    fn junk_text_rejected() {
        for junk in ["...", "-", "***", "_, _, _", "   ", ".."] {
            let raw = wrapped(json!([{"what": junk}]));
            assert!(
                parse_facts(raw).is_empty(),
                "expected {junk:?} to be rejected as degenerate"
            );
        }
    }

    #[test]
    fn short_non_alphanumeric_rejected_but_short_word_kept() {
        let raw = wrapped(json!([{"what": "-,"}]));
        assert!(parse_facts(raw).is_empty());

        // len<=2 but alphanumeric survives (e.g. an "OK" style fact,
        // however unlikely — the check is about punctuation, not length).
        let raw = wrapped(json!([{"what": "ok"}]));
        assert_eq!(parse_facts(raw).len(), 1);
    }

    #[test]
    fn causal_relations_out_of_range_dropped() {
        let raw = wrapped(json!([
            {"what": "fact 0"},
            {"what": "fact 1", "causal_relations": [
                {"target_index": 0, "relation_type": "caused_by"},
                {"target_index": 1, "relation_type": "caused_by"}, // == i, invalid
                {"target_index": -1, "relation_type": "caused_by"}, // negative, invalid
            ]},
        ]));
        let facts = parse_facts(raw);
        assert_eq!(facts[1].causal_relations.len(), 1);
        assert_eq!(facts[1].causal_relations[0].target_index, 0);
    }

    #[test]
    fn causal_target_index_remapped_after_drops() {
        // Review HIGH finding: raw index 0 is degenerate and dropped, so the
        // survivors compact. A raw relation 2→1 must become 1→0, NOT stay 1
        // (which would be a self-link on the compacted list).
        let raw = wrapped(json!([
            {"what": "..."}, // degenerate, dropped
            {"what": "real fact A"},
            {"what": "real fact B", "causal_relations": [
                {"target_index": 0, "relation_type": "caused_by"}, // → dropped fact: relation removed
                {"target_index": 1, "relation_type": "caused_by"}, // → fact A: remapped to 0
            ]},
        ]));
        let facts = parse_facts(raw);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[1].causal_relations.len(), 1);
        assert_eq!(facts[1].causal_relations[0].target_index, 0);
    }

    #[test]
    fn causal_relations_capped_at_two() {
        let raw = wrapped(json!([
            {"what": "fact 0"},
            {"what": "fact 1"},
            {"what": "fact 2"},
            {"what": "fact 3", "causal_relations": [
                {"target_index": 0, "relation_type": "caused_by"},
                {"target_index": 1, "relation_type": "caused_by"},
                {"target_index": 2, "relation_type": "caused_by"},
            ]},
        ]));
        let facts = parse_facts(raw);
        assert_eq!(facts[3].causal_relations.len(), 2);
    }

    #[test]
    fn entities_coerce_object_form() {
        let raw = wrapped(json!([{
            "what": "fact",
            "entities": ["Alice", {"text": "Bob"}, {"nope": "dropped"}, 42],
        }]));
        let facts = parse_facts(raw);
        assert_eq!(facts[0].entities, vec!["Alice".to_string(), "Bob".to_string()]);
    }

    #[test]
    fn event_kind_defaults_occurred_end_to_start() {
        let raw = wrapped(json!([{
            "what": "launched the rocket",
            "fact_kind": "event",
            "occurred_start": "2024-06-10",
        }]));
        let facts = parse_facts(raw);
        assert_eq!(facts[0].occurred_start.as_deref(), Some("2024-06-10"));
        assert_eq!(facts[0].occurred_end.as_deref(), Some("2024-06-10"));
    }

    #[test]
    fn conversation_kind_has_no_occurred_dates() {
        let raw = wrapped(json!([{
            "what": "likes coffee",
            "fact_kind": "conversation",
            "occurred_start": "2024-06-10",
        }]));
        let facts = parse_facts(raw);
        assert!(facts[0].occurred_start.is_none());
        assert!(facts[0].occurred_end.is_none());
    }

    #[test]
    fn unknown_fact_kind_defaults_to_conversation() {
        let raw = wrapped(json!([{"what": "fact", "fact_kind": "bogus"}]));
        let facts = parse_facts(raw);
        assert_eq!(facts[0].fact_kind, "conversation");
    }

    #[test]
    fn non_dict_entries_are_skipped() {
        let raw = wrapped(json!(["not a dict", {"what": "real fact"}, 42]));
        let facts = parse_facts(raw);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].text, "real fact");
    }
}
