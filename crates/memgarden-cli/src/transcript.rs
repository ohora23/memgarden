//! Reading a Claude Code transcript by byte offset.
//!
//! This module is pure: no network, no config, no state file, no clock. That
//! is why it is its own PR — the reader is the part that can be exhaustively
//! tested against a real fixture, and the cursor state machine (C4b) is the
//! part that needs undistracted review.
//!
//! # The one correctness detail
//!
//! **Claude Code appends to the transcript while we read it.** A `Stop` hook
//! fires the instant the assistant's turn ends, and the process that wrote
//! that turn is still flushing. So the last line in the file is routinely a
//! *partially written record*: valid bytes, no terminating `\n`, and often
//! truncated mid-token or mid-UTF-8-character.
//!
//! Consuming such a line would advance the cursor past bytes that were never
//! read, and those bytes are then lost forever — the transcript is the only
//! spool (plan §Binding decisions #9). So: **a line that does not end in `\n`
//! is not consumed.** [`Delta::consumed_to`] only ever advances over
//! newline-terminated lines, and the partial line is picked up whole on the
//! next call.
//!
//! Two consequences shape the code below:
//!
//! * Reading is done with [`BufRead::read_until`] into a `Vec<u8>`, not
//!   `read_line` into a `String`. `read_line` requires valid UTF-8 and a
//!   torn multi-byte character at EOF makes it return `InvalidData` *after*
//!   consuming the bytes from the buffered reader. `read_until` cannot fail
//!   that way, and `serde_json::from_slice` takes bytes directly, so no
//!   `&str` is ever indexed. (C3 found the neighbouring version of this bug:
//!   the plan's "truncate to 800" is `&s[..800]` in Rust, which *panics* on a
//!   non-boundary index — in a process whose whole contract is that it cannot
//!   fail loudly. Nothing here slices a string.)
//! * The reader never seeks backwards mid-parse and never re-reads a line.
//!
//! # What comes out
//!
//! `type ∈ {user, assistant}` entries contribute their `message` object,
//! matching `retain.py:56-63`. The flat `{role, content}` shape legacy also
//! accepts (`retain.py:64-65`) is accepted too; it exists for tests.
//!
//! `{"type":"system","subtype":"compact_boundary"}` increments a **counter**
//! and does nothing else — plan §Binding decisions #6. It does not reset the
//! offset (the file is append-only, so the compaction summary is *new*
//! content we want) and it does not drive `chunk`.
//!
//! Every other entry type is skipped.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// `BufReader` capacity. One page would make a 100 MB transcript ~25,000
/// `read` syscalls; 1 MiB makes it ~100. Measured whole-file reads with this
/// buffer: 21 ms for 19.7 MB, 124 ms for 106.9 MB.
pub const READ_BUFFER_BYTES: usize = 1024 * 1024;

/// What one `read_delta` call found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Delta {
    /// The `message` objects, in file order, ready to post.
    pub messages: Vec<Value>,
    /// The byte offset the caller may commit **after** the post is accepted.
    /// Always a line boundary, always `>= from_offset`.
    pub consumed_to: u64,
    /// `compact_boundary` lines seen in this delta. A counter, nothing more.
    pub compactions: u64,
    /// Whether the oversize fallback dropped leading messages. See
    /// [`read_delta`]; when this is set the daemon never sees the dropped
    /// bytes, so `retain_cap_saving` under-reports.
    pub truncated: bool,
}

impl Delta {
    /// Nothing read, cursor unmoved. The answer for a missing, unreadable or
    /// already-fully-consumed transcript — all of which are ordinary, not
    /// errors: `session-start` fires before the file exists, and 9 of every
    /// 10 `Stop`s are gated out before they get here anyway.
    fn empty_at(offset: u64) -> Self {
        Delta {
            messages: Vec::new(),
            consumed_to: offset,
            compactions: 0,
            truncated: false,
        }
    }

    /// Exact serialized length of `messages` as a JSON array — what the
    /// oversize fallback is actually bounding. Present so tests assert
    /// against the real number rather than against the reader's bookkeeping.
    pub fn body_bytes(&self) -> usize {
        serde_json::to_vec(&self.messages).map_or(0, |v| v.len())
    }
}

