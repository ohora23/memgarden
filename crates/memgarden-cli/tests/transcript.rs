//! The delta reader against a **real** transcript.
//!
//! `tests/fixtures/transcript-redacted.jsonl` is a 95-line slice of the live
//! 21 MB transcript this project is being developed in, sliced around a
//! compaction boundary. Free text, absolute paths and uuids are replaced;
//! entry types, key sets, content-block nesting and one multi-byte Korean
//! string are not. The plan is explicit about why a synthetic fixture would
//! not do: it would not have the property that actually matters, which is
//! that the file is **append-only** and is being appended to while we read.
//!
//! The unit tests in `src/transcript.rs` pin the branch behaviour one case at
//! a time. These pin the two whole-file properties:
//!
//! * splitting at *any* line boundary reconstructs the whole delta, and
//! * feeding the file in through a growing writer — cutting mid-line and
//!   mid-UTF-8 — loses and duplicates nothing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use memgarden_cli::transcript::{Delta, read_delta};
use serde_json::Value;

const CAP: usize = 24 * 1024 * 1024;

/// Counts taken from the fixture when it was cut. Exact, not bounds: a
/// `>= 1` assertion is satisfied by most of the mutants that would break
/// this reader.
const FIXTURE_LINES: usize = 95;
const FIXTURE_MESSAGES: usize = 35;
const FIXTURE_COMPACTIONS: u64 = 1;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcript-redacted.jsonl")
}

fn fixture_bytes() -> Vec<u8> {
    std::fs::read(fixture()).expect("fixture readable")
}

/// Byte offset just past each `\n`, i.e. every legal cursor position.
fn line_boundaries(bytes: &[u8]) -> Vec<u64> {
    bytes
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .map(|(i, _)| i as u64 + 1)
        .collect()
}

#[test]
fn the_fixture_is_the_transcript_it_claims_to_be() {
    let bytes = fixture_bytes();
    assert_eq!(line_boundaries(&bytes).len(), FIXTURE_LINES);
    assert_eq!(*bytes.last().expect("non-empty"), b'\n');

    // Every entry type from the plan's §Verified Environment Facts census is
    // present, so "each skipped type is skipped" is exercised by real lines
    // rather than by lines written to make the assertion pass.
    let mut kinds: Vec<String> = String::from_utf8(bytes.clone())
        .expect("utf-8")
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).expect("every fixture line parses")["type"]
                .as_str()
                .expect("every entry is typed")
                .to_string()
        })
        .collect();
    kinds.sort();
    kinds.dedup();
    for expected in [
        "agent-name",
        "ai-title",
        "assistant",
        "attachment",
        "file-history-delta",
        "file-history-snapshot",
        "last-prompt",
        "mode",
        "permission-mode",
        "pr-link",
        "queue-operation",
        "system",
        "user",
    ] {
        assert!(kinds.contains(&expected.to_string()), "missing {expected}");
    }
    assert_eq!(kinds.len(), 13, "unexpected entry type in the fixture");
}

#[test]
fn a_whole_file_read_keeps_only_user_and_assistant_messages() {
    let delta = read_delta(&fixture(), 0, CAP);
    assert_eq!(delta.messages.len(), FIXTURE_MESSAGES);
    assert_eq!(delta.compactions, FIXTURE_COMPACTIONS);
    assert!(!delta.truncated);
    assert_eq!(delta.consumed_to, fixture_bytes().len() as u64);

    // What came out is the `message` object, not the entry: the entry-level
    // `toolUseResult` (1,034 of them in the live file, 40% of its bytes) is
    // gone, and every kept object has a role.
    for message in &delta.messages {
        let role = message["role"].as_str().expect("role");
        assert!(role == "user" || role == "assistant", "{role}");
        assert!(message.get("toolUseResult").is_none());
        assert!(message.get("uuid").is_none());
    }

    // The Korean string survived the round trip through `Vec<u8>` →
    // `serde_json` → `Value`. Nothing in this path indexes a `&str`.
    let text = serde_json::to_string(&delta.messages).unwrap();
    assert!(text.contains("한국어"), "the multi-byte content was lost");
}

