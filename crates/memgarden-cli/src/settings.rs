//! `~/.claude/settings.json`, edited by **textual line splice** and never
//! reserialized (plan §Binding decisions #11).
//!
//! # Why this is not `serde_json::Value` surgery
//!
//! `serde_json` in this workspace has no `preserve_order` — verified in
//! `Cargo.lock`, whose `serde_json` dependency list is exactly `itoa, memchr,
//! serde, serde_core, zmij`. Its `Map` is therefore a `BTreeMap`, so **any**
//! `Value` round-trip re-emits the whole document with its keys sorted. The
//! user's real file is not sorted (`hooks`, `statusLine`, `enabledPlugins`,
//! `extraKnownMarketplaces`, `tui`, …) and it is shared: Orca's hooks, the
//! statusline, the plugin registry and the marketplace list all live in it.
//! Rewriting every byte of that file is the highest-damage operation this
//! phase performs, and it would happen on the *install* path — the one the
//! user runs first and trusts least.
//!
//! Turning the feature on (`serde_json/preserve_order`) is worse, not better:
//! Cargo feature unification would apply it to `memgardend` in any workspace
//! build, changing key order in every API response and every stored JSON blob.
//!
//! So here `serde_json` **validates and locates** and never produces output
//! bytes:
//!
//! * [`serde_json::from_str`] proves the file parses before we touch it, and
//!   proves the spliced result still parses before we write it;
//! * a small string-aware forward scan finds the byte offset to insert at;
//! * install inserts exactly one line, uninstall deletes exactly that line.
//!
//! That narrowing is what makes "uninstall restores the file to its pre-install
//! bytes" a test that can actually pass. General textual JSON editing would
//! not be less code than `Value` surgery; insert-one-line / delete-one-line is.
//!
//! # The marker
//!
//! Every line we write carries [`MARKER`] — the literal bytes
//! `"statusMessage":"memgarden: `.
//!
//! The plan named a different marker: "the installed binary's absolute path
//! followed by `" hook "`". That string cannot exist in what we emit, because
//! the same plan pins the **exec form** (`"command":"<bin>","args":["hook",…]`)
//! precisely so there is no `/bin/sh -c` hop — the path and the word `hook`
//! end up in two different JSON values. A path-derived marker also breaks
//! `uninstall` for anyone who moved or reinstalled the binary between the two
//! commands, which is the exact moment they need it to work. `statusMessage`
//! is a field Claude Code already renders, so the marker is visible to the
//! user in the UI, is ours by construction, and survives the binary moving.

use std::path::Path;

/// The bytes present in every line this module writes, and the only thing
/// [`uninstall`] matches on.
pub const MARKER: &str = "\"statusMessage\":\"memgarden: ";

/// The four hook entries, exactly as the plan's C5 table specifies them.
///
/// `timeout` is the Claude Code hook timeout in **seconds**, and each value is
/// a deliberate divergence or match against the legacy entries live in the
/// user's file today:
///
/// * `SessionStart` 5 s — matches legacy; the work is one bank upsert.
/// * `UserPromptSubmit` 10 s — **not** legacy's 45. The hook's own client
///   timeout is `recall_timeout_ms` (400 ms) and C3's breaker bounds the rest,
///   so anything above a couple of seconds can only ever hide a wedged daemon
///   instead of failing it.
/// * `Stop` 30 s with `async: true` — `async` matches legacy's own `Stop`
///   entry, which is wired directly in this user's `settings.json` (not
///   plugin-only), and it is what keeps the once-per-session initial retain
///   invisible.
/// * `SessionEnd` 5 s — `hook session-end` spawns a detached child and returns
///   in ~0.4 ms, so the budget is never in play.
pub struct Entry {
    pub event: &'static str,
    /// argv slot 1. Slot 0 is always `hook`, so an entry can never be spliced
    /// into the `hooks install` family by a typo here.
    pub sub: &'static str,
    pub timeout: u32,
    pub is_async: bool,
    /// Rendered by Claude Code while the hook runs, and — because it carries
    /// [`MARKER`] — the thing that makes this line ours. Both jobs are real:
    /// during a shadow run two memory systems are wired at once and the user
    /// needs to see which one spoke.
    pub status_message: &'static str,
}

pub const ENTRIES: &[Entry] = &[
    Entry {
        event: "SessionStart",
        sub: "session-start",
        timeout: 5,
        is_async: false,
        status_message: "memgarden: session start",
    },
    Entry {
        event: "UserPromptSubmit",
        sub: "recall",
        timeout: 10,
        is_async: false,
        status_message: "memgarden: recalling",
    },
    Entry {
        event: "Stop",
        sub: "retain",
        timeout: 30,
        is_async: true,
        status_message: "memgarden: retaining",
    },
    Entry {
        event: "SessionEnd",
        sub: "session-end",
        timeout: 5,
        is_async: false,
        status_message: "memgarden: session end",
    },
];

