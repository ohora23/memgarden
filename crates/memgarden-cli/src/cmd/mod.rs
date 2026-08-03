//! The hook subcommands, and the three things all of them share.
//!
//! C2a shipped `hook noop` and nothing else on purpose, so this module is
//! where the binary stops being a measuring instrument and starts doing work.
//! Everything here is written for the same guarantee as the rest of the crate:
//! there is no error channel out of a subcommand, because every error's
//! correct handling is "exit 0".

pub mod catchup;
pub mod recall;
pub mod session_start;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use memgarden_core::config::{Config, HooksConfig};

use crate::http::Timeouts;

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
