//! `include_tool_calls` A/B — the real ingest path, one variable.
//!
//! Feeds identical messages through `retain::plan_ingest` + `chunk::chunk_text`
//! twice, differing only in `include_tool_calls`, so the comparison cannot be
//! an artefact of a hand-rolled harness (the previous attempt truncated chunks
//! mid-JSON and produced two unusable numbers).
//!
//! `--plan-only` stops before Ollama: transcript size, token counts and chunk
//! counts are deterministic and free. Without it, every chunk is extracted.
//!
//! Usage:
//!   cargo run -p memgardend --example tool_calls_ab -- --plan-only <transcript.jsonl>...
//!   cargo run -p memgardend --example tool_calls_ab -- --out <dir> <transcript.jsonl>...

use std::path::Path;

use memgardend::extract;
use memgardend::ollama::OllamaClient;
use memgardend::retain::{chunk, plan_ingest};
use serde_json::Value;

/// Same rule as `memgarden-cli`'s `transcript::classify`: `user`/`assistant`
/// entries carrying a non-empty `message.role`, and the flat `{role, content}`
/// shape. Everything else is skipped, future types included.
fn messages_from(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("read transcript");
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(object) = entry.as_object() else {
            continue;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("user" | "assistant") => {
                let has_role = object
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    .is_some_and(|r| !r.is_empty());
                if has_role {
                    out.push(object["message"].clone());
                }
            }
            None if object.contains_key("role") && object.contains_key("content") => {
                out.push(entry.clone());
            }
            _ => {}
        }
    }
    out
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let plan_only = args.iter().any(|a| a == "--plan-only");
    let out_dir = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let files: Vec<&String> = args
        .iter()
        .filter(|a| a.ends_with(".jsonl"))
        .collect();
    assert!(!files.is_empty(), "give at least one transcript .jsonl");

    let base = memgarden_core::config::Config::defaults().expect("defaults");
    if let Some(d) = &out_dir {
        std::fs::create_dir_all(d).expect("out dir");
    }

    println!("session,arm,messages,raw_tokens,capped_tokens,chars,chunks");
    for file in &files {
        let path = Path::new(file.as_str());
        let session = path.file_stem().unwrap().to_string_lossy();
        let messages = messages_from(path);

        for (arm, include) in [("with_tools", true), ("text_only", false)] {
            let mut cfg = base.retain.clone();
            cfg.include_tool_calls = include;

            let Some(plan) = plan_ingest(&messages, "/repo", true, &cfg) else {
                println!("{session},{arm},{},0,0,0,0", messages.len());
                continue;
            };
            let chunks = chunk::chunk_text(&plan.transcript, base.retain.chunk_size);
            println!(
                "{session},{arm},{},{},{},{},{}",
                plan.message_count,
                plan.raw_tokens,
                plan.capped_tokens,
                plan.transcript.chars().count(),
                chunks.len()
            );

            if plan_only {
                continue;
            }
            let client = OllamaClient::new(base.ollama.clone()).expect("ollama client");
            let mut facts = Vec::new();
            for (i, c) in chunks.iter().enumerate() {
                match extract::extract(
                    &client,
                    c,
                    Some(memgarden_core::now_ms()),
                    Some(base.profile.retain_mission.as_str()),
                    false,
                )
                .await
                {
                    Ok(fs) => {
                        eprintln!("  {session}/{arm} chunk {}/{}: {} facts", i + 1, chunks.len(), fs.len());
                        for f in fs {
                            facts.push(serde_json::json!({
                                "chunk": i,
                                "text": f.text,
                                "fact_type": format!("{:?}", f.fact_type),
                            }));
                        }
                    }
                    Err(e) => eprintln!("  {session}/{arm} chunk {}/{}: ERROR {e}", i + 1, chunks.len()),
                }
            }
            if let Some(d) = &out_dir {
                let p = format!("{d}/{session}.{arm}.json");
                std::fs::write(&p, serde_json::to_string_pretty(&facts).unwrap()).expect("write");
                eprintln!("  -> {p} ({} facts)", facts.len());
            }
        }
    }
}
