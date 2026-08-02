//! Fact extraction: prompts ported from legacy `fact_extraction.py`, and
//! lenient parsing of the LLM's JSON response. See `prompts` and `parse`.

pub mod parse;
pub mod prompts;

use serde_json::{Value, json};

use crate::ollama::{OllamaClient, OllamaError};

/// The literal JSON schema restated in `format` (Ollama ignores it for
/// `/api/chat` — see `ollama.rs` — but it's cheap and it's what the plan's
/// verification runs used).
fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "facts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "what": {"type": "string"},
                        "when": {"type": "string"},
                        "where": {"type": "string"},
                        "who": {"type": "string"},
                        "why": {"type": "string"},
                        "fact_kind": {"type": "string", "enum": ["event", "conversation"]},
                        "fact_type": {"type": "string", "enum": ["world", "assistant"]},
                        "occurred_start": {"type": "string"},
                        "occurred_end": {"type": "string"},
                        "entities": {"type": "array", "items": {"type": "string"}},
                    },
                    "required": ["what", "when", "where", "who", "why", "fact_type"],
                },
            }
        },
        "required": ["facts"],
    })
}

/// One end-to-end extraction call: build the prompts, call Ollama, parse the
/// response. `event_date_ms` and `mission` come straight from the
/// `dry-run-extract` request body (`routes/extract.rs`); `causal` is fixed
/// `true` here — legacy's default (`DEFAULT_RETAIN_EXTRACT_CAUSAL_LINKS`,
/// `config.py:1096`) and there's no per-request override in this endpoint's
/// contract.
pub async fn extract(
    client: &OllamaClient,
    text: &str,
    event_date_ms: Option<i64>,
    mission: Option<&str>,
) -> Result<Vec<parse::ParsedFact>, OllamaError> {
    let system = prompts::system_prompt(true);
    let mission_preamble = prompts::retain_mission_preamble(mission);
    let user = prompts::user_message(&mission_preamble, event_date_ms, None, text);

    let raw: parse::RawFactsResponse = client.chat_json(&system, &user, &output_schema()).await?;
    Ok(parse::parse_facts(raw.into_facts()))
}
