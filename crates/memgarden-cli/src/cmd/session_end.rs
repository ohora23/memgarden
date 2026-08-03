//! `hook session-end` — the `SessionEnd` event.
//!
//! It reads stdin, spawns a **detached** `hook retain --force` child, and
//! exits. It never posts anything itself, and it writes nothing to stdout —
//! `SessionEnd` has no decision control at all (plan §Binding decisions #3).
//!
//! # Why it detaches instead of retaining inline
//!
//! The earlier draft did an inline retain with a 2 s client timeout. That was
//! wrong in a way that would have been silent.
//!
//! `POST …/retain`'s `prepare()` tokenizes the transcript **twice** with
//! `cl100k_base`, measured at **19.18 MB/s** on this machine, and the uncapped
//! pass is not bounded by `retain.max_initial_messages`. At `max_post_bytes =
//! 24 MB` that is ≈2.1 s of tokenization alone — over any ceiling that fits
//! `SessionEnd`'s documented 1.5 s shared budget, on exactly the path the
//! oversize fallback exists for. And the failure mode is the bad one: **the
//! daemon queues the job while the hook records a timeout and does not
//! advance.** The cursor then sits behind a job that is about to succeed, and
//! the next session re-sends bytes the daemon already has.
//!
//! Detaching removes the deadline rather than tuning it, takes `SessionEnd`
//! off the shared budget rather than raising it, and reuses the mechanism C2b
//! already built for catch-up. The final retain's outcome stops being
//! observable from this hook's exit — it lands in `retain_jobs`, which is
//! where `hooks status` reads it from anyway.
//!
//! **It never stops the daemon** (plan §Binding decisions #10). Legacy's
//! `stop_daemon` (`lib/daemon.py`, `session_end.py:44`) is not ported: killing
//! a model-loading daemon at the end of every session is the restart race the
//! PRD exists to remove.

use crate::hookio;

use super::{MAX_END_REASON_BYTES, MAX_SESSION_ID_BYTES};

pub fn run() {
    let Some(input) = hookio::read_stdin() else {
        return;
    };
    let Some(cfg) = super::enabled_config() else {
        return;
    };
    if input.session_id.is_empty() || input.session_id.len() > MAX_SESSION_ID_BYTES {
        super::debug(&cfg.hooks, "session_end: unusable session_id");
        return;
    }
    // Bounded before it becomes an argv element, not after. `reason` arrives
    // on stdin, which `hookio` bounds at 8 MB; an 8 MB argv element is an
    // `E2BIG` on the `execve`, and the lost child is a lost final retain.
    // Truncated on a char boundary — `&s[..n]` panics on a non-boundary index,
    // which C3 found the neighbouring version of.
    let reason = truncate_bytes(&input.reason, MAX_END_REASON_BYTES);

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // Fixed slots, values in value positions: `cmd::retain::run` reads
    // `args[2]` as `--force`, `args[4]` as the session id and `args[6]` as the
    // reason, and a session id spelled `--force` therefore cannot turn the
    // turn gate off.
    super::spawn_detached(
        &exe,
        &[
            "hook",
            "retain",
            "--force",
            "--session",
            &input.session_id,
            "--end-reason",
            reason,
        ],
    );
}

/// Longest prefix of `s` within `max` **bytes**, cut on a char boundary.
///
/// Bytes because the bound it mirrors is the daemon's `MAX_REASON_BYTES`,
/// which is bytes; the char boundary because a Rust string slice on a
/// non-boundary index panics, and this process's whole contract is that it
/// cannot fail loudly.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let end = (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_is_bounded_on_a_char_boundary() {
        // The six documented values pass through untouched.
        for reason in [
            "clear",
            "resume",
            "logout",
            "prompt_input_exit",
            "bypass_permissions_disabled",
            "other",
        ] {
            assert_eq!(truncate_bytes(reason, MAX_END_REASON_BYTES), reason);
            assert!(reason.len() <= MAX_END_REASON_BYTES, "{reason}");
        }
        assert_eq!(truncate_bytes("", MAX_END_REASON_BYTES), "");

        // A hostile one is cut to the bound, and the result is still a string.
        let long = "x".repeat(8 * 1024);
        assert_eq!(truncate_bytes(&long, MAX_END_REASON_BYTES).len(), 64);

        // Korean: 64 bytes is 21 characters and one byte, so the cut lands
        // mid-character and must move back rather than panic.
        let korean = "한".repeat(400);
        let cut = truncate_bytes(&korean, MAX_END_REASON_BYTES);
        assert_eq!(cut.len(), 63);
        assert_eq!(cut.chars().count(), 21);

        // A bound smaller than the first character yields the empty prefix,
        // not a panic.
        assert_eq!(truncate_bytes("한", 1), "");
    }
}
