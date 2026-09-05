//! The hook subcommands, and the three things all of them share.
//!
//! C2a shipped `hook noop` and nothing else on purpose, so this module is
//! where the binary stops being a measuring instrument and starts doing work.
//! Everything here is written for the same guarantee as the rest of the crate:
//! there is no error channel out of a subcommand, because every error's
//! correct handling is "exit 0".

pub mod catchup;
pub mod hooks;
pub mod recall;
pub mod retain;
pub mod self_update;
pub mod session_end;
pub mod session_start;
pub mod update_check;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use memgarden_core::config::{Config, HooksConfig};

use crate::http::{self, HttpError, Target, Timeouts};
use crate::state::SessionState;

/// legacy/daemon: `memgarden_store::sessions::MAX_SESSION_ID_BYTES` — the bound
/// `store::sessions::upsert` enforces, mirrored rather than imported because
/// `memgarden-store` is exactly what this crate's dependency budget keeps out
/// (`Cargo.toml`, CI-enforced).
///
/// Checked client-side because `session_id` arrives on untrusted stdin, which
/// `hookio` bounds at 8 MB: without this, an 8 MB id would be written into a
/// state file, POSTed as a body the daemon rejects, and passed as an argv
/// element that blows `ARG_MAX`. Claude Code sends 36-character uuids.
///
/// It lives here rather than in one subcommand because C3 is the second caller
/// and C4b is the third.
pub const MAX_SESSION_ID_BYTES: usize = 200;

/// daemon: `routes/sessions.rs::MAX_REASON_BYTES` — the bound
/// `POST …/sessions` enforces on `end_reason`, mirrored for the same reason
/// `MAX_SESSION_ID_BYTES` is.
///
/// Checked client-side because `reason` arrives on untrusted stdin (bounded
/// only by `hookio`'s 8 MB) and C4b passes it **as an argv element** to a
/// detached child. An 8 MB argv element is an `E2BIG` on the `execve`, which
/// is a lost `session-end` retain rather than a 400 — a failure one layer
/// further out than the daemon's own check can reach. Claude Code sends one of
/// six documented words.
pub const MAX_END_REASON_BYTES: usize = 64;

/// Loads the config and hands it back **only when the hooks are on**.
///
/// Two switches, deliberately at two different depths:
///
/// * `MEMGARDEN_HOOKS_DISABLE` is checked in [`crate::dispatch`], before any
///   subcommand and before this function, so a user who has turned the hooks
///   off does not pay a TOML read to find out.
/// * `[hooks] enabled = false` can only be seen *after* that read, so it is
///   checked here — once, where every subcommand routes through.
///
/// C2a shipped the second switch inert: nothing loaded config, so `enabled`
/// was a documented knob that changed no behaviour. This is the PR that makes
/// it real, and `the_config_switch_makes_no_request_and_writes_no_state` is
/// what keeps it that way.
pub fn enabled_config() -> Option<Config> {
    let cfg = Config::load().ok()?;
    cfg.hooks.enabled.then_some(cfg)
}

/// One stderr line, gated on `[hooks] debug` (plan §Binding decisions #3:
/// stdout is a protocol, stderr is a log).
///
/// **This can never change an exit code**, which is the entire difference
/// between it and legacy's `debug`: `recall.py:287-291` exits 2 when the flag
/// is set, and on `UserPromptSubmit` that erases the user's prompt.
pub fn debug(cfg: &HooksConfig, message: &str) {
    if cfg.debug {
        let _ = writeln!(std::io::stderr(), "memgarden: {message}");
    }
}

/// The interactive-path budget: a loopback connect plus one small round trip.
///
/// `recall_timeout_ms` (400) rather than `retain_timeout_ms` (5000) because
/// the requests this covers are single-row writes with no `prepare()` behind
/// them — nothing like the tokenize-twice-then-202 that justifies retain's 5 s.
/// The plan does not name a knob for `session-start`; this is the choice, made
/// once here so `session-end` (C4b) inherits it rather than picking again.
pub fn interactive_timeouts(cfg: &HooksConfig) -> Timeouts {
    Timeouts::from_ms(cfg.connect_timeout_ms, cfg.recall_timeout_ms)
}

