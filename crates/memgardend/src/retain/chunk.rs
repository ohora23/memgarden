//! Structure-preserving chunking, ported from legacy
//! `hindsight-api-slim/hindsight_api/engine/retain/fact_extraction.py:494-660`.
//!
//! **Idempotence is the load-bearing property**: `chunk_text(chunk_text(x))
//! == chunk_text(x)`. The streaming retain pipeline pre-chunks a document
//! once and the extractor re-chunks every piece; if a piece re-split, the
//! sub-chunks would inherit one chunk index and collide (legacy issue #2301,
//! port-brief gotcha #6). Every branch below therefore emits pieces that are
//! already `<= max_chars`, which the length short-circuit at the top returns
//! unchanged on a second pass.
//!
//! One documented divergence: legacy delegates plain-text splitting to
//! LangChain's `RecursiveCharacterTextSplitter`. This is a hand-rolled
//! equivalent over the same separator ladder with `chunk_overlap = 0`, not a
//! byte-exact reimplementation of LangChain's regex/merge internals. It
//! guarantees what the pipeline actually depends on: every piece fits the
//! budget, earlier separators are preferred, and re-chunking is a no-op.
//! Serialized JSON also differs from Python's `json.dumps` by the latter's
//! `", "` / `": "` separators; sizing is self-consistent either way.

use serde_json::Value;

/// Separators for sentence-aware recursive splitting, most- to
/// least-preferred. The final `""` lets the splitter break mid-word as a
/// last resort so a chunk can never exceed the budget.
/// legacy: `_RECURSIVE_TEXT_SEPARATORS`, `fact_extraction.py:461-471`.
const SEPARATORS: [&str; 9] = ["\n\n", "\n", ". ", "! ", "? ", "; ", ", ", " ", ""];

/// Splits `text` into chunks of at most `max_chars` **characters**,
/// preserving conversation structure when the input is a JSON array of turn
/// objects.
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }

    // legacy passes `structured_chunk_size = None`, which defaults to
    // `max_chars` (`fact_extraction.py:517`).
    let structured_limit = max_chars;

    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(turns)) if turns.iter().all(Value::is_object) => {
            chunk_conversation(&turns, max_chars, structured_limit)
        }
        // A lone JSON object is one structured unit the producer deliberately
        // kept whole. We only get here when it is already longer than
        // `max_chars` (the short-circuit above), and `structured_limit ==
        // max_chars`, so the "keep it whole" arm legacy has is unreachable
        // for us and is not written out (review LOW 12). The branch still
        // has to exist: without it a lone object would fall through to JSONL
        // detection and then plain-text splitting on a chunk the producer
        // deliberately kept whole — breaking idempotence (#2301).
        Ok(Value::Object(_)) => split_oversized_unit(text, max_chars),
        _ => match chunk_jsonl(text, max_chars, structured_limit) {
            Some(chunks) => chunks,
            None => split_oversized_unit(text, max_chars),
        },
    }
}

/// Packs whole turns into chunks; a turn too large to stand alone is flushed
/// and split as text. legacy: `_chunk_conversation`, `fact_extraction.py:555-603`.
fn chunk_conversation(turns: &[Value], max_chars: usize, structured_limit: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    let mut current_size = 2usize; // accounts for "[]"

    for turn in turns {
        let turn_json = serde_json::to_string(turn).unwrap_or_default();
        let unit_size = turn_json.chars().count();
        let turn_size = unit_size + 1; // +1 for the comma separator

        // `+2` for the enclosing "[]" (review LOW 11): legacy compares the
        // bare turn against the limit, which lets a turn of exactly
        // `max_chars` produce a `max_chars + 2` chunk — and a chunk over the
        // budget re-splits on the next pass, breaking idempotence.
        if unit_size + 2 > structured_limit {
            flush(&mut chunks, &mut current, &mut current_size);
            chunks.extend(split_oversized_unit(
                &turn_json,
                structured_limit.min(max_chars),
            ));
            continue;
        }
        if current_size + turn_size > max_chars && !current.is_empty() {
            flush(&mut chunks, &mut current, &mut current_size);
        }
        current.push(turn.clone());
        current_size += turn_size;
    }
    flush(&mut chunks, &mut current, &mut current_size);

    if chunks.is_empty() {
        return vec![serde_json::to_string(turns).unwrap_or_default()];
    }
    chunks
}

