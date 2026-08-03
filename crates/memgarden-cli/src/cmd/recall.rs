//! `hook recall` — the `UserPromptSubmit` event.
//!
//! **This is the first hook on the per-prompt path, and the only one whose
//! stdout is the point.** `noop` never fires in production and `session-start`
//! fires twice a session; this one runs before every single thing the user
//! types. Three consequences shape the whole file:
//!
//! * **Its stdout goes into the model's context** (plan §Binding decisions #3).
//!   In `full` mode that is the deliverable. In `shadow` mode, on every failure
//!   path, and on every path that is not a 200 carrying text we are willing to
//!   emit, it must be **empty** — not "short", not "a warning", empty.
//! * **Exit 2 erases the user's typed prompt**, and this is the hook where the
//!   legacy footgun actually lived (`recall.py:287-291` exits 2 under `debug`).
//!   Nothing here returns a `Result` to the caller; see `crate` docs.
//! * **The cost of failing is paid on every prompt.** A down daemon costs one
//!   `ECONNREFUSED`; three of them open the circuit breaker and the fourth
//!   prompt opens no socket at all.
//!
//! # Recall never poisons
//!
//! `reject_failures` exists to protect a *cursor* from advancing past bytes the
//! daemon durably refused. Recall advances nothing, so no answer from the
//! daemon — 400, 404, 503, 500 — moves any counter. They collapse into one arm
//! here, which is why the §Failure posture table's separate `503` and `404`
//! rows are indistinguishable in this file and distinguishable only in C4b's.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use memgarden_core::config::{Config, HooksConfig};
use memgarden_core::now_ms;
use serde::Deserialize;

use crate::http::{self, Target};
use crate::state::{self, SessionState};
use crate::{bank, hookio};

/// legacy: `recall.py:128` — a prompt shorter than this is not a query.
///
/// The same number as the daemon's `recall::MIN_QUERY_CHARS`, and in the same
/// unit: **characters**, counted after trimming. The daemon short-circuits a
/// short query to an empty result rather than a 400 precisely for this caller
/// (`routes/recall.rs:59-65`), so the gate here is a saved round trip rather
/// than a correctness requirement — but it saves it on `ok`, `yes`, `네` and
/// every other one-word turn, which is not a rare shape.
///
/// **Characters, not bytes**, and the two are not interchangeable on the input
/// this system is measured against: `안녕` is 2 characters and 6 bytes, so a
/// byte-length gate would send it and get an empty result back, paying a round
/// trip on the per-prompt path to learn what `chars().count()` knew for free.
const MIN_PROMPT_CHARS: usize = 5;

/// The A/B instrument (plan §Coexistence). One JSON object per prompt,
/// appended, rotated at `[hooks] shadow_log_max_bytes`.
const SHADOW_LOG: &str = "shadow-recall.jsonl";

/// The counterpart of legacy's `LAST_RECALL_STATE` (`recall.py:261-269`): the
/// file you read when someone asks *why did it inject that*, or — more often —
/// *why did it inject nothing*. Overwritten every invocation that got as far
/// as talking to the daemon.
const LAST_RECALL: &str = "last_recall.json";

/// What the daemon's `RecallOutcome` is allowed to tell this hook.
///
/// Same discipline as C2b's `Mirror`: every field defaulted, unknown fields
/// ignored, so a daemon that grows a field cannot turn a 200 into a transport
/// failure. `results` is deliberately absent — CE-6 already formatted
/// `injected_text` (plan §Workspace decision keeps this hook thin), so parsing
/// the structured hits would be work whose only consumer is a `Vec` we drop.
#[derive(Debug, Deserialize)]
struct Reply {
    #[serde(default)]
    injected_text: String,
    #[serde(default)]
    counts: Counts,
}

#[derive(Debug, Default, Deserialize)]
struct Counts {
    #[serde(default)]
    returned: usize,
    #[serde(default)]
    tokens: usize,
}

/// What the round trip produced, split by what each outcome is allowed to move.
#[derive(Debug)]
enum Outcome {
    /// 2xx with a body we could read.
    Recalled(Reply),
    /// The daemon answered and the answer was not 2xx. **No counter moves.**
    Rejected(u16),
    /// Connect refused, timeout, or a 2xx body we refuse to parse.
    /// `transport_failures += 1`, and at `breaker_failures` the breaker opens.
    Transport,
    /// `daemon_url` is not a loopback `http://host:port`. A **config** fault,
    /// so it must not move `transport_failures` — a typo that opened the
    /// circuit breaker would look exactly like an outage in `hooks status`.
    Config,
}