/// Reads everything appended since `from_offset`.
///
/// `max_post_bytes` bounds the serialized `messages` array (24 MB by default,
/// under the daemon's 32 MB `MAX_RETAIN_BODY_BYTES`). When the delta would
/// exceed it, **leading messages are dropped until it fits** and
/// [`Delta::truncated`] is set. That is not a nicety: the 106.9 MB transcript
/// on this machine would otherwise 413 on every attempt, forever, and the
/// cursor would never advance past it.
///
/// The plan specifies this as "re-read backwards: scan from EOF for the
/// largest whole-line suffix that fits". This is that suffix, computed by a
/// bounded forward window instead of a second backwards pass — same result,
/// one pass, and peak memory is the *cap* rather than the *file*. See
/// `docs/design/c4a-transcript-delta.md`.
///
/// Never returns an error. An unreadable file, a seek past EOF and a corrupt
/// line are all "nothing to send", because the only correct handling of any
/// error in a hook is to exit 0 and try again next turn.
pub fn read_delta(path: &Path, from_offset: u64, max_post_bytes: usize) -> Delta {
    let Ok(file) = File::open(path) else {
        return Delta::empty_at(from_offset);
    };
    let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, file);
    // Seeking past EOF is legal and reads return 0 — which is the right
    // answer. A caller whose cursor is genuinely past the end of a rewritten
    // file resets it via the `size < offset` guard (§Binding decisions #6),
    // and that guard is C4b's, not ours.
    if reader.seek(SeekFrom::Start(from_offset)).is_err() {
        return Delta::empty_at(from_offset);
    }

    let mut delta = Delta::empty_at(from_offset);
    // `(serialized_len + 1, message)`. The +1 is the comma that follows the
    // message inside the array, which makes the array's exact serialized
    // length `total + 1` (the leading `[`). Tracking it here rather than at
    // the end is what keeps the window's memory bounded by the cap.
    let mut window: VecDeque<(usize, Value)> = VecDeque::new();
    let mut total: usize = 0;
    let mut line: Vec<u8> = Vec::with_capacity(8 * 1024);

    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else {
            // A mid-file read error. Keep what we have; the bytes we did not
            // read stay unconsumed and the next call retries from there.
            break;
        };
        if read == 0 {
            break; // EOF.
        }
        if line.last() != Some(&b'\n') {
            // THE line. Partially written record — see the module docs.
            break;
        }
        delta.consumed_to += read as u64;

        let Some(message) = classify(&line, &mut delta.compactions) else {
            continue;
        };

        // `to_vec` rather than a counting writer: same serialization work,
        // zero extra code, and one short-lived allocation per message that
        // malloc does not notice. Serializing to measure costs ~2x the read
        // on the initial full-transcript pass and nothing measurable on a
        // steady-state 200 KB delta — see the design note's measurements.
        // ponytail: measure by serializing; switch to `RawValue` (borrowed
        // from `line`, zero re-encode) if the initial pass ever needs the
        // ~40 ms back.
        let size = serde_json::to_vec(&message).map_or(0, |v| v.len()) + 1;
        total += size;
        window.push_back((size, message));

        // The suffix rule. Note `!window.is_empty()` rather than
        // `window.len() > 1`: a single message larger than the whole cap is
        // dropped too, leaving an empty delta rather than a body that is
        // guaranteed to 413.
        while total + 1 > max_post_bytes && !window.is_empty() {
            let (dropped, _) = window.pop_front().expect("non-empty");
            total -= dropped;
            delta.truncated = true;
        }
    }

    delta.messages = window.into_iter().map(|(_, message)| message).collect();
    delta
}

