//! AC-6 evidence: metrics recording must be effectively free on the hot
//! path. Run in release mode for a realistic number:
//!   cargo test --release -p memgarden-core --test metrics_overhead -- --nocapture

use std::sync::atomic::Ordering;
use std::time::Instant;

use memgarden_core::metrics::METRICS;

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
    println!("Measured: record_us = {ns_per_op:.2} ns/op (target < 100)");
    assert!(
        ns_per_op < 100.0,
        "metrics recording overhead too high: {ns_per_op:.2} ns/op (target < 100)"
    );
}