fn flush(chunks: &mut Vec<String>, current: &mut Vec<Value>, current_size: &mut usize) {
    if current.is_empty() {
        return;
    }
    chunks.push(serde_json::to_string(&Value::Array(std::mem::take(current))).unwrap_or_default());
    *current_size = 2;
}

/// Newline-delimited JSON objects (two or more lines, each a complete
/// object) packed at line boundaries. `None` when the input is not JSONL, so
/// the caller falls back to plain-text splitting.
/// legacy: `_chunk_jsonl`, `fact_extraction.py:606-668`.
fn chunk_jsonl(text: &str, max_chars: usize, structured_limit: usize) -> Option<Vec<String>> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 2 {
        return None;
    }
    for line in &lines {
        match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(_)) => {}
            _ => return None,
        }
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut current_size = 0usize;

    for line in lines {
        let unit_size = line.chars().count();
        let line_size = unit_size + 1; // +1 for the joining newline
        if unit_size > structured_limit {
            if !current.is_empty() {
                chunks.push(current.join("\n"));
                current.clear();
                current_size = 0;
            }
            chunks.extend(split_oversized_unit(line, structured_limit.min(max_chars)));
            continue;
        }
        if current_size + line_size > max_chars && !current.is_empty() {
            chunks.push(current.join("\n"));
            current.clear();
            current_size = 0;
        }
        current.push(line);
        current_size += line_size;
    }
    if !current.is_empty() {
        chunks.push(current.join("\n"));
    }
    Some(chunks)
}

/// Sentence-aware recursive split of one unit that overflowed the budget.
/// legacy: `_split_oversized_unit`, `fact_extraction.py:474-491`.
fn split_oversized_unit(text: &str, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    split_recursive(text, max_chars, &SEPARATORS, &mut out);
    out.retain(|c| !c.trim().is_empty());
    if out.is_empty() {
        out.push(text.to_string());
    }
    out
}