#[derive(Debug)]
pub enum SettingsError {
    /// The file does not parse. We refuse rather than repair: a settings.json
    /// that is already broken is not a file to start splicing.
    Parse(String),
    /// The document parses but is not a JSON object, so it has no `hooks` key
    /// and never will.
    NotAnObject,
    /// `serde_json` found a key the scanner could not, which can only happen
    /// for an escaped key spelling (`"hooks"`). Refusing is the point:
    /// the alternative is inserting a **second** `"hooks"` member, and the
    /// last-wins duplicate would silently disable every hook in the first one.
    Unlocatable(&'static str),
    /// The key exists and holds something we cannot splice into — a `"hooks"`
    /// that is not an object, or an event that is not an array.
    ///
    /// Split out from [`SettingsError::Unlocatable`] because the two need
    /// different advice and the shared message was actively misleading: it
    /// blamed an escaped key name for a file whose key is spelled perfectly
    /// and simply holds a string.
    WrongType(&'static str),
    /// The same key appears twice at the level we were about to edit.
    ///
    /// **This is the silent-failure case, and it is why the check exists.**
    /// JSON duplicate keys are last-wins for every parser involved, but a
    /// forward scan finds the *first* member — so the splice would land in a
    /// member Claude Code ignores. Install would report four entries written
    /// and wire **none of them**, which is the one outcome worse than an
    /// error: `hooks status` would then agree, because it reads the same
    /// last-wins view.
    Duplicate(&'static str),
    /// The splice produced bytes that no longer parse. Nothing is written.
    /// This is the assertion that the scanner cannot corrupt the user's file
    /// even if every other belief in this module is wrong.
    WouldCorrupt(String),
    /// `uninstall` finished and the parsed document still shows entries wired.
    ///
    /// The byte scan and the parse can disagree — a co-tenant that rewrote
    /// this shared file through a pretty-printer leaves an entry the parse
    /// sees and a scan may not resolve to a deletable chunk. Reporting
    /// "nothing to remove" while four hooks keep firing is the one failure a
    /// cutover switch must not have, so this is an error rather than a
    /// partial success.
    Residual(usize),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::Parse(e) => write!(f, "settings.json does not parse: {e}"),
            SettingsError::NotAnObject => write!(f, "settings.json is not a JSON object"),
            SettingsError::Unlocatable(k) => write!(
                f,
                "the {k:?} key exists but is written in a form this editor cannot locate \
                 (an escaped key name?) — edit it by hand rather than risk a duplicate"
            ),
            SettingsError::WrongType(k) => write!(
                f,
                "the {k:?} key holds something this editor cannot splice into — \
                 \"hooks\" must be an object and an event must be an array. \
                 Fix it by hand and run install again"
            ),
            SettingsError::Duplicate(k) => write!(
                f,
                "settings.json has more than one {k:?} key at the same level. \
                 Duplicate keys are last-wins, so an edit to the first one would be \
                 silently ignored — install would report success and wire nothing. \
                 Delete the duplicate and run install again"
            ),
            SettingsError::WouldCorrupt(e) => write!(
                f,
                "refusing to write: the edited file would not parse ({e}). \
                 This is a bug in memgarden; nothing was changed"
            ),
            SettingsError::Residual(n) => write!(
                f,
                "{n} memgarden entr{} still wired after the removal pass — the file has \
                 been rewritten since install (a reformatter, or a hand edit) into a shape \
                 this uninstaller cannot locate. Nothing was written. Remove them by hand, \
                 or restore the timestamped backup from the install",
                if *n == 1 { "y is" } else { "ies are" }
            ),
        }
    }
}

/// One hook entry as the single compact line install inserts.
///
/// Key order is written by hand rather than through `serde_json::to_string`
/// for the same reason the whole module exists — a `Map` here would sort them
/// to `args, async, command, statusMessage, timeout, type`, which is readable
/// by nobody. Only the binary path goes through `serde_json`, because it is
/// the one value that can contain a quote or a backslash.
pub fn group_line(entry: &Entry, bin: &Path) -> String {
    let command = serde_json::to_string(&bin.to_string_lossy()).unwrap_or_else(|_| "\"\"".into());
    let async_field = if entry.is_async {
        r#","async":true"#
    } else {
        ""
    };
    format!(
        r#"{{"hooks":[{{"type":"command","command":{command},"args":["hook","{sub}"],"timeout":{timeout}{async_field},"statusMessage":"{status}"}}]}}"#,
        sub = entry.sub,
        timeout = entry.timeout,
        status = entry.status_message,
    )
}

/// The result of a splice: the new text plus the lines that were added or
/// removed, for the `--dry-run` diff.
#[derive(Debug)]
pub struct Splice {
    pub text: String,
    pub changed: Vec<String>,
}

impl Splice {
    fn unchanged(text: &str) -> Splice {
        Splice {
            text: text.to_string(),
            changed: Vec::new(),
        }
    }
    pub fn is_noop(&self) -> bool {
        self.changed.is_empty()
    }
}

/// Splices in every entry that is not already wired, and returns the new text.
///
/// Idempotent per event: an event that already carries one of our entries is
/// left completely alone, so running `install` twice writes nothing the second
/// time. That matters more than it sounds — the file watcher makes every write
/// visible to running Claude Code instances immediately, so a needless write is
/// a needless live reconfiguration of every open session.
pub fn install(src: &str, bin: &Path) -> Result<Splice, SettingsError> {
    let doc: serde_json::Value =
        serde_json::from_str(src).map_err(|e| SettingsError::Parse(e.to_string()))?;
    if !doc.is_object() {
        return Err(SettingsError::NotAnObject);
    }

    let already = wired_events(&doc);
    let mut text = src.to_string();
    let mut changed = Vec::new();
    for entry in ENTRIES {
        if already.contains(&entry.event) {
            continue;
        }
        let line = group_line(entry, bin);
        // Re-parsed per entry rather than threading a `Value` through: after
        // the first splice the old `doc` describes a file that no longer
        // exists, and "insert after the `[` of an event that a previous
        // iteration just created" is exactly the case a stale view gets wrong.
        let doc: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| SettingsError::Parse(e.to_string()))?;
        let (at, chunk) = insertion(&text, &doc, entry.event, &line)?;
        let indent = inner_indent(text.as_bytes(), at);
        // The file's own line ending, not ours. A bare LF into a CRLF file is
        // byte-restorable and therefore harmless on its own — but it leaves a
        // mixed-ending file, which an editor is then likely to normalise, and
        // normalising is what H1's whitespace tolerance exists to survive.
        // Not creating the trigger is cheaper than surviving it.
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let inserted = format!("{newline}{indent}{chunk}");
        text.insert_str(at, &inserted);
        changed.push(format!("{indent}{chunk}"));
    }

    if changed.is_empty() {
        return Ok(Splice::unchanged(src));
    }
    validate(&text)?;
    Ok(Splice { text, changed })
}

/// Deletes every line this module wrote, restoring the pre-install bytes.
///
/// The unit of deletion is the **inserted chunk plus the newline and indent
/// that preceded it** — not "the line", which would be wrong whenever the
/// original file kept an event array on a single line: inserting after the `[`
/// of `"Stop": [{orig}]` leaves our chunk and `{orig}` sharing a line, and a
/// line-wise delete would take the user's entry with it.
pub fn uninstall(src: &str) -> Result<Splice, SettingsError> {
    // Validated up front for symmetry with `install`: if the file does not
    // parse, the scan below is operating on bytes whose structure we have not
    // established, and the user has a bigger problem than an installed hook.
    serde_json::from_str::<serde_json::Value>(src)
        .map_err(|e| SettingsError::Parse(e.to_string()))?;

    let mut text = src.to_string();
    let mut changed = Vec::new();
    // One marker at a time, re-searching from the start each round: a chunk
    // can contain a later chunk (the `"hooks": {…}` wrapper form holds the
    // event array that holds a group), so offsets from the previous round are
    // not valid after a removal.
    // `from` exists so an entry we cannot resolve does not hide every later
    // one: the search restarts at 0 after each removal (a chunk can contain a
    // later chunk), but steps past a refusal instead of ending the pass. What
    // is left unresolved is reported by the residual check below.
    let mut from = 0usize;
    while let Some(m) = find_marker(text.as_bytes(), from) {
        let Some((start, end)) = chunk_span(text.as_bytes(), m) else {
            // Left in place deliberately: doing nothing is always recoverable,
            // and deleting a span we cannot prove is ours is not.
            from = m + 1;
            continue;
        };
        changed.push(text[start..end].trim_start().to_string());
        text.replace_range(start..end, "");
        from = 0;
    }

    // **The byte scan and the parsed view have to agree that we are done.**
    // They can disagree, and the disagreement is silent in the worst
    // direction: `install` decides idempotency from the *parsed* document,
    // `uninstall` deletes from *raw bytes*, and any other tool that rewrites
    // this shared file through a pretty-printer leaves an entry the parse
    // still sees and a byte scan may not. Reporting "nothing to remove" while
    // four hooks keep firing is the one failure a cutover switch must not
    // have, so the residual is an error with the count in it.
    let residual = serde_json::from_str::<serde_json::Value>(&text)
        .map(|doc| wired_events(&doc).len())
        .unwrap_or(0);
    if residual > 0 {
        return Err(SettingsError::Residual(residual));
    }

    if changed.is_empty() {
        return Ok(Splice::unchanged(src));
    }
    validate(&text)?;
    Ok(Splice { text, changed })
}