impl Outcome {
    /// The label that lands in `last_recall.json`. Short and greppable,
    /// because the file exists to be read by a human at 3 a.m.
    fn label(&self) -> &'static str {
        match self {
            Outcome::Recalled(_) => "recalled",
            Outcome::Rejected(_) => "rejected",
            Outcome::Transport => "transport_failure",
            Outcome::Config => "bad_daemon_url",
        }
    }

    /// The status code, for the diagnostic. `0` for the outcomes that never
    /// reached one — a 404 (the bank does not exist yet) and a 400 (a query
    /// the daemon refused) are *different* answers to "why did it inject
    /// nothing", and collapsing them into `rejected` would throw away the half
    /// that tells you which.
    fn http_status(&self) -> u16 {
        match self {
            Outcome::Rejected(status) => *status,
            _ => 0,
        }
    }
}

pub fn run() {
    let Some(input) = hookio::read_stdin() else {
        return;
    };

    // The prompt gate runs **before** the config read, unlike `session-start`'s
    // session-id gate. Deliberate: this is the one hook on the per-prompt path,
    // a one-word turn is common, and a TOML parse to discover that we are not
    // going to do anything is pure waste. Nothing downstream of here can change
    // the answer — `[hooks] enabled = false` and a 3-character prompt produce
    // the identical observable, which is nothing at all.
    let Some(prompt) = usable_prompt(&input) else {
        return;
    };
    let Some(cfg) = super::enabled_config() else {
        return;
    };
    if input.session_id.is_empty() || input.session_id.len() > super::MAX_SESSION_ID_BYTES {
        super::debug(&cfg.hooks, "recall: unusable session_id");
        return;
    }

    let dir = cfg.hooks.state_dir.as_path();
    let st = state::load(dir, &input.session_id);
    let now = now_ms();

    // The bank comes from the **session's own state** when there is one, and is
    // only derived when there is not.
    //
    // The plan does not say which, and deriving unconditionally is the obvious
    // reading — it is also wrong for the case C2b already decided: a `resume`
    // from a different cwd, or an edited `directory_bank_map`, changes what
    // `derive` returns mid-session, while `session-start` deliberately does not
    // refresh the stored `bank_id` because the cursor belongs to the bank its
    // bytes were posted to. Deriving here would then recall from a bank this
    // session has never written a byte to, and the failure is silent: an empty
    // result is indistinguishable from "no memories matched".
    let bank_id = st
        .as_ref()
        .map(|s| s.bank_id.clone())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| {
            let project_dir = std::env::var("CLAUDE_PROJECT_DIR").ok();
            bank::derive(&cfg.hooks, input.project_dir(project_dir.as_deref()))
        });

    // The breaker is checked **before the socket exists**, which is the whole
    // property: the fourth prompt after three failures must not connect at all.
    if st
        .as_ref()
        .is_some_and(|s| breaker_open(s, &cfg.hooks, now))
    {
        super::debug(&cfg.hooks, "recall: breaker open, skipping");
        return;
    }

    let query = truncate_chars(prompt, cfg.hooks.recall_max_query_chars);
    let outcome = fetch(&cfg, &bank_id, query);
    record(&cfg, dir, &input.session_id, &bank_id, &outcome, now);
    emit(&cfg, &input, query, &bank_id, &outcome, st.as_ref(), now);
}

/// The prompt to recall on, trimmed, or `None` when there is nothing worth
/// asking about.
///
/// Both spellings are accepted (`recall.py:127`): the hooks reference documents
/// `prompt`, some Claude Code sources use `user_prompt`, and legacy takes
/// whichever is truthy. A `prompt` of `"   "` is truthy in Python and strips to
/// empty, so it never falls through to `user_prompt` there and does not here.
fn usable_prompt(input: &hookio::HookInput) -> Option<&str> {
    let raw = if input.prompt.is_empty() {
        &input.user_prompt
    } else {
        &input.prompt
    };
    let trimmed = raw.trim();
    (trimmed.chars().count() >= MIN_PROMPT_CHARS).then_some(trimmed)
}

