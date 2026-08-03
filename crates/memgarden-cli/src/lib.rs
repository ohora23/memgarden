//! `memgarden` — the Claude Code hook binary (Phase C).
//!
//! # The one guarantee this crate exists to make: **never exit 2**
//!
//! On `UserPromptSubmit`, exit 2 "Blocks prompt processing and erases the
//! prompt" — it deletes what the user typed. On `Stop` it prevents the turn
//! from ending. Legacy has this live: `recall.py:287-291` exits 2 when
//! `debug` is set, and `debug: true` is in the user's real
//! `~/.hindsight/claude-code.json`, so any unhandled exception in the legacy
//! recall hook erases a prompt.
//!
//! Here it is structural rather than careful:
//!
//! * `main` returns `ExitCode::SUCCESS` on every path and no `?` propagates
//!   out of it;
//! * a panic hook prints one line to stderr and `exit(0)`s;
//! * there is no `clap` — its usage errors exit 2, and that alone disqualifies
//!   it from this binary;
//! * an unknown subcommand, empty stdin and malformed stdin are all silent
//!   successes.
//!
//! The guarantee is stated as **never 2**, not "never non-zero", because the
//! remaining non-zero exits are ones no code inside the process can prevent:
//! `SIGSEGV`/`abort` give 139/134, and a missing binary makes a shell launcher
//! return 127. None of those is 2.
//!
//! # What is here, and what is not
//!
//! C2a ships the foundation and **no user-facing hook**: `hook noop` only.
//! That is deliberate sequencing — the measuring instrument lands first, so
//! every later Phase C PR reports a delta against an established baseline
//! (`src/bin/hook_bench.rs`) instead of against a number from yesterday.

pub mod bank;
pub mod hookio;
pub mod http;
pub mod state;

pub use memgarden_core::config::{ENV_DAEMON_URL, ENV_HOOKS_DISABLE};

/// Runs one subcommand. Returns `()`: there is no error channel out of here
/// by construction, because every error's correct handling is "exit 0".
///
/// `args` excludes argv[0].
pub fn dispatch(args: &[String]) {
    // Checked once, here, before the match and before any config load — so no
    // subcommand added later can forget it, and a user who has turned the
    // hooks off does not pay a TOML read to find out.
    if hooks_disabled(std::env::var_os(ENV_HOOKS_DISABLE).as_deref()) {
        return;
    }

    match (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    ) {
        // The benchmark's paired baseline arm (plan §Measurement design): the
        // same binary, the same dynamic-link and page-cache state, parsing
        // argv and exiting. Arm A minus this is the subcommand's own work.
        (Some("hook"), Some("noop")) => {}

        // Test-only. Reachable by name but not documented in `hooks status` or
        // the installer, because its entire job is to prove that a panic
        // reaches the hook in `main` and still exits 0 with empty stdout.
        (Some("hook"), Some("__panic")) => panic!("injected panic (memgarden hook __panic)"),

        // Unknown subcommand, no subcommand, `--help`, a typo in the user's
        // settings.json: all exit 0 and say nothing. This is the arm that
        // `clap` would have exited 2 on.
        _ => {}
    }
}

/// Whether `MEMGARDEN_HOOKS_DISABLE` is set to something truthy.
///
/// The truthy set matches `memgarden-core`'s (and the fork's `_cast_env`,
/// `lib/config.py:136`). They have to agree: this runs before the config load
/// and the config's copy runs after, so a disagreement would make
/// `hooks status` report the opposite of what the hooks actually do.
pub fn hooks_disabled(raw: Option<&std::ffi::OsStr>) -> bool {
    raw.and_then(std::ffi::OsStr::to_str)
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn the_disable_switch_matches_the_configs_truthy_set() {
        for on in ["1", "true", "TRUE", "yes", "Yes"] {
            assert!(hooks_disabled(Some(OsStr::new(on))), "{on}");
        }
        // "0" and "false" are not disables. Reading them as one would turn the
        // hooks off for anyone who wrote `MEMGARDEN_HOOKS_DISABLE=0` meaning
        // "on" — and the config half would then disagree with this half.
        for off in ["0", "false", "no", "", "maybe"] {
            assert!(!hooks_disabled(Some(OsStr::new(off))), "{off}");
        }
        assert!(!hooks_disabled(None));
    }

    /// Every dispatch path returns normally. The exit code itself is asserted
    /// against the real binary in `tests/never_exit_two.rs`; this pins that
    /// nothing in the argv match can `panic!` or diverge.
    #[test]
    fn no_argv_shape_diverges() {
        for args in [
            vec![],
            vec!["hook"],
            vec!["hook", "noop"],
            vec!["hook", "noop", "--extra", "junk"],
            vec!["hook", "recall"], // not implemented until C3
            vec!["--help"],
            vec!["definitely-not-a-subcommand"],
        ] {
            dispatch(&args.into_iter().map(String::from).collect::<Vec<_>>());
        }
    }
}
