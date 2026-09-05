//! `memgarden` — the Claude Code hook binary (Phase C).
//!
//! # The one guarantee this crate exists to make: **never exit 2**
//!
//! On `UserPromptSubmit`, exit 2 "Blocks prompt processing and erases the
//! prompt" — it deletes what the user typed. On `Stop` it prevents the turn
//! from ending. `recall.py:287-291` exits 2 when `debug` is set, so any
//! unhandled exception in the legacy recall hook erases a prompt.
//!
//! That was live, not hypothetical: the user's real
//! `~/.hindsight/claude-code.json` carried `debug: true` until **2026-08-03**,
//! when it was set to `false` after this hazard was found. Legacy's own
//! default is `false` and the `coding` preset does not set it — but the flag
//! is one env var (`HINDSIGHT_DEBUG`) away from erasing prompts again, which
//! is why the answer here is a structural guarantee rather than a setting.
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
//! C2a shipped the foundation and **no user-facing hook**: `hook noop` only.
//! That was deliberate sequencing — the measuring instrument lands first, so
//! every later Phase C PR reports a delta against an established baseline
//! (`src/bin/hook_bench.rs`) instead of against a number from yesterday.
//!
//! C2b adds the first subcommand that does work: `hook session-start`, and the
//! detached `hook catchup` child it spawns (`src/cmd/`). C3 adds `hook recall`
//! — the first subcommand on the per-prompt path and the only one that writes
//! to stdout. The transcript reader (C4a) and `retain`/`session-end` (C4b)
//! follow.

pub mod bank;
pub mod cmd;
pub mod hookio;
pub mod http;
pub mod settings;
pub mod state;
pub mod transcript;

use std::process::ExitCode;

pub use memgarden_core::config::{ENV_DAEMON_URL, ENV_HOOKS_DISABLE};

/// Runs one subcommand.
///
/// The return type is `ExitCode` and it has exactly two inhabitants here:
/// `SUCCESS` from every hook path, and `FAILURE` (1) from the `hooks
/// install|uninstall` family, which is typed by a human and must be able to
/// refuse visibly. **2 is not constructible**, which is the guarantee this
/// crate exists to make — see the module docs.
///
/// `args` excludes argv[0].
pub fn dispatch(args: &[String]) -> ExitCode {
    // The installer family is not a hook: it is run by hand, it may exit 1,
    // and — the part that is easy to get backwards — `MEMGARDEN_HOOKS_DISABLE`
    // must NOT silence it. A tool that reports whether the hooks are wired has
    // to work in exactly the state the user is asking about. Hence before the
    // disable check rather than after it.
    // **Only the three real subcommands.** Routing on `args[0] == "hooks"`
    // alone made every `hooks <anything>` argv exempt from the disable switch
    // *and* able to exit 1 — so a typo in a hand-edited settings.json would
    // print usage on a hook event, where before this PR every unrecognised
    // argv exited 0 in silence. The unknown arm has no reason to be exempt.
    if args.first().map(String::as_str) == Some("hooks")
        && let Some(sub @ ("install" | "uninstall" | "status")) = args.get(1).map(String::as_str)
    {
        return cmd::hooks::run(sub, args);
    }
    // Run by hand, may exit 1, and must work with the hooks disabled — same
    // family as `hooks install`.
    if args.first().map(String::as_str) == Some("self-update") {
        return cmd::self_update::run(&args[1..]);
    }

    // Checked once, here, before the match and before any config load — so no
    // subcommand added later can forget it, and a user who has turned the
    // hooks off does not pay a TOML read to find out.
    if hooks_disabled(std::env::var_os(ENV_HOOKS_DISABLE).as_deref()) {
        return ExitCode::SUCCESS;
    }

    match (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    ) {
        // The benchmark's paired baseline arm (plan §Measurement design): the
        // same binary, the same dynamic-link and page-cache state, parsing
        // argv and exiting. Arm A minus this is the subcommand's own work.
        (Some("hook"), Some("noop")) => {}

        // `SessionStart`. Emits nothing on stdout by design — see
        // `cmd::session_start`.
        (Some("hook"), Some("session-start")) => cmd::session_start::run(),

        // `UserPromptSubmit`. The only subcommand whose stdout is meant to be
        // read — and only in `full` mode. See `cmd::recall`.
        (Some("hook"), Some("recall")) => cmd::recall::run(),

        // `Stop`. Never writes to stdout, and the one subcommand that can lose
        // memory if its cursor is wrong. See `cmd::retain`.
        //
        // `args` is passed whole because the detached `session-end` child
        // reaches the same entry point as `hook retain --force --session <sid>
        // --end-reason <reason>`: one state machine, three callers.
        (Some("hook"), Some("retain")) => cmd::retain::run(args),

        // `SessionEnd`. Spawns the detached child above and exits; it never
        // posts anything itself. See `cmd::session_end`.
        (Some("hook"), Some("session-end")) => cmd::session_end::run(),

        // Internal: the detached child `session-start` spawns. Reachable by
        // name so it can be run by hand with `--dry-run`, which is the only
        // way to observe a process whose three streams are `/dev/null`.
        (Some("hook"), Some("catchup")) => cmd::catchup::run(args),

        // Test-only. Reachable by name but not documented in `hooks status` or
        // the installer, because its entire job is to prove that a panic
        // reaches the hook in `main` and still exits 0 with empty stdout.
        (Some("hook"), Some("__panic")) => panic!("injected panic (memgarden hook __panic)"),

        // C5's wiring lives above the disable check; nothing reaches here.

        // Unknown subcommand, no subcommand, `--help`, a typo in the user's
        // settings.json: all exit 0 and say nothing. This is the arm that
        // `clap` would have exited 2 on.
        _ => {}
    }
    ExitCode::SUCCESS
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

    /// Every dispatch path returns normally, and every hook path returns
    /// success. The exit code itself is asserted against the real binary in
    /// `tests/never_exit_two.rs`; this pins that nothing in the argv match can
    /// `panic!` or diverge.
    ///
    /// `hooks …` is deliberately absent: it is the one family that may return
    /// `FAILURE`, and it reads the real environment. `tests/hooks_install.rs`
    /// drives it against a temp file instead.
    #[test]
    fn no_argv_shape_diverges() {
        for args in [
            vec![],
            vec!["hook"],
            vec!["hook", "noop"],
            vec!["hook", "noop", "--extra", "junk"],
            vec!["--help"],
            vec!["definitely-not-a-subcommand"],
        ] {
            let code = dispatch(&args.into_iter().map(String::from).collect::<Vec<_>>());
            assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        }
    }
}
