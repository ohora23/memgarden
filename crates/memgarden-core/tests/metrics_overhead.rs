//! AC-6 evidence: metrics recording must be effectively free on the hot
//! path. Run in release mode for a realistic number:
//!   cargo test --release -p memgarden-core --test metrics_overhead -- --nocapture

use std::sync::atomic::Ordering;
use std::time::Instant;

use memgarden_core::metrics::METRICS;

/// The assert is a rot detector, not the AC-6 figure. A debug build on a
/// shared CI runner measures 2-5x the release number this gate protects
/// (139 ns observed in CI vs 74-89 ns locally in release), so asserting the
/// real target here would fail on hardware speed rather than on a
/// regression. What must never happen is a mutex, an allocation, or a
/// syscall entering the metrics path — those are microsecond-scale and both
/// ceilings catch them. The quotable AC-6 numbers come from a release run.
const CEILING_NS: f64 = if cfg!(debug_assertions) {
    1_000.0
} else {
    200.0
};

#[test]
fn record_us_overhead() {
    const ITERS: u64 = 1_000_000;

    let start = Instant::now();
    for i in 0..ITERS {
        METRICS.http_requests.fetch_add(1, Ordering::Relaxed);
        METRICS.http_latency.record_us(i % 10_000);
    }
    let elapsed = start.elapsed();

    let ns_per_op = elapsed.as_nanos() as f64 / ITERS as f64;
    println!("Measured: record_us = {ns_per_op:.2} ns/op (MX-1 target < 100 in release)");
    assert!(
        ns_per_op < CEILING_NS,
        "metrics recording overhead too high: {ns_per_op:.2} ns/op (ceiling {CEILING_NS})"
    );
}

/// AC-6 closure for the recall path (plan §PR B4 + Critic Revision R8).
///
/// R8 dropped the planned `metrics-off` cargo feature: Phase A deliberately
/// removed the wrapper layer that `#[cfg]` would have hung off, and a
/// wall-clock A/B of a ~7ms request against a sub-microsecond difference is
/// statistically meaningless — the run-to-run noise is three orders of
/// magnitude larger than the effect. What *is* measurable is the cost of the
/// recording sequence itself, timed here in isolation. "Metrics off" is
/// exactly zero of it, so this number IS the delta.
///
/// The sequence below is every metrics site one `POST /recall` touches:
///   - `track_http` middleware: `http_requests` + `http_latency`
///   - `routes::recall`:        `recall_requests` + `recall_latency`
///   - `recall::recall`:        `recall_injected_tokens` + `recall_injected_memories`
///
/// `recall_errors` fires only on the error path and is not counted here.
///
///   cargo test --release -p memgarden-core --test metrics_overhead -- --nocapture
#[test]
fn recall_path_metrics_overhead() {
    const ITERS: u64 = 1_000_000;

    let start = Instant::now();
    for i in 0..ITERS {
        METRICS.http_requests.fetch_add(1, Ordering::Relaxed);
        METRICS.recall_requests.fetch_add(1, Ordering::Relaxed);
        METRICS
            .recall_injected_tokens
            .fetch_add(120, Ordering::Relaxed);
        METRICS
            .recall_injected_memories
            .fetch_add(6, Ordering::Relaxed);
        METRICS.recall_latency.record_us(i % 10_000);
        METRICS.http_latency.record_us(i % 10_000);
    }
    let elapsed = start.elapsed();

    let ns_per_recall = elapsed.as_nanos() as f64 / ITERS as f64;
    // AC-2's p50 gate. The share of it that metrics can possibly consume:
    let share = ns_per_recall / 35_000_000.0 * 100.0;
    println!(
        "Measured: full recall-path metrics sequence = {ns_per_recall:.1} ns/request \
         ({share:.6}% of the 35ms p50 SLO)"
    );
    // Six sites vs one, so scale the same rot-detection ceiling.
    let ceiling = CEILING_NS * 6.0;
    assert!(
        ns_per_recall < ceiling,
        "recall-path metrics overhead {ns_per_recall:.1} ns/request is no longer negligible \
         (ceiling {ceiling})"
    );
}
