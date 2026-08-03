//! The guarantee, asserted against the real binary rather than against
//! `dispatch`.
//!
//! Exit 2 on `UserPromptSubmit` "Blocks prompt processing and erases the
//! prompt". Legacy does exactly that under its live `debug: true`
//! (`recall.py:287-291`). These tests are the reason `main` has no `?`, no
//! `clap`, and a panic hook.
//!
//! They assert `== 0`, not `!= 2`: the stronger claim is the one we can
//! actually make from inside the process, and a test that only excluded 2
//! would pass on a binary that had started exiting 1.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run(args: &[&str], stdin: &[u8], env: &[(&str, &str)]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_memgarden"))
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn memgarden");
    // Best-effort, and it has to be: `hook noop` never reads stdin, so it can
    // exit before we finish writing and hand us EPIPE. That is the subject
    // under test behaving correctly — a hook that exits without draining its
    // input — and a `.expect()` here made CI fail on a race in the *harness*.
    // The exit code is the assertion; the write is only a fixture.
    let _ = child.stdin.take().expect("stdin").write_all(stdin);
    child.wait_with_output().expect("wait")
}

fn assert_exit_zero(out: &Output, what: &str) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{what}: exit {:?}, stderr {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_injected_panic_exits_zero_with_empty_stdout() {
    let out = run(&["hook", "__panic"], b"", &[]);
    assert_exit_zero(&out, "panic");
    // `process::exit(0)` from the panic hook skips Rust's end-of-main stdout
    // flush on purpose: a half-written `additionalContext` line must never
    // reach the model.
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
    // The operator still gets told, on the stream that is only ever a log.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("injected panic"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn every_argv_shape_that_is_not_a_subcommand_exits_zero() {
    for args in [
        vec![],
        vec!["hook"],
        vec!["definitely-not-a-subcommand"],
        // The two shapes `clap` would have exited 2 on.
        vec!["--help"],
        vec!["hook", "--nonexistent-flag"],
        vec!["hook", "recall"], // valid in C3, unknown today
    ] {
        let out = run(&args, b"", &[]);
        assert_exit_zero(&out, &format!("{args:?}"));
        assert!(out.stdout.is_empty(), "{args:?} wrote to stdout");
    }
}

#[test]
fn empty_and_malformed_stdin_exit_zero() {
    for stdin in [&b""[..], b"   ", b"not json", b"{\"session_id\":"] {
        let out = run(&["hook", "noop"], stdin, &[]);
        assert_exit_zero(&out, &format!("stdin {stdin:?}"));
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn the_disable_switch_exits_zero_and_stays_silent() {
    let out = run(
        &["hook", "noop"],
        b"{}",
        &[("MEMGARDEN_HOOKS_DISABLE", "1")],
    );
    assert_exit_zero(&out, "disabled");
    assert!(out.stdout.is_empty());
    // Even a subcommand that would otherwise panic is short-circuited, which
    // is the point of checking the switch before the match rather than inside
    // each arm.
    let out = run(
        &["hook", "__panic"],
        b"",
        &[("MEMGARDEN_HOOKS_DISABLE", "true")],
    );
    assert_exit_zero(&out, "disabled panic");
    assert!(out.stderr.is_empty(), "{:?}", out.stderr);
}

/// Arm B of the benchmark, and the only subcommand C2a ships. It must stay
/// silent on stdout: `hook noop` is measured against `hook recall`, and a
/// baseline that writes is not a baseline.
#[test]
fn noop_is_silent() {
    let out = run(&["hook", "noop"], b"{\"session_id\":\"s1\"}", &[]);
    assert_exit_zero(&out, "noop");
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
}