/// Longest prefix of `s` with at most `max` **characters**.
///
/// legacy slices `query[:recall_max_query_chars]` (`recall.py:167`), which is
/// characters in Python. Slicing bytes would both cut Korean prompts to a third
/// of the intended budget and, on the wrong index, panic — in the process whose
/// entire contract is that it cannot fail loudly.
fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Whether the circuit breaker is open *and* the stamp that says so is one we
/// could plausibly have written.
///
/// **Both sides of the window are guarded.** `breaker_open_until_ms` is read
/// from a file, and a value far enough in the future turns "skip for 60 s" into
/// "never recall again" — silently, on the one hook whose failure mode is
/// invisible by design. No attacker is required: an NTP step, a VM resume or a
/// dual-boot RTC produces one, and C2b hit the identical shape on
/// `poisoned_at`. Anything more than one cooldown ahead cannot have come from
/// the arm below, so it is treated as closed.
fn breaker_open(state: &SessionState, cfg: &HooksConfig, now_ms: i64) -> bool {
    let until = state.breaker_open_until_ms;
    now_ms < until && until <= now_ms.saturating_add(cooldown_ms(cfg))
}

fn cooldown_ms(cfg: &HooksConfig) -> i64 {
    i64::try_from(cfg.breaker_cooldown_secs.saturating_mul(1000)).unwrap_or(i64::MAX)
}

/// `POST /v1/banks/{bank}/recall`.
///
/// The body carries `budget` and `maxTokens` as **separate** knobs. That is not
/// redundancy: `budget` steers how many candidates get reranked
/// (`rerank_limit = thinking_budget * 2`) and `maxTokens` caps the injected
/// text. CE-6 preserved the split deliberately (`RecallConfig::max_tokens`),
/// because collapsing them made the live fork's `budget = "low"` cut the
/// injection to 100 tokens and invalidated the AC-1 A/B against a fork that
/// sends `low` **and** 1024.
fn fetch(cfg: &Config, bank_id: &str, query: &str) -> Outcome {
    let Ok(target) = Target::parse(&cfg.hooks.daemon_url) else {
        super::debug(
            &cfg.hooks,
            &format!("recall: bad daemon_url {:?}", cfg.hooks.daemon_url),
        );
        return Outcome::Config;
    };
    let body = serde_json::json!({
        "query": query,
        "maxTokens": cfg.recall.max_tokens,
        "budget": cfg.profile.recall_budget,
        "recallTypes": cfg.recall.types,
    })
    .to_string();
    let path = format!("/v1/banks/{}/recall", http::encode_path_segment(bank_id));

    match http::post(
        &target,
        &path,
        body.as_bytes(),
        &super::interactive_timeouts(&cfg.hooks),
    ) {
        Ok(r) if r.is_success() => match serde_json::from_slice::<Reply>(&r.body) {
            Ok(reply) => Outcome::Recalled(reply),
            // A 2xx we cannot read is a transport-class failure: a daemon
            // answering something we do not understand is the same class of
            // problem as one that does not answer.
            Err(e) => {
                super::debug(&cfg.hooks, &format!("recall: unparseable reply: {e}"));
                Outcome::Transport
            }
        },
        Ok(r) => {
            super::debug(&cfg.hooks, &format!("recall: rejected with {}", r.status));
            Outcome::Rejected(r.status)
        }
        Err(http::HttpError::Url(m)) => {
            super::debug(&cfg.hooks, &format!("recall: {m}"));
            Outcome::Config
        }
        Err(e) => {
            super::debug(&cfg.hooks, &format!("recall: failed: {e}"));
            Outcome::Transport
        }
    }
}

