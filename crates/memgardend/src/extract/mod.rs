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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ollama::OllamaClient;

    /// Live end-to-end against the real Ollama (spec: PR B2 Tests bullet).
    /// The `/api/chat` schema-ignoring behavior is only observable live, so
    /// this is the one test that catches a genuine prompt/model mismatch.
    /// Run: `cargo test -p memgardend live_extract -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "requires a running Ollama with the configured model"]
    async fn live_extract() {
        let cfg = memgarden_core::config::Config::defaults()
            .expect("default config")
            .ollama;
        let client = OllamaClient::new(cfg).expect("client");
        let text = "User: Our recall latency regressed to 830ms after wiring the reranker.\n\n\
                    Assistant: Confirmed the cause: the embedding model was competing for VRAM \
                    with the resident 13GB Ollama model, so I forced CPU inference for embeddings \
                    and the reranker. Recall p50 is now 20-37ms.";
        let started = std::time::Instant::now();
        let facts = extract(&client, text, Some(1_754_100_000_000), None)
            .await
            .expect("live extraction should succeed");
        println!(
            "live_extract: {} facts in {:.1}s",
            facts.len(),
            started.elapsed().as_secs_f64()
        );
        assert!(!facts.is_empty(), "live extraction returned zero facts");
    }
}
