//! The measurement harness is half of C2a, so it gets a test rather than only
//! a manual run.
//!
//! **Divergence from the plan**, which specified an `#[ignore]`d test: an
//! ignored test is compiled and never executed, which catches a type error and
//! nothing else. At `--n 3` the whole harness — stub daemon, interleaving,
//! percentiles, the transport probe — runs in well under a second, so it runs
//! for real. The *reportable* numbers still come from a manual `--n 300` run
//! pasted into the PR body; a benchmark's output is not something CI should be
//! asserting thresholds on.

use std::process::Command;

#[test]
fn the_harness_produces_both_arms_and_a_paired_delta() {
    let out = Command::new(env!("CARGO_BIN_EXE_hook_bench"))
        .args([
            "--bin",
            env!("CARGO_BIN_EXE_memgarden"),
            "--n",
            "3",
            "--warmup",
            "1",
            "--transport-n",
            "12",
        ])
        .output()
        .expect("run hook_bench");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "hook_bench failed: {}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Both arms and the paired difference, which is the whole point: a
    // harness that reported only absolutes would be measuring the machine.
    assert!(stdout.contains("| A `hook noop` |"), "{stdout}");
    assert!(stdout.contains("| B `hook noop` (baseline) |"), "{stdout}");
    assert!(stdout.contains("| paired A-B |"), "{stdout}");
    // The budget verdict is in the output, not in the reader's memory.
    assert!(
        stdout.contains("PASS") || stdout.contains("OVER BUDGET"),
        "{stdout}"
    );
    // The embedded stub really served the client — this is the only place in
    // C2a where `http.rs` is exercised at volume.
    assert!(
        stdout.contains("in-process transport round trip"),
        "the transport probe did not complete: {stdout}"
    );
}

/// The null experiment. A = B = `hook noop`, so the paired delta must sit at
/// approximately zero; a harness that cannot measure "no difference" cannot be
/// trusted to measure one.
///
/// The bound is deliberately loose (0.5 ms at N=20 on a possibly-loaded CI
/// box). It is not a latency assertion — it catches a harness that has started
/// charging one arm for something the other does not pay, such as a warm-up
/// that only runs on A.
#[test]
fn identical_arms_produce_a_paired_delta_near_zero() {
    let out = Command::new(env!("CARGO_BIN_EXE_hook_bench"))
        .args([
            "--bin",
            env!("CARGO_BIN_EXE_memgarden"),
            "--n",
            "20",
            "--warmup",
            "5",
            "--transport-n",
            "11",
        ])
        .output()
        .expect("run hook_bench");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.starts_with("| paired A-B |"))
        .unwrap_or_else(|| panic!("no paired row in:\n{stdout}"));
    let p50: f64 = line
        .split('|')
        .nth(2)
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or_else(|| panic!("unparseable paired row: {line}"));
    assert!(
        p50.abs() < 0.5,
        "identical arms differ by {p50:.3} ms p50: {line}"
    );
}