/// Applies the outcome to the session's counters, and writes **only if
/// something actually changed**.
///
/// The steady state — a healthy daemon, counters already zero — therefore does
/// no state I/O at all, which matters here in a way it did not for
/// `session-start`: this runs on every prompt, and a temp-create + write +
/// rename + lock open per prompt would be pure cost for a no-op write.
///
/// The comparison is against a **baseline**, not against the loaded `Option`.
/// Comparing against `None` would make a healthy recall on a session with no
/// state file *create* one — and a state file that exists is a state file
/// `session-start` will not rebuild from the daemon's mirror, so the successful
/// path would quietly disable the wiped-state-dir recovery. On the failure
/// path creating it is the point: the breaker has nowhere else to live.
fn record(
    cfg: &Config,
    dir: &Path,
    session_id: &str,
    bank_id: &str,
    outcome: &Outcome,
    now_ms: i64,
) {
    let _ = state::with_lock(dir, session_id, || {
        // Re-read inside the lock: `run` loaded a snapshot before a round trip
        // that can take `recall_timeout_ms`, and an `async: true` Stop's retain
        // may have moved the cursor in between.
        let baseline =
            state::load(dir, session_id).unwrap_or_else(|| SessionState::new(session_id, bank_id));
        let mut st = baseline.clone();
        match outcome {
            // Any success clears the breaker, including a 200 that recalled
            // nothing: the daemon answered, which is all the breaker measures.
            Outcome::Recalled(_) => {
                st.transport_failures = 0;
                st.breaker_open_until_ms = 0;
            }
            Outcome::Transport => {
                st.transport_failures = st.transport_failures.saturating_add(1);
                if st.transport_failures >= cfg.hooks.breaker_failures {
                    st.breaker_open_until_ms = now_ms.saturating_add(cooldown_ms(&cfg.hooks));
                }
            }
            // A rejection moves nothing — recall has no cursor to poison — and
            // neither does a config fault.
            Outcome::Rejected(_) | Outcome::Config => {}
        }
        if st != baseline
            && let Err(e) = state::store(dir, &st)
        {
            super::debug(&cfg.hooks, &format!("recall: state write failed: {e}"));
        }
    });
}

/// Everything that leaves this process: stdout, the shadow log, and the
/// diagnostic.
///
/// One function because the three share one decision — whether we have a
/// payload we are willing to hand on — and splitting it would let a future
/// edit make stdout and the log disagree about what was recalled.
fn emit(
    cfg: &Config,
    input: &hookio::HookInput,
    query: &str,
    bank_id: &str,
    outcome: &Outcome,
    before: Option<&SessionState>,
    now_ms: i64,
) {
    let mut status = outcome.label();
    let mut returned = 0;
    let mut tokens = 0;
    let mut injected: Option<&str> = None;

    if let Outcome::Recalled(reply) = outcome {
        returned = reply.counts.returned;
        tokens = reply.counts.tokens;
        if reply.injected_text.is_empty() {
            status = "empty";
        } else if reply.injected_text.len() > cfg.hooks.max_inject_bytes {
            // **The bound the daemon cannot talk us out of.** `injected_text`
            // is daemon-built and already defangs closing tags by tag-name
            // prefix (Phase B `defang`), so this is not about escaping — it is
            // about volume. A daemon bug, a runaway preamble or a `maxTokens`
            // that stopped being honoured must not be able to push megabytes
            // into the model's context through us, and the same ceiling
            // applies to the shadow log so it cannot be filled at the same
            // rate. Recorded rather than truncated: half an injection is a
            // worse thing to hand a model than none.
            status = "oversize";
            super::debug(
                &cfg.hooks,
                &format!(
                    "recall: injection of {} bytes exceeds max_inject_bytes {}",
                    reply.injected_text.len(),
                    cfg.hooks.max_inject_bytes
                ),
            );
        } else {
            injected = Some(&reply.injected_text);
        }
    }

    let dir = cfg.hooks.state_dir.as_path();
    if let Some(text) = injected {
        if cfg.hooks.mode == "full" {
            // Exactly one line of compact JSON. `writeln!` rather than
            // `println!` because `println!` **panics** when the write fails,
            // and a panic here would reach the hook in `main`, exit 0 and
            // flush nothing — correct, but by accident. Stdout is a
            // `LineWriter`, so the `\n` is also the flush, which is what makes
            // the panic hook's `exit(0)` unable to truncate a line already
            // handed over.
            let line = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit",
                    "additionalContext": text,
                }
            })
            .to_string();
            let _ = writeln!(std::io::stdout(), "{line}");
        } else if let Err(e) = append_shadow(
            cfg, dir, input, bank_id, query, text, returned, tokens, now_ms,
        ) {
            super::debug(&cfg.hooks, &format!("recall: shadow log failed: {e}"));
        }
    }

    let diagnostic = serde_json::json!({
        "ts": now_ms,
        "mode": cfg.hooks.mode,
        "status": status,
        "http_status": outcome.http_status(),
        "session_id": input.session_id,
        "bank_id": bank_id,
        "prompt_chars": query.chars().count(),
        "returned": returned,
        "tokens": tokens,
        "injected_bytes": injected.map_or(0, str::len),
        "injected_text": injected,
        // The two numbers that answer "why is it injecting nothing *now*",
        // read before this invocation's own update.
        "transport_failures": before.map_or(0, |s| s.transport_failures),
        "breaker_open_until_ms": before.map_or(0, |s| s.breaker_open_until_ms),
    });
    if let Err(e) = write_diagnostic(dir, &diagnostic.to_string()) {
        super::debug(
            &cfg.hooks,
            &format!("recall: last_recall write failed: {e}"),
        );
    }
}

