//! Fact extraction: prompts ported from legacy `fact_extraction.py`, and
//! lenient parsing of the LLM's JSON response. See `prompts` and `parse`.

pub mod parse;
pub mod prompts;

use serde_json::{Value, json};

use crate::ollama::{OllamaClient, OllamaError};

/// The most facts one chunk may produce, **enforced by the decoding grammar**
/// rather than hoped for.
///
/// A 3,000-character chunk yields 8-10 facts in practice (measured: 57 facts
/// over 6 chunks). This is ~2.5x that, so it bounds a runaway without
/// truncating an ordinary chunk.
///
/// It exists because a runaway is not hypothetical: on the first real retain
/// of the shadow run a 3,000-character chunk produced a **24,525-character**
/// reply that stopped at `num_predict` (8,192 tokens) mid-object, and the
/// whole chunk's facts were lost. A cap loses the tail of an unusually rich
/// chunk; the truncation lost all of it. `maxItems` is honoured by the
/// grammar — verified live: a schema capped at 3 answered "list twenty
/// fruits" with exactly 3.
const MAX_FACTS_PER_CHUNK: usize = 24;

/// The most existing facts one new fact may retract, **enforced by the
/// grammar** for the same reason `MAX_FACTS_PER_CHUNK` is: this is a 14B model
/// answering with row positions, and a fact that claims to retract eleven
/// others is a runaway, not a discovery.
///
/// **One**, measured down from three. At three, one chunk about AC-1 named all
/// twelve candidates it was shown — see
/// `docs/evidence/supersession-detection.md`. One retraction also makes
/// `superseded_quote` unambiguous, which is what lets the quote be *checked*
/// rather than trusted.
const MAX_SUPERSEDES_PER_FACT: usize = 1;

/// Per-field `maxLength`s, **enforced by the grammar** like `maxItems` above.
///
/// An unbounded string field is where a looping reply goes to die: the model
/// keeps appending to one value until `num_predict` cuts the object off, and
/// the whole chunk is lost as a truncated prefix. Measured on the live daemon
/// (2026-10-13): of 5 replies that failed to parse, 2 ran away inside
/// `occurred_start` (`…Z/N/A/N/A, N/A:00:00.000Z/2026-10-13T…` and an ISO
/// duration loop), 2 inside a prose field repeating one sentence, and 1
/// spilled reasoning into a value. Over three weeks, 24 retain jobs ended
/// `partial` or `failed` on a truncated or degenerate reply. With a
/// bound, the grammar closes the string and the object, so a runaway costs
/// one field's tail instead of the chunk — and the degenerate-reply check
/// still sees the repetition.
///
/// Dates get 40: the longest legitimate value is an RFC 3339 timestamp with
/// a zone offset (29 chars). The prose bounds are well above anything the
/// prompt asks for; `what` takes the shared grammar ceiling.
const DATE_MAX_CHARS: usize = 40;
const SHORT_MAX_CHARS: usize = 300;
const WHY_MAX_CHARS: usize = 600;
const ENTITY_MAX_CHARS: usize = 120;
const MAX_ENTITIES_PER_FACT: usize = 16;

fn bounded(max: usize) -> Value {
    json!({"type": "string", "maxLength": max})
}

/// The JSON schema sent as `format`, and — since the transport moved to
/// `/api/generate` — **actually enforced**. See `ollama.rs::try_chat`: the
/// same schema on `/api/chat` is silently ignored, which is what made a
/// malformed reply a normal outcome rather than an anomaly.
fn output_schema(supersession: bool) -> Value {
    let supersedes = if supersession {
        // No `minimum`/`maximum`: an out-of-range position is rejected in
        // `parse` anyway, and every extra keyword is one more thing the
        // grammar compiler can refuse — which is exactly how CE-10 spent two
        // months returning nothing. `maxItems` is the one that is verified to
        // be honoured, and it is the one that bounds the damage.
        json!({"type": "array", "maxItems": MAX_SUPERSEDES_PER_FACT, "items": {"type": "integer"}})
    } else {
        Value::Null
    };
    let mut schema = json!({
        "type": "object",
        "properties": {
            "facts": {
                "type": "array",
                "maxItems": MAX_FACTS_PER_CHUNK,
                "items": {
                    "type": "object",
                    "properties": {
                        "what": bounded(crate::ollama::GRAMMAR_MAX_CHARS),
                        "when": bounded(SHORT_MAX_CHARS),
                        "where": bounded(SHORT_MAX_CHARS),
                        "who": bounded(SHORT_MAX_CHARS),
                        "why": bounded(WHY_MAX_CHARS),
                        "fact_kind": {"type": "string", "enum": ["event", "conversation"]},
                        "fact_type": {"type": "string", "enum": ["world", "assistant"]},
                        "occurred_start": bounded(DATE_MAX_CHARS),
                        "occurred_end": bounded(DATE_MAX_CHARS),
                        "entities": {
                            "type": "array",
                            "maxItems": MAX_ENTITIES_PER_FACT,
                            "items": bounded(ENTITY_MAX_CHARS),
                        },
                    },
                    "required": ["what", "when", "where", "who", "why", "fact_type"],
                },
            }
        },
        "required": ["facts"],
    });
    // **Required, with "N/A" as the absence marker** — the idiom the four
    // ported string fields already use, and the reason they get filled. The
    // first version added these as optional properties, and measured on the
    // live model: `expires_at` was produced 0 times in 11 facts and
    // `superseded_quote` 0 times in 11, while `supersedes` — the one field
    // whose absence the prompt does not offer — was produced constantly.
    // A grammar-optional field on this model is a field that does not exist.
    if let Some(items) = schema
        .pointer_mut("/properties/facts/items")
        .and_then(Value::as_object_mut)
    {
        let props = items
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .expect("items.properties");
        if supersession {
            props.insert("expires_at".to_string(), bounded(DATE_MAX_CHARS));
            props.insert("superseded_quote".to_string(), bounded(WHY_MAX_CHARS));
            props.insert("supersedes".to_string(), supersedes);

            let required = items
                .get_mut("required")
                .and_then(Value::as_array_mut)
                .expect("items.required");
            required.push(json!("expires_at"));
            required.push(json!("supersedes"));
            required.push(json!("superseded_quote"));
        }
    }
    schema
}