/// One line → an optional message, bumping `compactions` on the way past.
///
/// Returns `None` for every entry the plan's census lists as skipped
/// (`attachment`, `system`, `queue-operation`, `pr-link`, `ai-title`,
/// `agent-name`, `mode`, `permission-mode`, `last-prompt`,
/// `file-history-snapshot`, `file-history-delta`) — and for any *future*
/// type too. The skip is written as a catch-all rather than as that literal
/// list on purpose: an entry type Claude Code adds next month is skipped
/// rather than half-read, which is the direction a memory system should fail
/// in.
fn classify(line: &[u8], compactions: &mut u64) -> Option<Value> {
    let entry: Value = serde_json::from_slice(line).ok()?;
    let object = entry.as_object()?;

    match object.get("type").and_then(Value::as_str) {
        Some("user" | "assistant") => {
            let message = object.get("message")?;
            // `retain.py:56-63`: a dict with a truthy `role`. `Value::get`
            // returns `None` for a string index into a non-object, so the
            // `isinstance(msg, dict)` half is free.
            let role = message.get("role").and_then(Value::as_str)?;
            (!role.is_empty()).then(|| message.clone())
        }
        Some("system") => {
            if object.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                *compactions += 1;
            }
            None
        }
        Some(_) => None,
        // No `type` at all: the flat `{role, content}` shape
        // (`retain.py:64-65`). Legacy reaches this branch from an `elif`, so
        // it would also accept a *typed* entry carrying top-level `role` and
        // `content`; we do not. Measured: 0 such entries in 6,460 lines of
        // the live transcript, so this is a shape that does not occur rather
        // than a behaviour that changed.
        None => {
            (object.contains_key("role") && object.contains_key("content")).then(|| entry.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("transcript.jsonl");
        let mut f = File::create(&path).expect("create");
        f.write_all(body.as_bytes()).expect("write");
        path
    }

    const CAP: usize = 24 * 1024 * 1024;

    #[test]
    fn a_missing_file_is_an_empty_delta_at_the_same_offset() {
        let delta = read_delta(Path::new("/nonexistent/transcript.jsonl"), 4242, CAP);
        assert_eq!(delta, Delta::empty_at(4242));
        assert_eq!(delta.consumed_to, 4242);
    }

    #[test]
    fn seeking_past_the_end_reads_nothing_and_moves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\"}}\n",
        );
        let delta = read_delta(&path, 9_000, CAP);
        assert!(delta.messages.is_empty());
        assert_eq!(delta.consumed_to, 9_000);
    }

    /// The invariant this whole module exists for.
    #[test]
    fn a_line_without_a_trailing_newline_is_not_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let whole = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"first\"}}\n";
        // Valid JSON, no newline: exactly what a flushing writer leaves.
        let partial = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"second\"}}";
        let path = write(&dir, &format!("{whole}{partial}"));

        let delta = read_delta(&path, 0, CAP);
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0]["content"], "first");
        // Distinguishable: the whole line's length, not "less than the file".
        assert_eq!(delta.consumed_to, whole.len() as u64);

        // The writer finishes. The same record is now picked up whole, once.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"\n").unwrap();
        let next = read_delta(&path, delta.consumed_to, CAP);
        assert_eq!(next.messages.len(), 1);
        assert_eq!(next.messages[0]["content"], "second");
        assert_eq!(
            next.consumed_to,
            (whole.len() + partial.len() + 1) as u64,
            "the resumed read must land on the new end of file"
        );
    }

    /// The nastier half: the partial line is not merely unterminated, it is
    /// cut mid-UTF-8. `read_line` would return `InvalidData` here having
    /// already eaten the bytes.
    #[test]
    fn a_line_cut_mid_utf8_character_is_not_consumed_and_survives_completion() {
        let dir = tempfile::tempdir().unwrap();
        let whole = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"안녕\"}}\n";
        let tail = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"한국어\"}}\n";
        let path = dir.path().join("t.jsonl");

        // Cut inside the last character's 3-byte encoding.
        let bytes = tail.as_bytes();
        let cut = bytes.len() - 6;
        assert!(std::str::from_utf8(&bytes[..cut]).is_err(), "must be torn");
        let mut f = File::create(&path).unwrap();
        f.write_all(whole.as_bytes()).unwrap();
        f.write_all(&bytes[..cut]).unwrap();
        drop(f);

        let delta = read_delta(&path, 0, CAP);
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0]["content"], "안녕");
        assert_eq!(delta.consumed_to, whole.len() as u64);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&bytes[cut..]).unwrap();
        let next = read_delta(&path, delta.consumed_to, CAP);
        assert_eq!(next.messages.len(), 1);
        assert_eq!(next.messages[0]["content"], "한국어");
        assert_eq!(next.consumed_to, (whole.len() + tail.len()) as u64);
    }

    /// The skip list is load-bearing, not decorative.
    ///
    /// Found by mutation: adding `attachment` to the kept-types arm survived
    /// the whole suite, because no real `attachment` entry happens to carry a
    /// `message.role` and the `role` requirement was silently doing the work.
    /// An entry type is excluded because of its **type**.
    #[test]
    fn a_skipped_type_is_skipped_even_when_it_carries_a_usable_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "{\"type\":\"attachment\",\"message\":{\"role\":\"user\",\"content\":\"nope\"}}\n\
             {\"type\":\"queue-operation\",\"message\":{\"role\":\"user\",\"content\":\"nope\"}}\n\
             {\"type\":\"file-history-snapshot\",\"message\":{\"role\":\"assistant\",\"content\":\"nope\"}}\n\
             {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"kept\"}}\n",
        );
        let delta = read_delta(&path, 0, CAP);
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0]["content"], "kept");
    }

    /// The one deliberate divergence from `retain.py:56-65`.
    ///
    /// Legacy reaches the flat-shape branch from an `elif`, so a *typed*
    /// entry carrying top-level `role` and `content` is kept by legacy and
    /// skipped by us. Measured: 0 such entries in 6,460 lines of the live
    /// transcript and 0 in 7,338 of the 106.9 MB one. Pinned so the
    /// divergence stays deliberate.
    #[test]
    fn a_typed_entry_with_top_level_role_and_content_is_skipped_unlike_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            // Both arms of the type match: `system` (which we branch on for
            // the compaction counter) and a type that reaches the catch-all.
            "{\"type\":\"system\",\"role\":\"user\",\"content\":\"legacy keeps this\"}\n\
             {\"type\":\"attachment\",\"role\":\"user\",\"content\":\"legacy keeps this too\"}\n",
        );
        assert!(read_delta(&path, 0, CAP).messages.is_empty());
    }

    /// The cap is a `<=`, checked from both sides.
    ///
    /// Found by mutation: `total > cap` (dropping the `+1` that accounts for
    /// the array's opening bracket) and dropping the per-message separator
    /// byte both survived a fallback test whose cap had slack in it. A cap
    /// with slack cannot distinguish them; a cap on the exact boundary can.
    #[test]
    fn the_cap_is_exact_on_both_sides_of_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for i in 0..4 {
            body.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{i}\"}}}}\n"
            ));
        }
        let path = write(&dir, &body);

        let exact = read_delta(&path, 0, CAP).body_bytes();

        let at = read_delta(&path, 0, exact);
        assert_eq!(at.messages.len(), 4, "a body of exactly the cap fits");
        assert!(!at.truncated);
        assert_eq!(at.body_bytes(), exact);

        let one_byte_tighter = exact - 1;
        let under = read_delta(&path, 0, one_byte_tighter);
        assert!(under.truncated, "one byte over the cap must truncate");
        assert_eq!(under.messages.len(), 3);
        assert!(under.body_bytes() <= one_byte_tighter);
    }

    #[test]
    fn the_flat_role_content_shape_is_accepted_and_kept_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "{\"role\":\"user\",\"content\":\"flat\",\"extra\":1}\n\
             {\"role\":\"assistant\"}\n",
        );
        let delta = read_delta(&path, 0, CAP);
        // The second line has no `content`, so legacy drops it and so do we.
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0]["content"], "flat");
        assert_eq!(delta.messages[0]["extra"], 1);
    }

    #[test]
    fn an_entry_whose_message_has_no_role_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "{\"type\":\"user\",\"message\":{\"content\":\"no role\"}}\n\
             {\"type\":\"user\",\"message\":{\"role\":\"\",\"content\":\"empty role\"}}\n\
             {\"type\":\"user\",\"message\":\"not an object\"}\n\
             {\"type\":\"assistant\"}\n\
             {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"kept\"}}\n",
        );
        let delta = read_delta(&path, 0, CAP);
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0]["content"], "kept");
        // Everything was still *consumed* — not advancing over lines we
        // deliberately skip would rescan them on every turn forever.
        assert_eq!(delta.consumed_to, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn corrupt_and_blank_lines_are_skipped_but_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let body = "\n\
                    not json at all\n\
                    [1,2,3]\n\
                    \"a bare string\"\n\
                    {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"ok\"}}\n";
        let path = write(&dir, body);
        let delta = read_delta(&path, 0, CAP);
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.consumed_to, body.len() as u64);
    }

    #[test]
    fn compact_boundaries_are_counted_and_neither_reset_nor_filter() {
        let dir = tempfile::tempdir().unwrap();
        let before = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"before\"}}\n";
        let boundary = "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"content\":\"Conversation compacted\"}\n";
        let other = "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n";
        let after = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"after\"}}\n";
        let path = write(&dir, &format!("{before}{boundary}{other}{after}{boundary}"));

        let delta = read_delta(&path, 0, CAP);
        assert_eq!(delta.compactions, 2);
        // Not a control signal: the messages on both sides survive, in order,
        // and the offset ran straight through.
        assert_eq!(delta.messages.len(), 2);
        assert_eq!(delta.messages[0]["content"], "before");
        assert_eq!(delta.messages[1]["content"], "after");
        assert_eq!(
            delta.consumed_to,
            (before.len() + boundary.len() + other.len() + after.len() + boundary.len()) as u64
        );
    }

    #[test]
    fn is_sidechain_is_not_filtered() {
        // Legacy has no such check (`retain.py:56-63`) and 0 of the live
        // transcript's 3,489 user+assistant entries set it, so filtering
        // would be an untested behaviour change with nothing to show for it.
        // Recorded as an open question in the design note, not a divergence.
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "{\"type\":\"user\",\"isSidechain\":true,\"message\":{\"role\":\"user\",\"content\":\"sub\"}}\n",
        );
        let delta = read_delta(&path, 0, CAP);
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0]["content"], "sub");
    }

    #[test]
    fn a_mid_file_offset_returns_exactly_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let head = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"one\"}}\n";
        let mid = "{\"type\":\"attachment\",\"payload\":\"skipped\"}\n";
        let tail =
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"two\"}}\n";
        let path = write(&dir, &format!("{head}{mid}{tail}"));

        let delta = read_delta(&path, head.len() as u64, CAP);
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0]["content"], "two");
        assert_eq!(
            delta.consumed_to,
            (head.len() + mid.len() + tail.len()) as u64
        );
    }

    #[test]
    fn the_oversize_fallback_keeps_the_largest_whole_line_suffix() {
        let dir = tempfile::tempdir().unwrap();
        // Four messages of ~100 bytes each, distinguishable by content so a
        // mutant that keeps the *prefix* cannot pass on a count assertion.
        let mut body = String::new();
        for i in 0..4 {
            body.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{}\",\"pad\":\"{}\"}}}}\n",
                i,
                "x".repeat(80)
            ));
        }
        let path = write(&dir, &body);

        let whole = read_delta(&path, 0, CAP);
        assert_eq!(whole.messages.len(), 4);
        assert!(!whole.truncated);
        let full_body = whole.body_bytes();

        // A cap that fits two of the four and not three.
        let per = (full_body - 2) / 4; // serialized length of one message
        let cap = per * 2 + 3;
        let delta = read_delta(&path, 0, cap);
        assert!(delta.truncated, "the fallback must announce itself");
        assert_eq!(delta.messages.len(), 2);
        // The *suffix*: 2 and 3, not 0 and 1.
        assert_eq!(delta.messages[0]["content"], "2");
        assert_eq!(delta.messages[1]["content"], "3");
        // The real serialized array, not the reader's own bookkeeping.
        assert!(delta.body_bytes() <= cap, "{} > {cap}", delta.body_bytes());
        // The cursor still reaches the end. Not advancing here is what makes
        // an oversize transcript 413 forever.
        assert_eq!(delta.consumed_to, body.len() as u64);
    }

    #[test]
    fn a_single_message_larger_than_the_cap_leaves_an_empty_truncated_delta() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}\n",
            "x".repeat(500)
        );
        let path = write(&dir, &body);
        let delta = read_delta(&path, 0, 64);
        assert!(delta.messages.is_empty());
        assert!(delta.truncated);
        assert_eq!(delta.consumed_to, body.len() as u64);
    }

    #[test]
    fn body_bytes_is_exactly_what_the_window_accounted_for() {
        // The +1-per-message bookkeeping is the only arithmetic in this
        // module; a mutant that drops it would still satisfy a `<= cap`
        // assertion, so it gets its own equality check.
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for i in 0..5 {
            body.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{i}\"}}}}\n"
            ));
        }
        let path = write(&dir, &body);
        let delta = read_delta(&path, 0, CAP);
        let accounted: usize = delta
            .messages
            .iter()
            .map(|m| serde_json::to_vec(m).unwrap().len() + 1)
            .sum::<usize>()
            + 1;
        assert_eq!(delta.body_bytes(), accounted);
    }
}