#[test]
fn splitting_at_any_line_boundary_reconstructs_the_whole_delta() {
    let bytes = fixture_bytes();
    let whole = read_delta(&fixture(), 0, CAP);

    for cut in line_boundaries(&bytes) {
        let head = read_delta(&fixture(), 0, CAP);
        // A head read stops nowhere by itself, so bound it the way a real
        // caller does: read the head from 0 and the tail from `cut`, and
        // check the tail is exactly the whole minus the head-up-to-`cut`.
        let tail = read_delta(&fixture(), cut, CAP);
        assert_eq!(tail.consumed_to, whole.consumed_to, "cut={cut}");
        assert_eq!(head.messages, whole.messages);

        let head_msgs = messages_before(&bytes, cut);
        assert_eq!(
            head_msgs + tail.messages.len(),
            FIXTURE_MESSAGES,
            "cut={cut}: {head_msgs} + {} != {FIXTURE_MESSAGES}",
            tail.messages.len()
        );
        assert_eq!(
            tail.messages,
            whole.messages[head_msgs..].to_vec(),
            "cut={cut}: the tail is not the suffix"
        );
    }
}

/// How many messages live strictly before `cut`, computed independently of
/// the reader so the test is not checking the reader against itself.
fn messages_before(bytes: &[u8], cut: u64) -> usize {
    String::from_utf8(bytes[..cut as usize].to_vec())
        .expect("cut on a line boundary is valid utf-8")
        .lines()
        .filter(|l| {
            let e: Value = serde_json::from_str(l).expect("parses");
            matches!(e["type"].as_str(), Some("user" | "assistant"))
                && e["message"]["role"].as_str().is_some_and(|r| !r.is_empty())
        })
        .count()
}

/// The append-only property, which is the reason the fixture is real.
///
/// The fixture is replayed into a growing file in 7,919-byte chunks — a prime
/// chosen so the cuts land mid-line and, several times, mid-UTF-8. A reader
/// that consumed a partial line would drop the record it cut; one that
/// re-read a consumed line would duplicate it. Only the exact sequence
/// passes.
#[test]
fn a_file_replayed_through_a_growing_writer_loses_and_duplicates_nothing() {
    let bytes = fixture_bytes();
    let whole = read_delta(&fixture(), 0, CAP);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("growing.jsonl");
    File::create(&path).unwrap();

    let mut accumulated: Vec<Value> = Vec::new();
    let mut compactions = 0;
    let mut offset = 0u64;
    let mut reads_that_stopped_short = 0;

    for chunk in bytes.chunks(7_919) {
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(chunk).unwrap();
        drop(f);

        let delta = read_delta(&path, offset, CAP);
        assert!(delta.consumed_to >= offset);
        if delta.consumed_to < std::fs::metadata(&path).unwrap().len() {
            reads_that_stopped_short += 1;
        }
        accumulated.extend(delta.messages);
        compactions += delta.compactions;
        offset = delta.consumed_to;
    }

    assert!(
        reads_that_stopped_short >= 10,
        "only {reads_that_stopped_short} reads hit a partial line — the \
         chunking is not exercising the invariant"
    );
    assert_eq!(accumulated, whole.messages);
    assert_eq!(compactions, FIXTURE_COMPACTIONS);
    assert_eq!(offset, bytes.len() as u64);
}

#[test]
fn the_oversize_fallback_on_the_real_fixture_keeps_a_suffix_under_the_cap() {
    let whole = read_delta(&fixture(), 0, CAP);
    let cap = whole.body_bytes() / 2;

    let delta = read_delta(&fixture(), 0, cap);
    assert!(delta.truncated);
    assert!(delta.body_bytes() <= cap, "{} > {cap}", delta.body_bytes());
    // A suffix of the whole, not a prefix and not a resample.
    assert!(!delta.messages.is_empty());
    assert!(delta.messages.len() < whole.messages.len());
    let skipped = whole.messages.len() - delta.messages.len();
    assert_eq!(delta.messages, whole.messages[skipped..].to_vec());
    // Largest such suffix: one more message would not have fitted.
    let one_more = &whole.messages[skipped - 1..];
    assert!(
        serde_json::to_vec(one_more).unwrap().len() > cap,
        "the fallback dropped more than it had to"
    );
    // Compactions are counted over everything read, including the dropped
    // head — the counter is not a property of the payload.
    assert_eq!(delta.compactions, FIXTURE_COMPACTIONS);
    assert_eq!(delta.consumed_to, whole.consumed_to);
}