fn split_recursive(text: &str, max_chars: usize, seps: &[&str], out: &mut Vec<String>) {
    if text.chars().count() <= max_chars {
        if !text.trim().is_empty() {
            out.push(text.trim().to_string());
        }
        return;
    }
    // Pick the first separator that actually occurs; "" (the last rung) is
    // the hard character split.
    let (idx, sep) = seps
        .iter()
        .enumerate()
        .find(|(_, s)| s.is_empty() || text.contains(**s))
        .map(|(i, s)| (i, *s))
        .unwrap_or((seps.len() - 1, ""));

    if sep.is_empty() {
        let chars: Vec<char> = text.chars().collect();
        for piece in chars.chunks(max_chars) {
            let s: String = piece.iter().collect();
            if !s.trim().is_empty() {
                out.push(s);
            }
        }
        return;
    }

    // Keep the separator attached to the piece it terminates, so rejoining
    // is lossless in the common "\n\n" / ". " cases.
    let raw = text.split(sep);
    let count = raw.clone().count();
    let pieces: Vec<String> = raw
        .enumerate()
        .map(|(i, p)| {
            if i + 1 < count {
                format!("{p}{sep}")
            } else {
                p.to_string()
            }
        })
        .collect();

    let rest = &seps[idx + 1..];
    let mut buf = String::new();
    for piece in pieces {
        if piece.chars().count() > max_chars {
            if !buf.trim().is_empty() {
                out.push(buf.trim().to_string());
            }
            buf.clear();
            split_recursive(&piece, max_chars, rest, out);
            continue;
        }
        if buf.chars().count() + piece.chars().count() > max_chars && !buf.is_empty() {
            if !buf.trim().is_empty() {
                out.push(buf.trim().to_string());
            }
            buf.clear();
        }
        buf.push_str(&piece);
    }
    if !buf.trim().is_empty() {
        out.push(buf.trim().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_idempotent(text: &str, max: usize) {
        let once = chunk_text(text, max);
        for chunk in &once {
            assert_eq!(
                chunk_text(chunk, max),
                vec![chunk.clone()],
                "re-chunking a chunk must be a no-op (legacy issue #2301)"
            );
            assert!(
                chunk.chars().count() <= max,
                "chunk of {} chars exceeds max {max}",
                chunk.chars().count()
            );
        }
        let twice: Vec<String> = once.iter().flat_map(|c| chunk_text(c, max)).collect();
        assert_eq!(once, twice);
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk_text("hello", 3000), vec!["hello".to_string()]);
    }

    #[test]
    fn plain_text_splits_on_paragraphs_and_is_idempotent() {
        let text = (0..40)
            .map(|i| format!("Paragraph number {i}. It has a couple of sentences. Here is another one."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_text(&text, 300);
        assert!(chunks.len() > 5);
        assert_idempotent(&text, 300);
    }

    #[test]
    fn conversation_array_splits_at_turn_boundaries() {
        let turns: Vec<Value> = (0..20)
            .map(|i| json!({ "role": "user", "content": format!("turn {i} {}", "x".repeat(200)) }))
            .collect();
        let text = serde_json::to_string(&turns).unwrap();
        let chunks = chunk_text(&text, 1000);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            let parsed: Value = serde_json::from_str(chunk)
                .expect("each conversation chunk must stay a valid JSON array");
            assert!(parsed.is_array(), "no turn is split across chunks");
        }
        assert_idempotent(&text, 1000);
    }

    #[test]
    fn oversized_single_turn_is_split_as_text() {
        let turns = json!([{ "role": "user", "content": "y".repeat(5000) }]);
        let text = serde_json::to_string(&turns).unwrap();
        let chunks = chunk_text(&text, 500);
        assert!(chunks.len() > 5);
        assert_idempotent(&text, 500);
    }

    #[test]
    fn jsonl_packs_at_line_boundaries() {
        let text = (0..30)
            .map(|i| serde_json::to_string(&json!({ "i": i, "pad": "z".repeat(100) })).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_text(&text, 600);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            for line in chunk.lines() {
                serde_json::from_str::<Value>(line).expect("no JSONL line split across chunks");
            }
        }
        assert_idempotent(&text, 600);
    }

    #[test]
    fn hard_split_when_no_separator_exists() {
        let text = "q".repeat(1000);
        let chunks = chunk_text(&text, 250);
        assert_eq!(chunks.len(), 4);
        assert!(chunks.iter().all(|c| c.chars().count() == 250));
        assert_idempotent(&text, 250);
    }

    #[test]
    fn korean_text_counts_characters_not_bytes() {
        // 1200 Hangul syllables = 3600 bytes. Chunking on bytes would produce
        // 2 chunks and could slice a multi-byte codepoint; chars gives 3.
        let text = "가".repeat(1200);
        let chunks = chunk_text(&text, 400);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() == 400));
    }

    #[test]
    fn single_json_object_kept_whole_under_the_limit() {
        let obj = json!({ "role": "user", "content": "w".repeat(400) });
        let text = serde_json::to_string(&obj).unwrap();
        // Longer than 100 -> would normally split, but a lone object is one
        // structured unit and the structured limit equals max_chars, so it is
        // split as text only when it exceeds it.
        assert_eq!(chunk_text(&text, 10_000), vec![text.clone()]);
        assert_idempotent(&text, 200);
    }
}