/// The first byte of a `"statusMessage"` member whose value begins
/// `memgarden: `, scanning **whitespace-tolerantly** across the colon.
///
/// [`MARKER`] is what [`group_line`] emits and is exact by construction — but
/// `settings.json` is shared, and a co-tenant that rewrites it through
/// `JSON.stringify(x, null, 2)` or `to_string_pretty` produces
/// `"statusMessage": "memgarden: …"` with a space. Matching the emitted
/// spelling only would make `uninstall` a no-op on a file that has merely been
/// reformatted — while `status`, which reads the parse, still reported four
/// hooks wired.
///
/// The opening `"` of the key is what is returned, because [`chunk_span`]
/// only needs a byte inside the chunk.
fn find_marker(s: &[u8], start: usize) -> Option<usize> {
    const KEY: &[u8] = b"\"statusMessage\"";
    const VALUE_PREFIX: &[u8] = b"\"memgarden: ";
    let mut from = start.min(s.len());
    while let Some(rel) = s[from..]
        .windows(KEY.len())
        .position(|w| w == KEY)
        .map(|p| from + p)
    {
        let after_key = rel + KEY.len();
        let colon = skip_ws(s, after_key);
        if s.get(colon) == Some(&b':') {
            let value = skip_ws(s, colon + 1);
            if s.get(value..value + VALUE_PREFIX.len()) == Some(VALUE_PREFIX) {
                return Some(rel);
            }
        }
        from = rel + 1;
    }
    None
}

fn validate(text: &str) -> Result<(), SettingsError> {
    serde_json::from_str::<serde_json::Value>(text)
        .map(|_: serde_json::Value| ())
        .map_err(|e| SettingsError::WouldCorrupt(e.to_string()))
}

/// Which events already carry one of our entries, read from the parsed
/// document rather than from the raw bytes.
///
/// Reading is where `Value` is safe and welcome: the ban is on *emitting*.
pub fn wired_events(doc: &serde_json::Value) -> Vec<&'static str> {
    ENTRIES
        .iter()
        .filter(|entry| {
            groups(doc, entry.event).any(|hook| {
                hook.get("statusMessage")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.starts_with("memgarden: "))
                    && hook
                        .get("args")
                        .and_then(|v| v.as_array())
                        .and_then(|a| a.get(1))
                        .and_then(|v| v.as_str())
                        == Some(entry.sub)
            })
        })
        .map(|entry| entry.event)
        .collect()
}

/// The `command` of **our** entry for `event`, if one is wired.
///
/// Exists so `status` can notice that the path it wired no longer resolves:
/// `wired_events` deliberately matches on the marker and the subcommand
/// rather than on the path, because that is what makes `install` idempotent
/// after a rebuild — but it means a *moved* binary reads as healthy.
pub fn wired_command(doc: &serde_json::Value, event: &str) -> Option<String> {
    groups(doc, event)
        .find(|hook| {
            hook.get("statusMessage")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.starts_with("memgarden: "))
        })
        .and_then(|hook| hook.get("command"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The legacy (hindsight) hook commands wired for `event`, if any.
///
/// Matched on the substring `hindsight` anywhere in `command`, because that is
/// what every legacy entry carries in this user's file three times over
/// (`CLAUDE_PLUGIN_ROOT=…/hindsight/…`, `CLAUDE_PLUGIN_DATA=…/hindsight-…`,
/// and the script path). A tighter match on the script filename would miss a
/// user who wrapped the call in a shell one-liner, and the consequence of a
/// false negative is worse than a false positive: a false negative lets
/// `--mode full` proceed into double injection, which is the thing this
/// detection exists to stop.
pub fn legacy_commands(doc: &serde_json::Value, event: &str) -> Vec<String> {
    groups(doc, event)
        .filter_map(|hook| hook.get("command").and_then(|v| v.as_str()))
        .filter(|cmd| cmd.to_ascii_lowercase().contains("hindsight"))
        .map(str::to_string)
        .collect()
}

/// Every command-hook object wired for `event`, flattened across matcher
/// groups: `hooks.<Event>[].hooks[]`.
fn groups<'a>(
    doc: &'a serde_json::Value,
    event: &str,
) -> impl Iterator<Item = &'a serde_json::Value> {
    doc.get("hooks")
        .and_then(|h| h.get(event))
        .and_then(|e| e.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|group| group.get("hooks").and_then(|h| h.as_array()))
        .flatten()
}

// --- the scanner -----------------------------------------------------------
//
// Everything below operates on bytes that `serde_json` has already accepted,
// so it may assume well-formedness. It may **not** assume formatting: the
// user's file is hand-edited and machine-edited by several tools.

/// Where to insert, and what.
///
/// Three shapes, narrowing from "the common case" to "an empty settings.json":
///
/// 1. the event array exists — insert the group after its `[`;
/// 2. `hooks` exists but the event does not — insert `"<Event>": [<group>]`
///    after the `hooks` `{`;
/// 3. neither exists — insert `"hooks": {"<Event>": [<group>]}` after the
///    document's `{`.
///
/// All three are **one line**, which is what keeps uninstall exact.
fn insertion(
    text: &str,
    doc: &serde_json::Value,
    event: &'static str,
    line: &str,
) -> Result<(usize, String), SettingsError> {
    let s = text.as_bytes();
    let root = skip_ws(s, 0);

    let Some(hooks) = doc.get("hooks").filter(|v| v.is_object()) else {
        // Shape 3. A `hooks` key that exists but is not an object is left
        // alone by the `filter` and lands here — where the splice would
        // produce a duplicate key, so `validate` is not enough. Refuse.
        if doc.get("hooks").is_some() {
            return Err(SettingsError::WrongType("hooks"));
        }
        let chunk = format!(
            r#""hooks": {{"{event}": [{line}]}}{comma}"#,
            comma = trailing_comma(s, root),
        );
        return Ok((root + 1, chunk));
    };

    let hooks_at = find_member(s, root, "hooks").map_err(|e| e.at("hooks"))?;
    if hooks.get(event).and_then(|e| e.as_array()).is_none() {
        // Shape 2. Same refusal as above for a non-array event value: we
        // cannot insert into it and must not shadow it.
        if hooks.get(event).is_some() {
            return Err(SettingsError::WrongType(event));
        }
        let chunk = format!(
            r#""{event}": [{line}]{comma}"#,
            comma = trailing_comma(s, hooks_at),
        );
        return Ok((hooks_at + 1, chunk));
    }

    // Shape 1.
    let event_at = find_member(s, hooks_at, event).map_err(|e| e.at(event))?;
    Ok((
        event_at + 1,
        format!("{line}{}", trailing_comma(s, event_at)),
    ))
}