/// One JSONL line per injected recall, rotated at `shadow_log_max_bytes`.
///
/// This is the AC-1 instrument: in `shadow` mode the model sees nothing and
/// this file records, prompt by prompt, what MemGarden *would* have said while
/// legacy is still driving the session.
#[allow(clippy::too_many_arguments)]
fn append_shadow(
    cfg: &Config,
    dir: &Path,
    input: &hookio::HookInput,
    bank_id: &str,
    query: &str,
    injected_text: &str,
    returned: usize,
    tokens: usize,
    now_ms: i64,
) -> std::io::Result<()> {
    state::ensure_dir(dir)?;
    let path = dir.join(SHADOW_LOG);
    // `metadata` here would follow a symlink and report the *target's* size;
    // `open_regular` below refuses the symlink anyway, so use the same lstat
    // for both decisions and keep them from disagreeing.
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.len() >= cfg.hooks.shadow_log_max_bytes) {
        // ponytail: one generation. `.1` is overwritten, so the retained
        // history is between one and two times `shadow_log_max_bytes`. A
        // numbered ladder if an AC-1 run ever needs more than 64 MB of
        // history, which is ~40k prompts.
        let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
    }
    let line = serde_json::json!({
        "ts": now_ms,
        "session_id": input.session_id,
        "bank_id": bank_id,
        "prompt_chars": query.chars().count(),
        "returned": returned,
        "tokens": tokens,
        "injected_text": injected_text,
    })
    .to_string();
    let mut file = open_regular(&path, true)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")
}

fn write_diagnostic(dir: &Path, body: &str) -> std::io::Result<()> {
    state::ensure_dir(dir)?;
    let mut file = open_regular(&dir.join(LAST_RECALL), false)?;
    file.write_all(body.as_bytes())
}

