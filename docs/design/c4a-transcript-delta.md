# C4a / HK-1e — the transcript delta reader

`crates/memgarden-cli/src/transcript.rs`. One function:

```rust
pub fn read_delta(path: &Path, from_offset: u64, max_post_bytes: usize) -> Delta
pub struct Delta { messages: Vec<Value>, consumed_to: u64, compactions: u64, truncated: bool }
```

Pure. No network, no config, no state file, no clock, no `unsafe`, no new
dependency. It was split out of the retain hook because the reader is the part
that can be exhaustively tested against a real fixture, and the cursor state
machine (C4b) is the part that needs undistracted review. Nothing of C4b's
cursor logic is here: `read_delta` reports `consumed_to`, it does not decide
whether to commit it.

---

## The one correctness detail

**Claude Code appends to the transcript while we read it.** A `Stop` hook fires
the instant the assistant's turn ends, and the process that wrote that turn is
still flushing. The last line of the file is routinely a partially written
record: valid bytes, no terminating `\n`, often cut mid-token.

So: **a line that does not end in `\n` is not consumed.** `consumed_to` only
advances over newline-terminated lines, and the partial record is picked up
whole on the next call. Consuming it would advance the cursor past bytes that
were never read, and those bytes are gone — the transcript is the only spool
(§Binding decisions #9).

Two things follow, and the second is a correction to the plan.

### `read_until`, not `read_line` — the plan names the wrong primitive

The plan says "`BufReader` with a 1 MiB buffer, `seek(SeekFrom::Start(from))`,
**`read_line`**". `read_line` is the wrong call for exactly the input this
invariant exists to handle.

`read_line` requires valid UTF-8. A transcript torn mid-multi-byte-character —
the common case, since Claude Code writes Korean and every tool result is
JSON-escaped UTF-8 — makes it return `ErrorKind::InvalidData`. The bytes are
already gone from the buffered reader at that point, and the error carries no
information about how many. A caller that treated the error as EOF and kept its
running offset would be correct only by accident; one that trusted the reader's
position would consume a record it never parsed.

`read_until(b'\n', &mut Vec<u8>)` cannot fail that way, and
`serde_json::from_slice` takes bytes, so **no `&str` is ever created or
indexed** anywhere in this module. That is the same defect class C3 found in
the plan's "truncate to 800" — `&s[..800]` panics on a non-boundary index — in
a process whose entire contract is that it cannot fail loudly.
`a_line_cut_mid_utf8_character_is_not_consumed_and_survives_completion` pins
it with a real 3-byte character cut at byte 2.

### The buffer

1 MiB, as specified. One page would make a 100 MB transcript ~25,000 `read`
syscalls; 1 MiB makes it ~100. `READ_BUFFER_BYTES` is `pub` so C4b's
measurement can name it.

---

## What comes out

| entry | outcome |
|---|---|
| `type: user` / `type: assistant` with `message.role` non-empty | the **`message` object** is kept (`retain.py:56-63`) |
| no `type`, but top-level `role` **and** `content` | the whole entry is kept (`retain.py:64-65`, the flat testing shape) |
| `type: system`, `subtype: compact_boundary` | `compactions += 1`, nothing else |
| everything else | skipped |

**Compaction is a counter and nothing else** (§Binding decisions #6). It does
not reset the offset — the file is append-only, so the compaction summary is
*new* content we want — and it does not drive `chunk`.

Note the ratio that makes the `subtype` check load-bearing: the live transcript
has **412 `system` entries of which 2 are `compact_boundary`** (the rest are
`turn_duration` 194, `stop_hook_summary` 171, `away_summary` 21,
`scheduled_task_fire` 21, `informational` 3). A reader that counted `system`
entries would report 412 compactions instead of 2. The plan says `subtype`
correctly; the census in §Verified Environment Facts does not break `system`
down, so the ratio is recorded here.

### The skip list is a catch-all, not the census list

The plan enumerates the types to skip (`attachment`, `system`,
`queue-operation`, `pr-link`, `ai-title`, `agent-name`, `mode`,
`permission-mode`, `last-prompt`, `file-history-snapshot`,
`file-history-delta`). The code matches `Some(_) => None` instead, so a type
Claude Code adds next month is skipped rather than half-read. Fail-closed is
the right direction for a memory system, and the fixture contains all 13 types
so the enumerated ones are exercised by real lines either way.

Mutation testing showed this arm was *not* doing the work it looked like it was
doing — see §Mutation evidence.

---

## The oversize fallback

If the serialized `messages` array would exceed `max_post_bytes` (24 MB,
under the daemon's 32 MB `MAX_RETAIN_BODY_BYTES`), leading messages are dropped
until it fits and `truncated` is set.

Required, not optional. Measured on the 106.9 MB transcript on this machine:

| | |
|---|---|
| file | 106,910,771 B (7,338 lines) |
| user+assistant entries | 4,671 |
| **uncapped body** | **52,629,089 B (50.2 MB)** |
| daemon limit | 33,554,432 B (32 MB) |
| after the fallback | 25,159,211 B, 2,457 messages, `truncated: true` |

50.2 MB against a 32 MB limit is a **413 on every attempt, forever**, with the
cursor never advancing past it. That is the failure the fallback exists to
prevent, and the number confirming it is measured, not assumed.

### Diverged from the plan: a forward window, not a backwards re-read

The plan says "re-read backwards: scan from EOF for the largest whole-line
suffix that fits". The result here is that same suffix, computed by a bounded
forward window in the pass we are already making:

* each kept message is pushed with its serialized length;
* while the running total exceeds the cap, messages are popped from the
  **front**.

Same answer, one pass instead of two, and peak memory is bounded by the **cap**
(24 MB) rather than by the **file** (106.9 MB). A backwards scan would also
have needed its own partial-line logic at the seek point, which is the code
this PR is most careful about and least keen to write twice.

### Diverged from the plan: the unit is body bytes, not file bytes

This one changes the answer, not just the route.

The plan's "largest whole-line **suffix**" bounds *file* bytes. But we post the
`message` objects, not the entries. The entry-level `toolUseResult` — 1,034 of
them in the live transcript — never leaves the machine. Measured message-to-line
byte ratios:

| transcript | u+a line bytes | body bytes | ratio |
|---|---|---|---|
| live, 21.4 MB | 17.1 MB | 10.3 MB | **0.601** |
| incident, 106.9 MB | 100.9 MB | 50.2 MB | **0.497** |

A backwards *line*-byte scan targeting 24 MB of lines on the incident
transcript would have kept ≈24 MB × 0.497 ≈ **11.9 MB of body — less than half
of the 25.16 MB that actually fits**, discarding around 1,200 messages that had
room. The reader measures the serialized message instead, which is the thing
the daemon's limit is actually about.

### Accounting, exactly

`total` accumulates `serialize(message).len() + 1` per message. The `+1` is the
comma that follows it inside the array, which makes the array's exact
serialized length `total + 1` (the opening `[`). So the test is `total + 1 >
max_post_bytes` and it is exact, not approximate — `Delta::body_bytes()` exposes
the real `serde_json::to_vec(&messages).len()` so tests assert against the
serialization rather than against the reader's bookkeeping, and
`the_cap_is_exact_on_both_sides_of_the_boundary` checks a body of exactly the
cap fits and one byte over does not. Both halves were found by mutation, not by
inspection.

Sizes come from `serde_json::to_vec(&message).len()`. That is a second
serialization pass, measured below; `// ponytail: measure by serializing;
switch to a borrowed `RawValue` (zero re-encode) if the initial pass ever needs
the ~10 ms back` — the upgrade needs `serde_json`'s `raw_value` feature, which
adds no crates but does change a dependency's build, so it is not being taken
for a cost that only lands once per session on the path the plan already
declares an exception.

---

## `isSidechain` — open question, not a divergence

Not filtered, matching legacy (`retain.py:56-63` has no such check).

**Measured, this PR:** `isSidechain` is present as a key on **all 3,493**
user+assistant entries of the live transcript and is `false` on all 3,493. (The
plan's census says 0 of 3,198; the file has grown since — 6,460 lines now, not
5,741 — and the count is still zero.) It is present and `false` on all **8,169** user+assistant
entries across both transcripts.

So filtering would be an untested behaviour change with no observable benefit
on any transcript available to measure. The open question is whether subagent
turns *should* be retained once a transcript that contains them exists; the
answer needs data this machine does not have. `is_sidechain_is_not_filtered`
pins current behaviour so a future change to it is deliberate.

---

## Known limits

### `retain_cap_saving` under-reports whenever `truncated` is set

The daemon computes the `benefit_ledger` `retain_cap_saving` row from the bytes
it receives. In the oversize path it **never sees the bytes we dropped**, so the
saving it records is measured against a payload that has already been cut. On
the 106.9 MB transcript the ledger would attribute its ratio to 25.16 MB of
input when 50.2 MB existed — the reported saving is real but the denominator is
not the transcript.

Stated plainly because an undocumented accepted risk is itself a finding: the
alternative is a 413 loop, and no ledger row at all. C4b posts `truncated` in
`metadata`, so the daemon side has what it needs to qualify the row if Phase F
wants it to.

### A single message larger than the cap is dropped entirely

The window pops until it fits, including popping a lone message that exceeds
the cap on its own, leaving an empty `truncated` delta. The cursor still
advances past it — not advancing is the 413 loop again, in a costlier form. In
practice the largest single `message` across both transcripts is **659 KB**
(675,261 B) against a 24 MB cap — a 38× margin, not a live risk. Pinned by
`a_single_message_larger_than_the_cap_leaves_an_empty_truncated_delta` so it
stays a decision.

### The window holds `Value`s, so peak memory is the cap plus one message

~24 MB of `serde_json::Value` is meaningfully more than 24 MB of RSS — a `Value`
tree costs several times its serialized size. This is bounded and it only
happens on the initial retain of an oversize transcript. `RawValue` would fix
this and the serialization cost in the same change; the re-entry criterion is
the same one.

### `consumed_to` advances over lines we skipped

Deliberate. A delta of 200 lines that are all `attachment` returns no messages
but a moved `consumed_to`; not advancing would rescan them on every `Stop`
forever. C4b's step 6 is written for exactly this. `read_delta` has no way to
distinguish "nothing interesting" from "nothing at all" and does not try —
`messages.is_empty() && consumed_to > from_offset` is the caller's signal.

### An unreadable file is silence

A missing file, a permission error, a failed seek and a mid-file read error all
return whatever was read so far with the cursor where it was. There is no error
channel out of a hook (§Binding decisions #2), and every one of these is
ordinary rather than exceptional: `session-start` fires before the transcript
exists.

---

## Diverged from legacy

1. **Byte offset, not message index.** Legacy re-reads and re-parses the whole
   file every retain and slices by message count (`retain.py`, `state.py`). We
   seek. §Binding decisions #6; the measurement below is the justification.
2. **A typed entry carrying top-level `role` and `content` is skipped.**
   Legacy reaches its flat-shape branch from an `elif`, so such an entry would
   be kept there. Measured: **0 occurrences in 13,831 lines** across both
   transcripts. Pinned by
   `a_typed_entry_with_top_level_role_and_content_is_skipped_unlike_legacy`.
3. **Compaction does not reset the cursor.** §Binding decisions #6, already
   ratified; recorded here because the reader is where it becomes visible —
   `compact_boundaries_are_counted_and_neither_reset_nor_filter` asserts that
   messages on both sides of a boundary come back in one delta.
4. **A partial trailing line is never consumed.** Legacy reads whole files, so
   it has no cursor to corrupt and no equivalent behaviour. This is new, not
   changed.

---

## Fixtures are real

`crates/memgarden-cli/tests/fixtures/transcript-redacted.jsonl` — 95 lines,
110,626 bytes, sliced out of the live transcript around a compaction boundary.

* **All 13 entry types** from the census are present.
* 35 user+assistant messages, 1 `compact_boundary`.
* Redacted: free text, absolute paths (including `file-history-snapshot`'s map
  **keys**, which is where the first pass leaked), uuids, git branch, cwd.
* Preserved: entry types, key sets, content-block nesting, `toolUseResult` at
  the entry level, and one multi-byte Korean string.

The plan is explicit about why this is not synthetic, and it was right for a
reason narrower than "realism": a synthetic fixture is a file you wrote, and a
file you wrote does not have the property that it is **being appended to**.
`a_file_replayed_through_a_growing_writer_loses_and_duplicates_nothing`
replays it into a growing file in 7,919-byte chunks — a prime, so cuts land
mid-line and repeatedly mid-UTF-8 — and asserts the accumulated messages equal
the whole-file read exactly. It also asserts that **at least 10 of those reads
stopped short of EOF**, so a chunking that stopped exercising the invariant
fails loudly instead of passing vacuously.

`splitting_at_any_line_boundary_reconstructs_the_whole_delta` does the other
half: for every one of the 95 legal cursor positions, the tail read from that
offset is exactly the suffix of the whole-file read. The expected split point is
computed by an independent parser in the test, not by `read_delta`, so the test
is not checking the reader against itself.

---

## Measurement

Release profile, Ryzen 7 9800X3D, warm page cache, one warm-up read per row.
Reproduce with:

```
MEMGARDEN_LIVE_TRANSCRIPT=<path> \
  cargo test --release -p memgarden-cli --test transcript -- --ignored --nocapture
```

**Live transcript, 21,473,373 B**

| from_offset | delta bytes | body bytes | messages | compactions | truncated | wall | of which serialize |
|---|---|---|---|---|---|---|---|
| 0 (full file) | 21,473,373 | 10,191,413 | 3,493 | 2 | false | **41.28 ms** | 9.91 ms |
| size − 200 KB | 204,800 | 93,919 | 28 | 0 | false | **0.38 ms** | 0.05 ms |
| size (caught up) | 0 | 2 | 0 | 0 | false | **0.00 ms** | 0.00 ms |

**Incident transcript, 106,910,771 B**

| from_offset | delta bytes | body bytes | messages | compactions | truncated | wall | of which serialize |
|---|---|---|---|---|---|---|---|
| 0 (full file) | 106,910,771 | 25,159,211 | 2,457 | 4 | **true** | **81.12 ms** | 13.86 ms |
| size − 200 KB | 204,800 | 75,033 | 32 | 1 | false | **0.50 ms** | 0.04 ms |
| size (caught up) | 0 | 2 | 0 | 0 | false | **0.00 ms** | 0.00 ms |

### Against the plan's reference points

| | plan | measured | why |
|---|---|---|---|
| 200 KB tail-seek | 0.45 ms | **0.38 / 0.50 ms** | agrees |
| 19.7 MB full parse | 21.0 ms | **41.28 ms** at 21.4 MB | see below |
| 106.9 MB full parse | 123.5 ms | **81.12 ms** | see below |

The full-file row is **~2× the plan's reference**, and the attribution is
measured rather than argued: **9.91 ms of the 41.28 ms is the sizing
serialization** (timed separately in the same test, over exactly the kept
messages), and the remainder is the per-message `Value` clone out of the parsed
entry. The plan's 21 ms was a parse-only prototype that produced no size
accounting and cloned nothing. This is a real regression against that number,
it is confined to the initial retain — which §C4b already declares the
exception and runs under `async: true` — and the `RawValue` upgrade path above
recovers most of it if it ever matters. The steady-state row that actually
runs thousands of times is 0.38 ms.

The 106.9 MB row is *faster* than the plan's 123.5 ms for the opposite reason:
we never build a list of all 4,671 messages. 96 % of the file's lines are
skipped types, and the window holds at most 24 MB.

### Manual verification, and why it is not `hook retain --dry-run`

The plan asks for `memgarden hook retain --dry-run` against the live transcript
at three offsets. That flag needs a `retain` arm in `dispatch`, in
`crates/memgarden-cli/src/lib.rs` — **the one file C3 (PR #25) is editing on a
sibling branch off the same master**. Adding one here would be a merge conflict
in a shared file, for a diagnostic C4b re-implements two PRs later. The
`#[ignore]`d `live_transcript_measurement` test above is the substitute: same
three offsets, same numbers, no dispatch change. The tables above are its
verbatim output.

---

## Mutation evidence

22 mutations against `src/transcript.rs`, one at a time, whole `memgarden-cli`
suite per run. **19 caught, 3 survived.**

The three survivors, each named rather than counted:

| survivor | verdict |
|---|---|
| `consumed_to += read` → `reader.stream_position()` | **equivalent.** `BufReader::stream_position` returns the logical position (underlying minus buffered remainder), which is `from_offset + Σ read` at that point by construction. |
| `from_slice(line).ok()?` → `.unwrap_or(Value::Null)` | **equivalent.** `Value::Null.as_object()` is `None`, so the following `?` still returns. The mutation does not do what its label claims. |
| `READ_BUFFER_BYTES` 1 MiB → 1 B | **predicted, and correct to survive.** The buffer is a syscall-count decision; the only test that could catch it would assert syscall counts, which is a change detector, not a test. |

The useful finding is the same shape as C3's: **the survivors that mattered were
ones nobody predicted.**

* Adding `attachment` to the kept-types arm **survived the entire suite** —
  including a whole-file assertion over a fixture with 17 real `attachment`
  entries. No `attachment` entry happens to carry a `message.role`, so the role
  requirement had been silently doing the skip list's job and the skip list was
  decorative. Closed by
  `a_skipped_type_is_skipped_even_when_it_carries_a_usable_message`, which
  feeds skipped types that *do* carry a usable message.
* `total > cap` (dropping the array's opening bracket) and dropping the
  per-message separator byte **both survived** the original fallback test,
  whose cap had slack in it — the textbook case of a test asserting a bound the
  most-likely mutant satisfies. Closed by
  `the_cap_is_exact_on_both_sides_of_the_boundary`.
* The deliberate divergence from legacy's `elif` had **no test at all**. Its
  first version also failed to kill the mutant, because it used a `system`
  entry, which reaches a different match arm than the one being mutated.

All three were behavioural gaps, not cosmetic ones, and none was visible by
reading the tests.

---

## What C4b inherits

* `read_delta` returns `consumed_to`; **C4b decides whether to commit it** and
  owns `pending`, the rollback, the `size < offset` reset and the turn gate.
* `compactions` is a number to forward in the POST body. It is not a signal.
* `truncated` goes in `metadata` and is the flag the ledger caveat above hangs
  off.
* `messages` is `Vec<Value>` and goes into the body as-is.