/// The retain-POST budget, and **only** the retain POST.
///
/// `retain_timeout_ms` is 5 s because `POST …/retain`'s `prepare()` is
/// synchronous before the 202: tokenize twice with `cl100k_base`, upsert the
/// document, insert the ledger and job rows. ~0.6 s was observed on a 9.4 MB
/// initial retain, so 400 ms would abandon a legitimate retain *after* the
/// daemon had already queued it — the one failure shape §Binding decisions #8
/// exists to make recoverable, arrived at by a client that gave up early.
///
/// C4b's other two requests — the reconcile `GET /v1/retain/{job_id}` and the
/// 404 bank-create — deliberately use [`interactive_timeouts`] instead: they
/// are single-row operations with no `prepare()` behind them, and the
/// reconcile in particular runs on **gated** turns, where a 5 s budget against
/// a hung daemon would cost a `Stop` five seconds before the breaker ever got
/// to skip a socket.
pub fn retain_timeouts(cfg: &HooksConfig) -> Timeouts {
    Timeouts::from_ms(cfg.connect_timeout_ms, cfg.retain_timeout_ms)
}

/// Whether the circuit breaker is open *and* the stamp that says so is one we
/// could plausibly have written.
///
/// **Both sides of the window are guarded.** `breaker_open_until_ms` is read
/// from a file, and a value far enough in the future turns "skip for 60 s"
/// into "never talk to the daemon again" — silently. No attacker is required:
/// an NTP step, a VM resume or a dual-boot RTC produces one. Anything more
/// than one cooldown ahead cannot have come from `breaker_cooldown_secs`, so
/// it is treated as closed.
///
/// It lives here, next to [`poisoned_within_throttle`], because the same shape
/// of bug was found independently in C2b (`poisoned_at`) and C3
/// (`breaker_open_until_ms`) and C4b is the third caller of both. One guard
/// where every caller routes through is a smaller diff than a guard per hook —
/// and it is the only version a future hook cannot forget to write.
pub fn breaker_open(state: &SessionState, cfg: &HooksConfig, now_ms: i64) -> bool {
    let until = state.breaker_open_until_ms;
    now_ms < until && until <= now_ms.saturating_add(breaker_cooldown_ms(cfg))
}

pub fn breaker_cooldown_ms(cfg: &HooksConfig) -> i64 {
    i64::try_from(cfg.breaker_cooldown_secs.saturating_mul(1000)).unwrap_or(i64::MAX)
}