/// Opens a file under the state dir for writing **without ever following a
/// symlink planted at that path**, at 0600.
///
/// `File::create` is `O_CREAT|O_WRONLY|O_TRUNC`: it follows a symlink and
/// truncates the target, which C2b measured on the lock file against a planted
/// `sX.lock -> /outside/precious.conf`. `append(true)` does not truncate, but
/// it still *writes through* the link, which for a log the daemon controls the
/// content of is not better.
///
/// So: an `lstat` decides. A regular file is opened in place; anything else —
/// symlink, fifo, directory — is refused; an absent path is `create_new`, which
/// fails rather than following a link planted between the two syscalls.
///
/// // ponytail: `O_NOFOLLOW` on the open is the airtight version and needs
/// // `libc`, which this crate's CI-enforced dependency closure refuses. The
/// // residual race needs write access to a 0700 directory, at which point the
/// // attacker can write the state files directly.
fn open_regular(path: &Path, append: bool) -> std::io::Result<File> {
    let mut options = File::options();
    if append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => options.open(path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        )),
        Err(_) => options.create_new(true).open(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HooksConfig {
        Config::defaults().unwrap().hooks
    }

    fn input(prompt: &str, user_prompt: &str) -> hookio::HookInput {
        hookio::HookInput {
            prompt: prompt.to_string(),
            user_prompt: user_prompt.to_string(),
            ..Default::default()
        }
    }

    /// legacy `recall.py:127-128`, both halves: either spelling is accepted,
    /// and the gate is on the **trimmed** value.
    #[test]
    fn either_prompt_spelling_is_accepted_and_short_ones_are_refused() {
        assert_eq!(
            usable_prompt(&input("hello there", "")),
            Some("hello there")
        );
        assert_eq!(
            usable_prompt(&input("", "hello there")),
            Some("hello there")
        );
        // `prompt` wins when both are set, matching legacy's `or`.
        assert_eq!(usable_prompt(&input("first", "second")), Some("first"));
        // Trimmed before the gate, and the trimmed form is what is sent.
        assert_eq!(usable_prompt(&input("  spaced  ", "")), Some("spaced"));

        for short in ["", "   ", "ok", "yes", "four", " a\n"] {
            assert!(
                usable_prompt(&input(short, "")).is_none(),
                "{short:?} must not reach the daemon"
            );
        }
        // Exactly at the boundary: 5 characters recalls, 4 does not.
        assert!(usable_prompt(&input("abcde", "")).is_some());
        assert!(usable_prompt(&input("abcd", "")).is_none());
    }

    /// The unit is characters, and getting it wrong is not cosmetic here: a
    /// byte gate would send `안녕하` (3 chars, 9 bytes) and refuse nothing it
    /// should have refused, paying a round trip per short Korean turn.
    #[test]
    fn the_prompt_gate_counts_characters_not_bytes() {
        assert_eq!("안녕하".len(), 9);
        assert!(usable_prompt(&input("안녕하", "")).is_none());
        assert_eq!("안녕하세요".chars().count(), 5);
        assert!(usable_prompt(&input("안녕하세요", "")).is_some());
    }

    #[test]
    fn the_query_is_truncated_by_characters_on_a_char_boundary() {
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("", 10), "");
        // The case a byte slice would panic on rather than merely shorten.
        let korean = "한".repeat(1000);
        let cut = truncate_chars(&korean, 800);
        assert_eq!(cut.chars().count(), 800);
        assert_eq!(cut.len(), 2400);
        // And the default budget cannot produce a query over the daemon's
        // 8 KB `MAX_QUERY_BYTES`, which is what the config bound guarantees.
        assert!(cut.len() <= 8 * 1024);
    }

    #[test]
    fn the_breaker_is_open_only_inside_a_window_it_could_have_written() {
        let mut st = SessionState::new("s1", "b1");
        let cfg = cfg();
        let now = 1_000_000i64;
        let cooldown = cooldown_ms(&cfg);

        // Closed by default, and closed at both ends of the window.
        assert!(!breaker_open(&st, &cfg, now));
        st.breaker_open_until_ms = now;
        assert!(!breaker_open(&st, &cfg, now), "an expired stamp is closed");
        st.breaker_open_until_ms = now + 1;
        assert!(breaker_open(&st, &cfg, now));
        st.breaker_open_until_ms = now + cooldown;
        assert!(breaker_open(&st, &cfg, now), "the far edge is still open");

        // **A stamp we could not have written is not a throttle.** One
        // millisecond past a full cooldown, an NTP step, and the value a
        // corrupted file is most likely to hold.
        for absurd in [now + cooldown + 1, now + cooldown * 1000, i64::MAX] {
            st.breaker_open_until_ms = absurd;
            assert!(
                !breaker_open(&st, &cfg, now),
                "{absurd} wedged recall off forever"
            );
        }
    }

    #[test]
    fn a_reply_missing_every_field_is_still_a_reply() {
        let reply: Reply = serde_json::from_slice(b"{}").unwrap();
        assert_eq!(reply.injected_text, "");
        assert_eq!((reply.counts.returned, reply.counts.tokens), (0, 0));
        // The real shape, with a field this hook does not read.
        let reply: Reply = serde_json::from_slice(
            br#"{"injected_text":"x","results":[{"id":1}],
                 "counts":{"candidates":30,"returned":8,"tokens":412}}"#,
        )
        .unwrap();
        assert_eq!(reply.injected_text, "x");
        assert_eq!((reply.counts.returned, reply.counts.tokens), (8, 412));
    }

    /// The rule the whole file exists to keep: nothing under the state dir is
    /// opened through a symlink. Both spellings — the truncating one and the
    /// appending one — because `append` not truncating is not the property
    /// that matters.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_refused_by_both_open_modes() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("precious.conf");
        std::fs::write(&victim, b"do not touch").unwrap();
        for (name, append) in [("trunc", false), ("append", true)] {
            let link = dir.path().join(name);
            std::os::unix::fs::symlink(&victim, &link).unwrap();
            assert!(open_regular(&link, append).is_err(), "{name}");
        }
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch");

        // A file we created is opened normally, at 0600.
        use std::os::unix::fs::PermissionsExt;
        let real = dir.path().join("real.json");
        open_regular(&real, false)
            .unwrap()
            .write_all(b"{}")
            .unwrap();
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