/// `","` unless the container opening at `open` is empty — in which case a
/// comma would produce `[x,]`, which is not JSON.
fn trailing_comma(s: &[u8], open: usize) -> &'static str {
    let next = skip_ws(s, open + 1);
    match s.get(next) {
        Some(b'}') | Some(b']') | None => "",
        _ => ",",
    }
}

/// The indentation to give the inserted line.
///
/// Taken from the first existing child of the container we are inserting into,
/// so the line lands in the file's own style rather than in ours. With no
/// child to copy (an empty or single-line container) it is the container's own
/// line indent plus two spaces — the style of every settings.json seen here,
/// and cosmetic either way: the JSON is identical.
fn inner_indent(s: &[u8], after_open: usize) -> String {
    let mut i = after_open;
    while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
        i += 1;
    }
    if s.get(i) == Some(&b'\n') {
        let start = i + 1;
        let mut j = start;
        while j < s.len() && (s[j] == b' ' || s[j] == b'\t') {
            j += 1;
        }
        // Not `..j` blindly: a blank line inside the container would give an
        // empty indent and a ragged file.
        if j > start {
            return String::from_utf8_lossy(&s[start..j]).into_owned();
        }
    }
    let line_start = s[..after_open]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |p| p + 1);
    let mut k = line_start;
    while k < s.len() && (s[k] == b' ' || s[k] == b'\t') {
        k += 1;
    }
    format!("{}  ", String::from_utf8_lossy(&s[line_start..k]))
}

/// The byte span to delete for the chunk containing the marker at `marker`.
///
/// Starts at the newline that precedes the chunk (so the indentation we added
/// goes with it) and ends after the chunk's optional trailing comma.
///
/// The chunk is only extended back to the line start when the line **begins**
/// with one of the three prefixes [`insertion`] emits; otherwise the span is
/// just the group object itself. That check is what stops a wrapper we did not
/// write from being deleted.
///
/// // ponytail: prefix-matched, not provenance-tracked. If a user hand-adds a
/// // second entry inside an event array MemGarden created, uninstall takes it
/// // too — the timestamped backup is the recovery, and the runbook says so. A
/// // real fix needs a sidecar recording what we wrote, which is a file to
/// // keep in sync for a case that has not happened.
fn chunk_span(s: &[u8], marker: usize) -> Option<(usize, usize)> {
    let newline = s[..marker].iter().rposition(|&b| b == b'\n')?;
    let line_start = skip_ws_inline(s, newline + 1);
    // The line ending install writes is the file's own, so on a CRLF file the
    // deletion has to take the `\r` with the `\n`. Leaving it behind was
    // measured, and it accumulates: `"hooks": {\r\r\r\r\n` after four rounds.
    let start = match newline.checked_sub(1) {
        Some(prev) if s[prev] == b'\r' => prev,
        _ => newline,
    };

    match s.get(line_start)? {
        // The group object itself — the shape install writes into an event
        // array that already existed. Deleting it takes exactly one entry.
        b'{' => {}
        // A wrapper. **Two checks, and neither is optional.** Review
        // demonstrated the unchecked version deleting an entire
        // `"SubagentStop": [{other tool}, {ours}]` member — another tool's
        // hook gone, reported as `removed 1 entries`, exit 0, and `validate`
        // cannot see it because the result is valid JSON.
        b'"' => {
            let key_end = skip_string(s, line_start)?;
            let key = s.get(line_start + 1..key_end - 1)?;
            // 1. It has to be a key we could have written.
            let ours = key == b"hooks" || ENTRIES.iter().any(|e| e.event.as_bytes() == key);
            if !ours {
                return None;
            }
        }
        _ => return None,
    }

    let end = chunk_end(s, line_start)?;
    // The marker has to be inside what we are about to delete. Without this,
    // a line that merely *precedes* our chunk could claim it.
    if !(line_start..end).contains(&marker) {
        return None;
    }
    // 2. And it has to hold **only** our entry. One `"command"` member is one
    // hook; a second belongs to somebody else and this span is not ours to
    // delete. Refusing here is not a silent skip — `uninstall`'s residual
    // check turns an unresolvable entry into a visible error.
    // `"command":` with the colon, not `"command"` — every hook object also
    // carries `"type":"command"`, whose *value* is the same eight bytes.
    let occurrences = s
        .get(line_start..end)?
        .windows(b"\"command\":".len())
        .filter(|w| *w == b"\"command\":")
        .count();
    if occurrences != 1 {
        return None;
    }
    Some((start, end))
}

/// The end of the chunk starting at `start`: either `{…}` (a group) or
/// `"key": {…}` / `"key": […]` (a wrapper), plus one optional comma.
fn chunk_end(s: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    if s.get(i) == Some(&b'"') {
        i = skip_string(s, i)?;
        i = skip_ws(s, i);
        if s.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(s, i + 1);
    }
    let mut end = skip_value(s, i)?;
    if s.get(end) == Some(&b',') {
        end += 1;
    }
    Some(end)
}

/// Why a member lookup did not produce exactly one offset. Mapped to a
/// [`SettingsError`] by the caller, which knows whether it was looking for
/// `"hooks"` or for an event.
enum NoMember {
    NotFound,
    Duplicate,
}

impl NoMember {
    fn at(self, key: &'static str) -> SettingsError {
        match self {
            NoMember::NotFound => SettingsError::Unlocatable(key),
            NoMember::Duplicate => SettingsError::Duplicate(key),
        }
    }
}

/// The byte offset of the value of `key` in the object whose `{` is at
/// `obj_start`. Members are walked at exactly one level of nesting; a matching
/// key deeper in the tree is not a match.
///
/// **The whole object is scanned even after a match, to catch a duplicate
/// key.** That is not thoroughness for its own sake: JSON duplicate keys are
/// last-wins everywhere, but a forward scan finds the *first* member, so
/// splicing into it would write into a member Claude Code discards. The result
/// is the worst outcome available — install reports four entries written,
/// `hooks status` reads the same last-wins view and agrees that none are
/// wired, and nothing in the system is lying about anything except the effect.
/// Verified against the built binary before this guard existed: a settings.json
/// with two `"hooks"` keys installed "successfully" and wired zero hooks.
fn find_member(s: &[u8], obj_start: usize, key: &str) -> Result<usize, NoMember> {
    if s.get(obj_start) != Some(&b'{') {
        return Err(NoMember::NotFound);
    }
    let mut found: Option<usize> = None;
    let mut i = skip_ws(s, obj_start + 1);
    while s.get(i) == Some(&b'"') {
        let Some(key_end) = skip_string(s, i) else {
            break;
        };
        let matches = s
            .get(i + 1..key_end - 1)
            .is_some_and(|k| k == key.as_bytes());
        i = skip_ws(s, key_end);
        if s.get(i) != Some(&b':') {
            break;
        }
        let value = skip_ws(s, i + 1);
        if matches {
            if found.is_some() {
                return Err(NoMember::Duplicate);
            }
            found = Some(value);
        }
        let Some(after) = skip_value(s, value) else {
            break;
        };
        i = skip_ws(s, after);
        if s.get(i) != Some(&b',') {
            break;
        }
        i = skip_ws(s, i + 1);
    }
    found.ok_or(NoMember::NotFound)
}

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && s[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Whitespace **except** newlines: used where crossing a line break would
/// change which line we think we are on.
fn skip_ws_inline(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
        i += 1;
    }
    i
}