/// Whether `poisoned_at` is set and its retry window has not elapsed.
///
/// Poisoning is a **slow-retry state, not a latch**: a session the daemon has
/// durably rejected retries once per `poison_retry_secs` rather than every
/// turn, and any success clears it.
///
/// `poisoned_at <= now_ms` is the future-stamp guard, for
/// [`breaker_open`]'s reason and with a worse consequence here: the window is
/// an hour rather than a minute, and the process that reads it writes to
/// `/dev/null`.
pub fn poisoned_within_throttle(state: &SessionState, retry_secs: u64, now_ms: i64) -> bool {
    let Some(poisoned_at) = state.poisoned_at else {
        return false;
    };
    // Saturating throughout: `poison_retry_secs` is a `u64` from config and an
    // operator who writes `u64::MAX` should get "never retry", not an overflow
    // panic in a process nobody is watching.
    let window_ms = i64::try_from(retry_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    poisoned_at <= now_ms && now_ms < poisoned_at.saturating_add(window_ms)
}

/// The **only** production way to build a [`Target`]: the daemon's url plus
/// the identity token every one of its responses must carry.
///
/// `Target::parse` alone is left for the in-process transport tests and the
/// benchmark's stub, which have no daemon to identify. Routing every
/// subcommand through here is the same "one guard where all callers pass"
/// rule that put the path check in `http::request` — a future subcommand that
/// called `Target::parse` directly would silently opt out of the check, so
/// there is exactly one place to look.
///
/// An unreadable token is [`HttpError::Token`] and therefore **transport**
/// class at every caller: it must not be a config fault, because a config
/// fault moves no counter, the breaker would never open, and every prompt for
/// the rest of the session would pay a full round trip to learn the same
/// thing.
pub fn target(cfg: &HooksConfig) -> Result<Target, HttpError> {
    let path = memgarden_core::paths::daemon_token_path()
        .map_err(|e| HttpError::Token(format!("cannot resolve the token path: {e}")))?;
    let token = std::fs::read_to_string(&path)
        .map_err(|e| HttpError::Token(format!("{}: {e}", path.display())))?;
    let token = token.trim();
    if token.is_empty() {
        return Err(HttpError::Token(format!("{} is empty", path.display())));
    }
    Target::parse_verified(&cfg.daemon_url, token.to_string())
}

/// What the daemon's `sessions` mirror is allowed to tell recovery.
///
/// **`byte_offset` is deliberately not a field**, and that is the whole point
/// of this struct existing rather than reusing `SessionResponse`. That one
/// carries both cursors; seeding `offset` from the optimistic one skips
/// exactly the bytes the dual cursor exists to protect, because it is what
/// some hook *POSTed* — already ahead of reality after a failed job or a
/// byte-budget 429. C2a's `SessionState::recovered` names its parameter
/// `confirmed_offset` to make the right thing the easy thing; **a field that
/// is never deserialized cannot be misused at all**, so the wrong cursor is
/// not merely discouraged here, it is not present.
///
/// Both fields are `i64` and clamped rather than `u64`: a negative would make
/// the *whole* struct fail to parse, silently dropping `chunk_index` too, and
/// falling back to a full re-ingest over a value the daemon cannot produce.
///
/// They are numbers, so `#[serde(default)]` is enough here — but see
/// `hookio`'s module docs for the rule that is **not** obvious and that C4b
/// got wrong once: a daemon field that can be `null` must be `Option<T>`,
/// because `default` covers an absent key and not an explicit null.
///
/// It lives here rather than in `session_start` because C4b is the second
/// caller — see [`recover`].
#[derive(Debug, serde::Deserialize)]
pub struct Mirror {
    #[serde(default)]
    pub confirmed_offset: i64,
    #[serde(default)]
    pub chunk_index: i64,
}

impl Mirror {
    /// The mirror as a fresh `SessionState`, always through the constructor
    /// that takes only the durable cursor.
    pub fn into_state(&self, session_id: &str, bank_id: &str) -> SessionState {
        SessionState::recovered(
            session_id,
            bank_id,
            self.confirmed_offset.max(0) as u64,
            self.chunk_index.max(0) as u64,
        )
    }
}

/// `GET /v1/banks/{bank}/sessions/{sid}` — the wiped-state-dir recovery.
///
/// `session-start` gets the same shape back from its upsert and does not need
/// this. **`retain` does**, and that is not symmetry for its own sake: a state
/// dir wiped *mid-session* is a case `session-start` cannot cover, because it
/// does not fire mid-session and `retain` is the hook that does. §Failure
/// posture says the case is handled by `session-start` preferring the mirror;
/// that is true only of a wipe between sessions.
///
/// Without it, retain re-ingests the whole transcript under **chunk 0's bare
/// `document_id`**, and `routes/retain.rs` rebuilds `documents.metadata` from
/// scratch — overwriting the real chunk 0's `message_count`/`files_modified`.
/// The daemon's own `RetainRequest::chunk` doc names this exact scenario as
/// the reason C1 added the column, so without this call C1 built a column for
/// a recovery nothing performed.
///
/// `None` for anything at all — no daemon, a 404, an unreadable body — and the
/// caller starts from zero, which is what it would have done anyway.
pub fn recover(cfg: &HooksConfig, bank_id: &str, session_id: &str) -> Option<Mirror> {
    let target = target(cfg).ok()?;
    let path = format!(
        "/v1/banks/{}/sessions/{}",
        http::encode_path_segment(bank_id),
        http::encode_path_segment(session_id)
    );
    let response = http::get(&target, &path, &interactive_timeouts(cfg)).ok()?;
    response
        .is_success()
        .then(|| serde_json::from_slice(&response.body).ok())
        .flatten()
}

/// Spawns a child that outlives us, wired to `/dev/null` on all three streams.
///
/// Both halves are load-bearing and neither is a style preference:
///
/// * **`Stdio::null()` on stdout.** On `SessionStart`, a hook's stdout *is*
///   the model's context channel (plan §Binding decisions #3), so anything the
///   child writes lands in the user's conversation. It is also how a
///   "detached" child hangs its supervisor: an inherited pipe stays open after
///   the parent exits, and whoever is reading that pipe waits for the child.
///   That would invalidate C2a's measured 0.243 ms arm B by making the hook's
///   observed cost the *child's* lifetime.
/// * **`process_group(0)`.** `setsid`-equivalent: the child leaves our process
///   group, so the terminal going away — or a `SIGINT` to the foreground
///   group — does not take it with us.
///
/// Failure is silence. A missing or unspawnable child is a lost catch-up,
/// which the next session's catch-up covers; it is never a failed session.
pub fn spawn_detached(exe: &Path, args: &[&str]) {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    // Not waited on: the parent is about to exit and `init` reaps the child.
    // Waiting is the one thing "detached" must not do.
    let _ = command.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HooksConfig {
        Config::defaults().unwrap().hooks
    }

    /// Moved here from `cmd::recall` when C4b became the third caller. The
    /// window is asserted at **both** ends and one millisecond past the far
    /// one, because the mutation this pins — dropping the upper conjunct — is
    /// invisible to any test that only checks "open while inside".
    #[test]
    fn the_breaker_is_open_only_inside_a_window_it_could_have_written() {
        let mut st = SessionState::new("s1", "b1");
        let cfg = cfg();
        let now = 1_000_000i64;
        let cooldown = breaker_cooldown_ms(&cfg);

        assert!(!breaker_open(&st, &cfg, now));
        st.breaker_open_until_ms = now;
        assert!(!breaker_open(&st, &cfg, now), "an expired stamp is closed");
        st.breaker_open_until_ms = now + 1;
        assert!(breaker_open(&st, &cfg, now));
        st.breaker_open_until_ms = now + cooldown;
        assert!(breaker_open(&st, &cfg, now), "the far edge is still open");

        for absurd in [now + cooldown + 1, now + cooldown * 1000, i64::MAX] {
            st.breaker_open_until_ms = absurd;
            assert!(
                !breaker_open(&st, &cfg, now),
                "{absurd} wedged the hook off forever"
            );
        }
    }

    /// Moved here from `cmd::catchup` for the same reason, and asserted on the
    /// same shape: both ends of the window, plus a stamp from the future.
    #[test]
    fn poisoning_throttles_inside_its_window_and_never_from_the_future() {
        let mut st = SessionState::new("s1", "b1");
        let retry_secs = 3600u64;
        let window = 3_600_000i64;
        let now = 1_000_000_000i64;

        assert!(
            !poisoned_within_throttle(&st, retry_secs, now),
            "an unpoisoned session is never throttled"
        );
        st.poisoned_at = Some(now);
        assert!(poisoned_within_throttle(&st, retry_secs, now));
        st.poisoned_at = Some(now - window + 1);
        assert!(poisoned_within_throttle(&st, retry_secs, now));
        // The far edge: exactly one window old is a retry, not a skip.
        st.poisoned_at = Some(now - window);
        assert!(!poisoned_within_throttle(&st, retry_secs, now));

        for future in [now + 1, now + window * 1000, i64::MAX] {
            st.poisoned_at = Some(future);
            assert!(
                !poisoned_within_throttle(&st, retry_secs, now),
                "poisoned_at = {future} (now = {now}) must not throttle"
            );
        }
        // `u64::MAX` seconds is "never retry", not an overflow panic.
        st.poisoned_at = Some(now);
        assert!(poisoned_within_throttle(&st, u64::MAX, now));
    }

    /// The two budgets are not interchangeable, and a mutant that swaps them
    /// is silent: `session-start` with 5 s looks healthy and `retain` with
    /// 400 ms abandons a retain the daemon has already queued. Distinguishable
    /// values, asserted in both directions.
    #[test]
    fn the_retain_budget_is_the_retain_budget_and_not_the_interactive_one() {
        let mut cfg = cfg();
        cfg.recall_timeout_ms = 400;
        cfg.retain_timeout_ms = 5000;
        assert_eq!(retain_timeouts(&cfg).io.as_millis(), 5000);
        assert_eq!(interactive_timeouts(&cfg).io.as_millis(), 400);
        // Both take the same connect budget: reaching the daemon costs the
        // same microseconds whichever question we are about to ask.
        assert_eq!(
            retain_timeouts(&cfg).connect,
            interactive_timeouts(&cfg).connect
        );
    }

    /// The daemon 400s an `end_reason` over its own bound, and an `execve`
    /// fails outright well before that. The mirrored constant has to be the
    /// daemon's, not a rounder number near it.
    #[test]
    fn the_mirrored_bounds_match_the_daemons() {
        assert_eq!(
            MAX_SESSION_ID_BYTES,
            memgarden_store_sessions_max_session_id_bytes()
        );
        assert_eq!(MAX_END_REASON_BYTES, 64);
    }

    /// `memgarden-store` is not in this crate's dependency budget, so the
    /// bound is restated rather than imported. This function is where that
    /// restatement is written down once.
    fn memgarden_store_sessions_max_session_id_bytes() -> usize {
        200
    }
}