/// One end-to-end extraction call: build the prompts, call Ollama, parse the
/// response. `causal` is fixed `true` here — legacy's default
/// (`DEFAULT_RETAIN_EXTRACT_CAUSAL_LINKS`, `config.py:1096`) and there's no
/// per-request override in either caller's contract.
///
/// `background` picks the concurrency-permit policy: `false` for an HTTP
/// request handler (fail fast with `Busy` after 15s — Critic Revision R11),
/// `true` for the retain worker (wait untimed; the job's wall clock is the
/// bound).
/// `known` is CE-12's candidate list: the bank's nearest existing facts, which
/// the model may declare retracted. **An empty slice restores the pre-CE-12
/// prompt and schema byte for byte** — that is deliberate three times over: it
/// makes "detection off" and "a bank with nothing stored yet" the same code
/// path, it gives the A/B a real control arm, and since detection ships
/// **off** (`docs/evidence/supersession-detection.md`) it is what every
/// production retain actually runs.
///
/// The lifecycle fields — `supersedes`, `superseded_quote`, `expires_at` —
/// ride on the same switch, including `expires_at`, which needs no candidates
/// of its own. That is not tidiness: as a *required* field it works and as an
/// optional one the model never fills it (0 of 11 facts, measured), and the
/// required form is what made a chunk truncate at `num_predict` and lose every
/// fact in it. Off, the schema does not carry it at all.
pub async fn extract(
    client: &OllamaClient,
    text: &str,
    event_date_ms: Option<i64>,
    mission: Option<&str>,
    background: bool,
    known: &[prompts::KnownFact],
) -> Result<Vec<parse::ParsedFact>, OllamaError> {
    let system = prompts::system_prompt(true, !known.is_empty());
    let mission_preamble = prompts::retain_mission_preamble(mission);
    let user = prompts::user_message(&mission_preamble, event_date_ms, None, text, known);
    let schema = output_schema(!known.is_empty());

    let raw: parse::RawFactsResponse = if background {
        client.chat_json_background(&system, &user, &schema).await?
    } else {
        client.chat_json(&system, &user, &schema).await?
    };
    Ok(parse::parse_facts(raw.into_facts(), event_date_ms, known))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ollama::OllamaClient;

    /// Every string the model writes is bounded, in both schema arms — an
    /// unbounded one is where a looping reply runs until `num_predict` cuts
    /// the chunk off. Enums are already bounded by their values.
    #[test]
    fn every_free_string_field_is_bounded() {
        fn walk(v: &Value, path: &str, unbounded: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    if m.get("type") == Some(&json!("string"))
                        && !m.contains_key("enum")
                        && !m.contains_key("maxLength")
                    {
                        unbounded.push(path.to_string());
                    }
                    for (k, child) in m {
                        walk(child, &format!("{path}/{k}"), unbounded);
                    }
                }
                Value::Array(a) => a.iter().for_each(|c| walk(c, path, unbounded)),
                _ => {}
            }
        }
        for supersession in [false, true] {
            let schema = output_schema(supersession);
            let mut unbounded = Vec::new();
            walk(&schema, "", &mut unbounded);
            assert!(
                unbounded.is_empty(),
                "unbounded string fields: {unbounded:?}"
            );
            assert!(crate::ollama::max_string_len(&schema) <= crate::ollama::GRAMMAR_MAX_CHARS);
        }
    }

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
        let facts = extract(&client, text, Some(1_754_100_000_000), None, false, &[])
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