/// The offset just past the string literal starting at `i`.
///
/// This is the whole reason the scan is "string-aware" rather than a `find`:
/// the user's file contains `"hooks"`, `"Stop"` and `[` inside *string values*
/// — the Orca hook command mentions none of ours, but the SessionStart echo
/// hook embeds a whole escaped JSON document, braces and all. A `\"` inside it
/// must not end the string.
fn skip_string(s: &[u8], i: usize) -> Option<usize> {
    if s.get(i) != Some(&b'"') {
        return None;
    }
    let mut i = i + 1;
    while i < s.len() {
        match s[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// The offset just past the JSON value starting at (or after whitespace from)
/// `i`.
fn skip_value(s: &[u8], i: usize) -> Option<usize> {
    let i = skip_ws(s, i);
    match s.get(i)? {
        b'"' => skip_string(s, i),
        b'{' | b'[' => skip_container(s, i),
        // Numbers and the three literals. Ended by a structural character or
        // whitespace, which is sufficient because the document already parsed.
        _ => {
            let mut j = i;
            while j < s.len() && !matches!(s[j], b',' | b'}' | b']') && !s[j].is_ascii_whitespace()
            {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

fn skip_container(s: &[u8], mut i: usize) -> Option<usize> {
    let mut depth = 0usize;
    while i < s.len() {
        match s[i] {
            b'"' => {
                i = skip_string(s, i)?;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// --- writing ---------------------------------------------------------------

/// Copies `path` to `dir/settings-backup-<unix-ms>.json` and returns where.
///
/// Every `install` and `uninstall` takes one first. The targeted line removal
/// is the normal way back; this is the way back from everything else,
/// including the residual race [`write_atomic`] documents and cannot close.
/// **Backups live in their own subdirectory** — `<state_dir>/backups/` — and
/// not next to the session state, because `state::gc` deletes every `*.json`
/// in the state dir older than `session_retention_days` with no name filter.
/// The backup is the documented recovery for the residual `write_atomic` race
/// and for the acknowledged "uninstall eats a hand-added sibling entry" case;
/// putting it where a per-turn hook prunes it gives the recovery path a silent
/// expiry. `gc` does not descend, so a subdirectory is the whole fix.
///
/// The copy is written through `create_new` at 0600 rather than
/// `std::fs::copy`, for the reason `state.rs` documents at length: `fs::copy`
/// opens the destination `create(true).truncate(true)`, which **follows a
/// symlink** — a planted `settings-backup-<ms>.json -> ~/.bashrc` would come
/// back truncated and holding this file's contents.
/// A same-millisecond collision takes the next free suffix rather than
/// failing: `create_new` is what makes the symlink refusal work, but two
/// operations inside one millisecond are ordinary (`install` immediately
/// followed by `uninstall` in a script, which the property check does 1,000
/// times), and a backup that errors aborts the whole command by design.
pub fn backup(path: &Path, dir: &Path, now_ms: i64) -> std::io::Result<std::path::PathBuf> {
    let dir = dir.join("backups");
    crate::state::ensure_dir(&dir)?;
    let mut source = std::fs::File::open(path)?;
    let mut last = None;
    for attempt in 0..64 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let dest = dir.join(format!("settings-backup-{now_ms}{suffix}.json"));
        match open_private(&dest) {
            Ok(mut destination) => {
                std::io::copy(&mut source, &mut destination)?;
                return Ok(dest);
            }
            // Taken already — by a previous backup in the same millisecond, or
            // by a planted symlink. Both are "pick another name", and neither
            // is a reason to follow it.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free backup name")
    }))
}

/// `create_new` at mode 0600 — the same primitive `state::open_private` uses,
/// restated here because that one is private to its module and this crate's
/// dependency budget does not extend to a shared-utility crate for two calls.
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Writes `text` to `path` atomically, refusing if the file no longer holds
/// `expected`.
///
/// **Atomic because of the file watcher, not because of crashes.** The hook
/// docs say direct edits to settings files "are normally picked up
/// automatically by the file watcher", so this write reconfigures every
/// running Claude Code instance the moment it lands — and a watcher that reads
/// a half-written file gets a settings.json that does not parse. Hence
/// tmp-in-the-same-directory (so `rename` is on one filesystem and therefore
/// atomic), `fsync`, then `rename`.
///
/// The `expected` re-check is a full byte comparison rather than the SHA-256
/// the plan names. It needs no hash implementation in a crate whose dependency
/// closure is CI-enforced, and on a file of this size (7 KB here) it is
/// strictly stronger: a hash can collide, a comparison cannot. The residual
/// race is unchanged and accepted — someone writing between this read and the
/// `rename` still loses — and [`backup`] is its recovery.
/// **The symlink is resolved first.** `rename` replaces the *final component*
/// without following it, so a `~/.claude/settings.json` symlinked into a
/// dotfiles repository — a common arrangement for a file people version —
/// would be replaced by a regular file, silently orphaning the copy the user
/// actually edits. `canonicalize` makes the write land on the real file, and
/// puts the temp file beside it so the `rename` stays single-filesystem.
pub fn write_atomic(path: &Path, text: &str, expected: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    // A `CLAUDE_CONFIG_DIR` pointing somewhere fresh, or a machine where
    // Claude Code has not started yet, otherwise fails here with a bare
    // "No such file or directory" printed *after* the diff that said we were
    // about to write. `create_dir_all` is a no-op when it already exists, so
    // no existing directory's mode is touched.
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = dir.join(format!(".memgarden-settings.{}.tmp", std::process::id()));
    // `create_new`, like `state::create_temp`: the open fails rather than
    // following a symlink someone planted at the temp path. (`remove_file`
    // above it is safe for the same reason — `unlink` removes the link, it
    // does not follow it.)
    let _ = std::fs::remove_file(&tmp);
    // **0600 at creation, widened afterwards — never the other way round.**
    // Without the explicit mode the temp file lands at `0666 & ~umask`, which
    // on this machine is 0664 against a settings.json that is 0660: a
    // world-readable copy of the user's hook configuration for the duration of
    // an fsync, and indefinitely if the process is killed before the rename.
    let mut file = open_private(&tmp)?;

    let write = (|| {
        // The user's own mode, not ours: settings.json is not a MemGarden file
        // and a hook installer has no business tightening or loosening it.
        // Applied *before* the content, so there is no window in which the
        // bytes are on disk under the wrong permissions. A file we are
        // creating for the first time keeps 0600 rather than the umask's
        // opinion.
        if let Ok(meta) = std::fs::metadata(&target) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        // Last thing before the rename, deliberately: the smaller this window
        // is, the smaller the race it cannot close.
        if std::fs::read(&target).as_deref().unwrap_or_default() != expected {
            return Err(std::io::Error::other(
                "settings.json changed while we were editing it — nothing written",
            ));
        }
        std::fs::rename(&tmp, &target)
    })();

    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bin() -> PathBuf {
        PathBuf::from("/home/u/.cargo/bin/memgarden")
    }

    /// The shape that matters: top-level keys in the real file's order, an
    /// event with a matcher group, an event whose array is single-line, an
    /// event that is absent, and string values containing `"hooks"`, `"Stop"`
    /// and braces.
    const FIXTURE: &str = r#"{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "python3 /r/hindsight/scripts/recall.py",
            "timeout": 45
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "matcher": "resume",
        "hooks": [
          {
            "type": "command",
            "command": "echo '{\"hooks\":\"Stop [not a real key]\"}'"
          }
        ]
      }
    ],
    "Stop": [{"hooks":[{"type":"command","command":"/o/orca.sh","timeout":10}]}],
    "PreCompact": []
  },
  "statusLine": {
    "type": "command",
    "command": "sh hud.sh"
  },
  "enabledPlugins": {
    "ponytail@ponytail": true
  },
  "tui": "fullscreen"
}
"#;

    fn install_ok(src: &str) -> String {
        install(src, &bin()).expect("install").text
    }

    /// The property the whole module exists for. Not "it parses to the same
    /// value" — byte equality outside the inserted lines, because the damage
    /// this prevents (sorted keys, reflowed formatting) parses to the same
    /// value by definition.
    #[test]
    fn install_changes_no_byte_outside_the_lines_it_inserts() {
        let splice = install(FIXTURE, &bin()).expect("install");
        assert_eq!(splice.changed.len(), 4, "one insertion per event");

        // Removing exactly what install reports it added must give the input
        // back, byte for byte. Asserted here against `changed` rather than via
        // `uninstall`, so the two properties fail independently: this one is
        // "the splice is surgical", the next one is "the removal finds it".
        let mut rebuilt = splice.text.clone();
        for chunk in &splice.changed {
            let with_newline = format!("\n{chunk}");
            assert!(
                rebuilt.contains(&with_newline),
                "install reported a chunk it did not insert: {chunk}"
            );
            rebuilt = rebuilt.replacen(&with_newline, "", 1);
        }
        assert_eq!(rebuilt, FIXTURE);
    }

    /// The test the plan names, and the one that would have failed against a
    /// `Value` round-trip on the first run.
    #[test]
    fn uninstall_restores_the_pre_install_bytes() {
        let installed = install_ok(FIXTURE);
        assert_ne!(installed, FIXTURE);
        let back = uninstall(&installed).expect("uninstall");
        assert_eq!(back.text, FIXTURE);
        assert_eq!(back.changed.len(), 4, "one chunk per event");
    }

    /// The fixture's top-level keys are in the real file's order, which is not
    /// sorted. If a future change reintroduces `Value` surgery this fails
    /// loudly rather than shipping a silently reordered settings.json.
    #[test]
    fn the_fixtures_key_order_is_unsorted_and_survives() {
        let order: Vec<&str> = FIXTURE
            .lines()
            .filter_map(|l| l.strip_prefix("  \""))
            .filter_map(|l| l.split('"').next())
            .collect();
        assert_eq!(
            order,
            vec!["hooks", "statusLine", "enabledPlugins", "tui"],
            "the fixture must not be in sorted order or it proves nothing"
        );
        let out = install_ok(FIXTURE);
        let after: Vec<&str> = out
            .lines()
            .filter_map(|l| l.strip_prefix("  \""))
            .filter_map(|l| l.split('"').next())
            .collect();
        assert_eq!(after, order);
    }

    #[test]
    fn install_is_idempotent() {
        let once = install_ok(FIXTURE);
        let twice = install(&once, &bin()).expect("second install");
        assert!(
            twice.is_noop(),
            "second install changed {:?}",
            twice.changed
        );
        assert_eq!(twice.text, once);
    }

    /// Every event ends up wired, including the one whose key did not exist
    /// and the one whose array was written on a single line.
    #[test]
    fn all_four_events_end_up_wired_whatever_shape_they_started_in() {
        let out = install_ok(FIXTURE);
        let doc: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(
            wired_events(&doc),
            vec!["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"]
        );
        // And the user's own entries are still there, in their arrays.
        assert_eq!(legacy_commands(&doc, "UserPromptSubmit").len(), 1);
        assert!(out.contains("/o/orca.sh"));
        assert!(out.contains("hud.sh"));
    }

    /// An empty settings.json exercises shape 3 (create `hooks`) and the
    /// empty-container comma rule at the same time.
    #[test]
    fn an_empty_object_gets_a_whole_hooks_member_and_gives_it_back() {
        for src in ["{}", "{}\n", "{\n}\n"] {
            let out = install(src, &bin()).expect("install");
            let doc: serde_json::Value = serde_json::from_str(&out.text).expect("valid json");
            assert_eq!(wired_events(&doc).len(), 4, "{src:?}");
            assert_eq!(
                uninstall(&out.text).expect("uninstall").text,
                src,
                "{src:?}"
            );
        }
    }

    /// `[x,]` is not JSON. The empty `PreCompact` array in the fixture is the
    /// same rule one level down.
    #[test]
    fn an_empty_array_takes_no_trailing_comma() {
        let src = r#"{"hooks": {"Stop": []}}"#;
        let out = install(src, &bin()).expect("install");
        serde_json::from_str::<serde_json::Value>(&out.text).expect("valid json");
        assert!(!out.text.contains("}]},"), "trailing comma: {}", out.text);
        assert_eq!(uninstall(&out.text).expect("uninstall").text, src);
    }

    /// The scanner must not be led by `"hooks"`, `"Stop"` or `[` inside a
    /// string value. The fixture's SessionStart echo hook contains all three,
    /// which is why our SessionStart entry has to land in the *real* array.
    #[test]
    fn strings_that_contain_our_key_names_do_not_move_the_insertion_point() {
        let src = r#"{
  "decoy": "\"hooks\": {\"Stop\": [ {",
  "hooks": {
    "Stop": [
      {"hooks":[{"type":"command","command":"real"}]}
    ]
  }
}"#;
        let out = install(src, &bin()).expect("install");
        let doc: serde_json::Value = serde_json::from_str(&out.text).expect("valid json");
        assert!(wired_events(&doc).contains(&"Stop"));
        assert_eq!(
            doc["decoy"].as_str().unwrap(),
            "\"hooks\": {\"Stop\": [ {",
            "the decoy string was edited"
        );
        assert_eq!(uninstall(&out.text).expect("uninstall").text, src);
    }

    #[test]
    fn a_file_that_does_not_parse_is_refused_and_never_repaired() {
        let broken = r#"{"hooks": {"Stop": [ }"#;
        assert!(matches!(
            install(broken, &bin()),
            Err(SettingsError::Parse(_))
        ));
        assert!(matches!(uninstall(broken), Err(SettingsError::Parse(_))));
        assert!(matches!(
            install("[]", &bin()),
            Err(SettingsError::NotAnObject)
        ));
    }

    /// A `hooks` key that is not an object, or an event that is not an array,
    /// must refuse rather than insert a duplicate key — last-wins would
    /// silently disable whatever the first one held.
    #[test]
    fn a_hooks_key_of_the_wrong_type_is_refused_rather_than_shadowed() {
        assert!(matches!(
            install(r#"{"hooks": "off"}"#, &bin()),
            Err(SettingsError::WrongType("hooks"))
        ));
        assert!(matches!(
            install(r#"{"hooks": {"Stop": "off"}}"#, &bin()),
            // Named by the event rather than by the word "event": the message
            // has to tell the user which key to go and fix.
            Err(SettingsError::WrongType("Stop"))
        ));
    }

    /// **The silent-failure case.** Duplicate keys are last-wins for every
    /// parser involved, but a forward scan finds the first member — so an
    /// install would write into a member Claude Code discards, report four
    /// entries written, and wire none. Verified against the built binary
    /// before this guard existed.
    ///
    /// Both levels are covered, because they fail identically and are found
    /// by different call sites.
    #[test]
    fn a_duplicate_key_is_refused_rather_than_spliced_into_the_dead_member() {
        let events = r#""SessionStart": [], "UserPromptSubmit": [], "Stop": [], "SessionEnd": []"#;
        let two_hooks = format!("{{\"hooks\": {{{events}}}, \"hooks\": {{{events}}}}}");
        assert!(
            matches!(
                install(&two_hooks, &bin()),
                Err(SettingsError::Duplicate("hooks"))
            ),
            "two hooks keys must refuse"
        );

        let two_events = format!("{{\"hooks\": {{\"Stop\": [], \"Stop\": [], {events}}}}}");
        assert!(
            matches!(
                install(&two_events, &bin()),
                Err(SettingsError::Duplicate("Stop"))
            ),
            "two event keys must refuse"
        );

        // And the guard is narrow: the same key at two *different* levels is
        // not a duplicate, which is the shape the real file has all over it.
        let nested = r#"{"other": {"hooks": {"Stop": []}}, "hooks": {"Stop": []}}"#;
        assert!(install(nested, &bin()).is_ok());
    }

    /// Uninstalling a file we never touched is a no-op, not an edit.
    #[test]
    fn uninstall_without_a_marker_writes_nothing() {
        let out = uninstall(FIXTURE).expect("uninstall");
        assert!(out.is_noop());
        assert_eq!(out.text, FIXTURE);
    }

    /// The exec form is the point: no `/bin/sh -c`, so no quoting hazard and
    /// no measured 0.28 ms shell hop. And `Stop` is the only `async` entry.
    #[test]
    fn the_installed_entries_match_the_plans_table() {
        let by_event = |e: &str| ENTRIES.iter().find(|x| x.event == e).unwrap();
        assert_eq!(by_event("UserPromptSubmit").timeout, 10, "not legacy's 45");
        assert_eq!(by_event("Stop").timeout, 30);
        assert!(by_event("Stop").is_async);
        for e in ENTRIES {
            assert_eq!(e.is_async, e.event == "Stop", "{}", e.event);
            let line = group_line(e, &bin());
            assert!(line.contains(MARKER), "{}", e.event);
            assert!(line.contains(r#""args":["hook","#), "{}", e.event);
            // No shell, ever: the command is the binary and nothing else.
            assert!(
                line.contains(&format!(r#""command":"{}""#, bin().display())),
                "{}",
                e.event
            );
        }
    }

    /// A path with a quote in it would otherwise produce a line that does not
    /// parse — and the failure would land in the user's settings.json.
    #[test]
    fn a_binary_path_needing_escapes_is_escaped() {
        let weird = PathBuf::from(r#"/tmp/a"b\c/memgarden"#);
        let out = install("{}", &weird).expect("install");
        let doc: serde_json::Value = serde_json::from_str(&out.text).expect("valid json");
        assert_eq!(
            doc["hooks"]["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap(),
            r#"/tmp/a"b\c/memgarden"#
        );
    }

    /// Legacy detection drives the `--mode full` refusal, so a miss is a
    /// double-injection ship. Matched case-insensitively and anywhere in the
    /// command.
    #[test]
    fn legacy_entries_are_found_wherever_hindsight_appears_in_the_command() {
        let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(legacy_commands(&doc, "UserPromptSubmit").len(), 1);
        assert!(legacy_commands(&doc, "Stop").is_empty());
        assert!(legacy_commands(&doc, "SessionEnd").is_empty());

        let wrapped = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command",
          "command":"sh -c 'exec python3 /r/HINDSIGHT/scripts/retain.py'"}]}]}}"#;
        let doc: serde_json::Value = serde_json::from_str(wrapped).unwrap();
        assert_eq!(legacy_commands(&doc, "Stop").len(), 1, "shell-wrapped");
    }

    /// Tabs, CRLF and a compact one-line document all go in and come back out.
    #[test]
    fn unusual_formatting_survives_the_round_trip() {
        for src in [
            "{\n\t\"hooks\": {\n\t\t\"Stop\": [\n\t\t]\n\t}\n}",
            "{\r\n  \"hooks\": {\r\n    \"Stop\": []\r\n  }\r\n}\r\n",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"x"}]}]},"tui":"fullscreen"}"#,
        ] {
            let out = install(src, &bin()).expect("install");
            serde_json::from_str::<serde_json::Value>(&out.text).expect("valid json");
            assert_eq!(
                uninstall(&out.text).expect("uninstall").text,
                src,
                "{src:?}"
            );
        }
    }

    /// The check that narrows the read-modify-write window. Someone else
    /// writing settings.json between our read and our rename loses their edit
    /// silently otherwise — and on this file that someone is Claude Code.
    #[test]
    fn a_file_that_changed_under_us_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();

        // Someone else got there first.
        std::fs::write(&path, r#"{"tui":"fullscreen"}"#).unwrap();
        let err = write_atomic(&path, "{\"ours\":true}", b"{}\n").unwrap_err();
        assert!(err.to_string().contains("changed while we were editing"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"tui":"fullscreen"}"#
        );
        // And no temp file is left lying next to it.
        let strays: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");

        // With the right expectation it goes through, and the mode is the
        // file's own rather than ours.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_atomic(&path, "{\"ours\":true}", br#"{"tui":"fullscreen"}"#).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"ours\":true}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "settings.json is not ours to chmod");
        }
    }

    /// A co-tenant rewriting this shared file through a pretty-printer turns
    /// `"statusMessage":"` into `"statusMessage": "`. Matching only the
    /// spelling we emit made `uninstall` a silent no-op on a file whose hooks
    /// were still firing — and `status`, which reads the parse, still reported
    /// four events wired. The scan is whitespace-tolerant across the colon.
    #[test]
    fn a_reformatted_file_can_still_be_uninstalled() {
        let installed = install_ok(FIXTURE);
        // Exactly what `JSON.stringify(x, null, 2)` and `to_string_pretty` do.
        let reformatted = installed.replace("\"statusMessage\":\"", "\"statusMessage\": \"");
        assert_ne!(reformatted, installed, "the fixture must actually change");

        let out = uninstall(&reformatted).expect("uninstall");
        assert_eq!(out.changed.len(), 4, "all four found after reformatting");
        let doc: serde_json::Value = serde_json::from_str(&out.text).expect("valid json");
        assert!(wired_events(&doc).is_empty());
    }

    /// **Never delete a member we did not write.** Review demonstrated the
    /// unguarded version removing an entire
    /// `"SubagentStop": [{other tool}, {ours}]` line — another tool's hook
    /// gone, reported as `removed 1 entries`, exit 0. `validate` cannot catch
    /// it: the result is perfectly good JSON.
    ///
    /// Both guards are asserted: the key has to be one we could have written,
    /// and the span has to hold exactly one hook.
    #[test]
    fn a_member_holding_someone_elses_hook_is_never_deleted() {
        let ours = group_line(&ENTRIES[2], &bin());

        // Our entry sharing a wrapper line with another tool's: the span holds
        // two `"command":` members, so it is not ours to delete. `Stop` *is*
        // one of our events, so the entry left behind is a residual and the
        // command fails rather than reporting success.
        let shared = format!(
            "{{\n  \"hooks\": {{\n    \"Stop\": [{{\"hooks\":[{{\"type\":\"command\",\"command\":\"/other/tool.sh\"}}]}},{ours}]\n  }}\n}}"
        );
        serde_json::from_str::<serde_json::Value>(&shared).expect("fixture parses");
        match uninstall(&shared) {
            Err(SettingsError::Residual(1)) => {}
            other => panic!("expected Residual(1), got {other:?}\n{shared}"),
        }

        // A key we never write, holding a copy of our entry. Left alone, and
        // *not* a residual: `SubagentStop` is not one of the four events this
        // installer manages, so it is outside what uninstall claims to cover.
        // Recorded here so the boundary is a decision rather than an accident.
        let foreign = format!("{{\n  \"hooks\": {{\n    \"SubagentStop\": [{ours}]\n  }}\n}}");
        let out = uninstall(&foreign).expect("not an error — simply not ours");
        assert!(out.is_noop());
        assert_eq!(out.text, foreign);
    }

    /// And when the scan genuinely cannot reach an entry the parse can see,
    /// the command **fails** rather than reporting "nothing to remove". A
    /// cutover switch whose off position silently does nothing is the one
    /// failure it must not have.
    #[test]
    fn entries_the_scan_cannot_reach_are_an_error_not_a_silent_success() {
        // A wired entry written in a shape the chunk scan refuses: the marker
        // is on the same line as the array open, so there is no newline to
        // anchor the deletion to.
        let stuck = format!(
            "{{\"hooks\": {{\"Stop\": [{}]}}}}",
            group_line(&ENTRIES[2], &bin())
        );
        let doc: serde_json::Value = serde_json::from_str(&stuck).unwrap();
        assert_eq!(wired_events(&doc), vec!["Stop"], "fixture must be wired");

        match uninstall(&stuck) {
            Err(SettingsError::Residual(1)) => {}
            other => panic!("expected Residual(1), got {other:?}"),
        }
    }

    #[test]
    fn a_backup_is_a_copy_under_a_timestamped_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, FIXTURE).unwrap();
        let dir = tmp.path().join("hooks");

        let dest = backup(&path, &dir, 1_700_000_000_123).unwrap();
        assert_eq!(
            dest.file_name().unwrap(),
            "settings-backup-1700000000123.json"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), FIXTURE);

        // **In its own subdirectory**, because `state::gc` deletes every
        // `*.json` in the state dir older than the retention window with no
        // name filter — which would give the documented recovery path a silent
        // expiry. `gc` does not descend.
        assert_eq!(dest.parent().unwrap(), dir.join("backups"));
        assert!(!dir.join("settings-backup-1700000000123.json").exists());

        // 0600: the backup holds the same content as a file the user may keep
        // at 0600 themselves, and `fs::copy` would have set the mode after
        // writing the bytes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    /// `std::fs::copy` opens the destination `create(true).truncate(true)`,
    /// which **follows a symlink** — the exact class `state.rs` documents
    /// having been bitten by. A planted backup name must not truncate what it
    /// points at.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_at_the_backup_path_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, FIXTURE).unwrap();
        let precious = tmp.path().join("precious.conf");
        std::fs::write(&precious, "do not lose me").unwrap();

        let dir = tmp.path().join("hooks");
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        std::os::unix::fs::symlink(&precious, dir.join("backups/settings-backup-7.json")).unwrap();

        // It takes the next free name rather than following the link — and
        // the link's target is untouched either way, which is the property
        // that matters.
        let dest = backup(&path, &dir, 7).expect("backup");
        assert_ne!(dest.file_name().unwrap(), "settings-backup-7.json");
        assert_eq!(
            std::fs::read_to_string(&precious).unwrap(),
            "do not lose me"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), FIXTURE);
    }

    /// Two operations inside one millisecond is ordinary — `install` then
    /// `uninstall` in a script — and a backup that errors aborts the command
    /// by design, so a name collision must not be an error.
    #[test]
    fn two_backups_in_the_same_millisecond_both_survive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, FIXTURE).unwrap();
        let dir = tmp.path().join("hooks");

        let first = backup(&path, &dir, 42).unwrap();
        let second = backup(&path, &dir, 42).unwrap();
        assert_ne!(first, second);
        for p in [&first, &second] {
            assert_eq!(std::fs::read_to_string(p).unwrap(), FIXTURE);
        }
    }

    /// `rename` replaces the final component without following it, so a
    /// settings.json symlinked into a dotfiles repository would be replaced by
    /// a regular file and the copy the user actually edits would be silently
    /// orphaned.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_written_through_rather_than_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("dotfiles").join("settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "{}\n").unwrap();
        let link = tmp.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomic(&link, "{\"ours\":true}", b"{}\n").unwrap();

        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the link was replaced by a regular file"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "{\"ours\":true}");
    }

    /// A settings.json this command **creates** must not inherit the process
    /// umask. Without an explicit mode the temp file lands at `0666 & ~umask`
    /// — 0664 on this machine — for a file that holds the user's hook
    /// configuration.
    #[cfg(unix)]
    #[test]
    fn a_created_settings_file_does_not_take_the_umask() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");

        write_atomic(&path, "{}\n", b"").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a file we create is ours to keep private");
        // And no temp file is left behind.
        let strays: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
    }

    /// The scanner walks members at one level only: a nested `"hooks"` inside
    /// a *value* must not be mistaken for the top-level one.
    #[test]
    fn a_nested_hooks_key_is_not_the_top_level_one() {
        let src = r#"{"other": {"hooks": {"Stop": []}}, "hooks": {"Stop": []}}"#;
        let out = install(src, &bin()).expect("install");
        let doc: serde_json::Value = serde_json::from_str(&out.text).expect("valid json");
        assert_eq!(doc["other"]["hooks"]["Stop"].as_array().unwrap().len(), 0);
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }
}