#[test]
fn a_torn_final_line_of_the_real_fixture_is_held_back() {
    let bytes = fixture_bytes();
    let boundaries = line_boundaries(&bytes);
    let last_full = boundaries[boundaries.len() - 2];

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.jsonl");
    // Everything, minus the final newline: the real shape of a transcript a
    // `Stop` hook opens while Claude Code is still flushing.
    File::create(&path)
        .unwrap()
        .write_all(&bytes[..bytes.len() - 1])
        .unwrap();

    let delta = read_delta(&path, 0, CAP);
    assert_eq!(delta.consumed_to, last_full);

    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let rest = read_delta(&path, delta.consumed_to, CAP);
    assert_eq!(rest.consumed_to, bytes.len() as u64);

    let mut joined = delta.messages;
    joined.extend(rest.messages);
    assert_eq!(joined, read_delta(&fixture(), 0, CAP).messages);
}

/// A `Delta` for a transcript that does not exist is not an error and does
/// not move the cursor — `session-start` runs before the file is created.
#[test]
fn the_absent_transcript_case_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let delta = read_delta(&dir.path().join("nope.jsonl"), 77, CAP);
    assert_eq!(
        delta,
        Delta {
            messages: vec![],
            consumed_to: 77,
            compactions: 0,
            truncated: false,
        }
    );
}

/// Measurement against a **live** transcript. Ignored by default because it
/// needs a file this repository cannot ship; run with
///
/// ```text
/// MEMGARDEN_LIVE_TRANSCRIPT=~/.claude/projects/<proj>/<sid>.jsonl \
///   cargo test -p memgarden-cli --test transcript -- --ignored --nocapture
/// ```
///
/// This stands in for the plan's `memgarden hook retain --dry-run`: that flag
/// needs a `retain` dispatch arm, and `lib.rs`'s dispatch is being edited by
/// C3 on a sibling branch. Adding one here would be a merge conflict in the
/// one file both PRs touch, for a diagnostic C4b re-implements anyway.
#[test]
#[ignore = "needs MEMGARDEN_LIVE_TRANSCRIPT"]
fn live_transcript_measurement() {
    let Some(path) = std::env::var_os("MEMGARDEN_LIVE_TRANSCRIPT") else {
        panic!("set MEMGARDEN_LIVE_TRANSCRIPT");
    };
    let path = PathBuf::from(path);
    let size = std::fs::metadata(&path).expect("stat").len();
    println!("\ntranscript: {} ({size} bytes)", path.display());
    println!(
        "| from_offset | delta bytes | body bytes | messages | compactions | truncated | wall | of which serialize |"
    );
    println!("|---|---|---|---|---|---|---|---|");

    for (label, from) in [
        ("0 (full file)", 0),
        ("size - 200 KB", size.saturating_sub(200 * 1024)),
        ("size (caught up)", size),
    ] {
        // One warm-up so the page cache is the same for every row.
        let _ = read_delta(&path, from, CAP);
        let start = std::time::Instant::now();
        let delta = read_delta(&path, from, CAP);
        let wall = start.elapsed();
        // One serialization pass over exactly the kept messages. `read_delta`
        // pays this same cost internally to size the window, so it is the
        // attribution for the gap between this row's wall time and the plan's
        // parse-only reference — measured, not reasoned about.
        let start = std::time::Instant::now();
        let body = delta.body_bytes();
        let serialize = start.elapsed();
        println!(
            "| {label} | {} | {body} | {} | {} | {} | {:.2} ms | {:.2} ms |",
            delta.consumed_to - from,
            delta.messages.len(),
            delta.compactions,
            delta.truncated,
            wall.as_secs_f64() * 1000.0,
            serialize.as_secs_f64() * 1000.0
        );
        assert_eq!(delta.consumed_to.max(from), delta.consumed_to);
    }
}
