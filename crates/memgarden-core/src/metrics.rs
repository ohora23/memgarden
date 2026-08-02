//! Lock-free metrics registry: a single `const`-initialized static of
//! `AtomicU64` counters and fixed-bucket histograms. No lazy_static /
//! once_cell / locks / heap allocation on the record path — a
//! metrics/prometheus/hdrhistogram crate's registry lookup and locking is
//! 10-100x the cost of a relaxed `fetch_add` (see 핵심 결정, PR 4).
//!
//! Quantiles are never computed on the hot path — only in `snapshot()`,
//! which is called at most once per `/metrics.json` request or metrics
//! snapshot tick.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Histogram bucket upper bounds, in microseconds. Fixed at 20 buckets.
/// Index 10 (35_000) and index 15 (60_000) are exact matches for the AC-2
/// SLO boundaries (35ms / 60ms), so `under_35ms` / `under_60ms` in the
/// snapshot are exact counts, not interpolated estimates. The last bucket
/// (`u64::MAX`) is the overflow catch-all.
const BOUNDS_US: [u64; 20] = [
    100,
    500,
    1_000,
    2_000,
    5_000,
    10_000,
    15_000,
    20_000,
    25_000,
    30_000,
    35_000,
    40_000,
    45_000,
    50_000,
    55_000,
    60_000,
    100_000,
    250_000,
    500_000,
    u64::MAX,
];

/// A fixed-bucket latency histogram. `record_us` is a bucket-index scan
/// plus 4 relaxed atomic RMWs: no lock, no allocation, no compare-exchange
/// retry loop.
pub struct Histogram {
    buckets: [AtomicU64; BOUNDS_US.len()],
    count: AtomicU64,
    sum_us: AtomicU64,
    max_us: AtomicU64,
}

impl Histogram {
    const fn new() -> Self {
        Histogram {
            buckets: [const { AtomicU64::new(0) }; BOUNDS_US.len()],
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }

    /// Records one observation, in microseconds. Values above the last
    /// finite bound land in the overflow bucket.
    pub fn record_us(&self, us: u64) {
        let idx = BOUNDS_US
            .iter()
            .position(|&bound| us <= bound)
            .unwrap_or(BOUNDS_US.len() - 1);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// `None` for an untouched histogram (serializes to JSON `null`, never
    /// `NaN`). Quantiles are linear interpolation within the bucket a
    /// target rank falls into, using the previous bucket's upper bound (or
    /// 0) as the lower edge.
    fn snapshot(&self) -> Option<HistogramSnapshot> {
        let count = self.count.load(Ordering::Relaxed);
        if count == 0 {
            return None;
        }
        let sum_us = self.sum_us.load(Ordering::Relaxed);
        let max_us = self.max_us.load(Ordering::Relaxed);

        let mut cumulative = [0u64; BOUNDS_US.len()];
        let mut running = 0u64;
        for (i, bucket) in self.buckets.iter().enumerate() {
            running += bucket.load(Ordering::Relaxed);
            cumulative[i] = running;
        }

        let quantile = |q: f64| -> f64 {
            let target = ((q * count as f64).ceil() as u64).clamp(1, count);
            let mut prev_bound = 0u64;
            let mut prev_cum = 0u64;
            for (i, &cum) in cumulative.iter().enumerate() {
                if cum >= target {
                    let bound = BOUNDS_US[i];
                    let bucket_count = cum - prev_cum;
                    if bucket_count == 0 {
                        return (prev_bound as f64).min(max_us as f64);
                    }
                    let frac = (target - prev_cum) as f64 / bucket_count as f64;
                    let value = prev_bound as f64 + frac * (bound as f64 - prev_bound as f64);
                    // Clamp: a sample landing in the overflow bucket
                    // (bound == u64::MAX) can interpolate toward u64::MAX
                    // even though the true max observed is far lower.
                    return value.min(max_us as f64);
                }
                prev_bound = BOUNDS_US[i];
                prev_cum = cum;
            }
            (prev_bound as f64).min(max_us as f64)
        };

        let under_bound = |bound_us: u64| -> u64 {
            BOUNDS_US
                .iter()
                .position(|&b| b == bound_us)
                .map(|idx| cumulative[idx])
                .unwrap_or(0)
        };

        Some(HistogramSnapshot {
            count,
            mean_us: sum_us as f64 / count as f64,
            max_us,
            p50_us: quantile(0.50),
            p90_us: quantile(0.90),
            p95_us: quantile(0.95),
            p99_us: quantile(0.99),
            under_35ms: under_bound(35_000),
            under_60ms: under_bound(60_000),
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub mean_us: f64,
    pub max_us: u64,
    pub p50_us: f64,
    pub p90_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub under_35ms: u64,
    pub under_60ms: u64,
}

/// Lock-free counter + histogram registry. Every field is directly
/// accessible (`METRICS.http_requests.fetch_add(1, Ordering::Relaxed)`) —
/// no wrapper methods, since a getter/setter per counter is boilerplate
/// this registry is explicitly trying to avoid.
pub struct Metrics {
    pub started_at_ms: AtomicU64,
    pub http_requests: AtomicU64,
    pub http_errors: AtomicU64,
    pub recall_requests: AtomicU64,
    pub recall_errors: AtomicU64,
    pub retain_requests: AtomicU64,
    pub retain_errors: AtomicU64,
    pub recall_injected_tokens: AtomicU64,
    pub recall_injected_memories: AtomicU64,
    pub nodes_written: AtomicU64,
    pub links_written: AtomicU64,
    pub retain_tokens_raw: AtomicU64,
    pub retain_tokens_capped: AtomicU64,
    /// Chunks whose LLM extraction failed but did not fail the whole job
    /// (Critic Revision R14).
    pub retain_chunks_failed: AtomicU64,
    /// `benefit_ledger` rows of kind `retain_cap_saving` written by the
    /// retain ingest (AC-6: the ledger auto-populates, MX-1's deferral).
    pub retain_cap_savings: AtomicU64,
    pub hook_invocations: AtomicU64,
    pub http_latency: Histogram,
    pub recall_latency: Histogram,
    pub retain_latency: Histogram,
}

impl Metrics {
    const fn new() -> Self {
        Metrics {
            started_at_ms: AtomicU64::new(0),
            http_requests: AtomicU64::new(0),
            http_errors: AtomicU64::new(0),
            recall_requests: AtomicU64::new(0),
            recall_errors: AtomicU64::new(0),
            retain_requests: AtomicU64::new(0),
            retain_errors: AtomicU64::new(0),
            recall_injected_tokens: AtomicU64::new(0),
            recall_injected_memories: AtomicU64::new(0),
            nodes_written: AtomicU64::new(0),
            links_written: AtomicU64::new(0),
            retain_tokens_raw: AtomicU64::new(0),
            retain_tokens_capped: AtomicU64::new(0),
            retain_chunks_failed: AtomicU64::new(0),
            retain_cap_savings: AtomicU64::new(0),
            hook_invocations: AtomicU64::new(0),
            http_latency: Histogram::new(),
            recall_latency: Histogram::new(),
            retain_latency: Histogram::new(),
        }
    }

    /// A serde-serializable point-in-time snapshot, incl. derived
    /// `retain_tokens_saved` / `retain_saving_ratio` (both `None` until
    /// both `retain_tokens_raw` and `retain_tokens_capped` are nonzero —
    /// there's nothing to derive from a raw total with no capped total
    /// yet, or vice versa).
    pub fn snapshot(&self) -> MetricsSnapshot {
        let retain_tokens_raw = self.retain_tokens_raw.load(Ordering::Relaxed);
        let retain_tokens_capped = self.retain_tokens_capped.load(Ordering::Relaxed);
        let (retain_tokens_saved, retain_saving_ratio) =
            if retain_tokens_raw > 0 && retain_tokens_capped > 0 {
                (
                    Some(retain_tokens_raw.saturating_sub(retain_tokens_capped)),
                    Some(1.0 - (retain_tokens_capped as f64 / retain_tokens_raw as f64)),
                )
            } else {
                (None, None)
            };

        MetricsSnapshot {
            started_at_ms: self.started_at_ms.load(Ordering::Relaxed) as i64,
            http_requests: self.http_requests.load(Ordering::Relaxed),
            http_errors: self.http_errors.load(Ordering::Relaxed),
            recall_requests: self.recall_requests.load(Ordering::Relaxed),
            recall_errors: self.recall_errors.load(Ordering::Relaxed),
            retain_requests: self.retain_requests.load(Ordering::Relaxed),
            retain_errors: self.retain_errors.load(Ordering::Relaxed),
            recall_injected_tokens: self.recall_injected_tokens.load(Ordering::Relaxed),
            recall_injected_memories: self.recall_injected_memories.load(Ordering::Relaxed),
            nodes_written: self.nodes_written.load(Ordering::Relaxed),
            links_written: self.links_written.load(Ordering::Relaxed),
            retain_tokens_raw,
            retain_tokens_capped,
            retain_chunks_failed: self.retain_chunks_failed.load(Ordering::Relaxed),
            retain_cap_savings: self.retain_cap_savings.load(Ordering::Relaxed),
            hook_invocations: self.hook_invocations.load(Ordering::Relaxed),
            retain_tokens_saved,
            retain_saving_ratio,
            http_latency: self.http_latency.snapshot(),
            recall_latency: self.recall_latency.snapshot(),
            retain_latency: self.retain_latency.snapshot(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    pub started_at_ms: i64,
    pub http_requests: u64,
    pub http_errors: u64,
    pub recall_requests: u64,
    pub recall_errors: u64,
    pub retain_requests: u64,
    pub retain_errors: u64,
    pub recall_injected_tokens: u64,
    pub recall_injected_memories: u64,
    pub nodes_written: u64,
    pub links_written: u64,
    pub retain_tokens_raw: u64,
    pub retain_tokens_capped: u64,
    pub retain_chunks_failed: u64,
    pub retain_cap_savings: u64,
    pub hook_invocations: u64,
    pub retain_tokens_saved: Option<u64>,
    pub retain_saving_ratio: Option<f64>,
    pub http_latency: Option<HistogramSnapshot>,
    pub recall_latency: Option<HistogramSnapshot>,
    pub retain_latency: Option<HistogramSnapshot>,
}

/// The process-wide metrics registry. `const`-initialized: no lazy init,
/// no allocation, safe to touch from the very first request.
pub static METRICS: Metrics = Metrics::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_monotonic() {
        // A fresh local instance, not the global METRICS static: unit
        // tests in this binary run in parallel and share the global, so
        // asserting an exact total against it would be flaky.
        let metrics = Metrics::new();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..100_000 {
                        metrics.hook_invocations.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(metrics.hook_invocations.load(Ordering::Relaxed), 800_000);
    }

    #[test]
    fn histogram_quantiles() {
        let h = Histogram::new();
        for _ in 0..50 {
            h.record_us(1_000);
        }
        for _ in 0..50 {
            h.record_us(5_000);
        }
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.count, 100);
        assert_eq!(snap.mean_us, 3_000.0);
        assert_eq!(snap.max_us, 5_000);
        assert_eq!(snap.p50_us, 1_000.0);
        assert_eq!(snap.p90_us, 4_400.0);
        assert_eq!(snap.p95_us, 4_700.0);
        assert_eq!(snap.p99_us, 4_940.0);
        assert_eq!(snap.under_35ms, 100);
        assert_eq!(snap.under_60ms, 100);
    }

    #[test]
    fn histogram_edges() {
        let h = Histogram::new();
        h.record_us(0);
        h.record_us(35_000); // exact SLO boundary: must count toward under_35ms
        h.record_us(u64::MAX); // overflow bucket
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.count, 3);
        assert_eq!(snap.max_us, u64::MAX);
        assert_eq!(
            snap.under_35ms, 2,
            "boundary value 35_000 must be <= bound, inclusive"
        );
        assert_eq!(snap.under_60ms, 2);
    }

    #[test]
    fn quantile_clamped_to_max() {
        // One sample lands in the overflow bucket (bound == u64::MAX) but
        // is nowhere near u64::MAX itself. Without clamping, interpolation
        // between the previous bucket's bound and u64::MAX would blow p99
        // up to ~1.8e19 even though the real max is 700_000.
        let h = Histogram::new();
        h.record_us(1_000);
        h.record_us(700_000);
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.max_us, 700_000);
        assert!(
            snap.p99_us <= snap.max_us as f64,
            "p99 {} must not exceed max_us {}",
            snap.p99_us,
            snap.max_us
        );
    }

    #[test]
    fn empty_histogram_is_none() {
        let h = Histogram::new();
        assert!(h.snapshot().is_none());
    }

    #[test]
    fn snapshot_serializes() {
        let metrics = Metrics::new();
        metrics.http_requests.fetch_add(5, Ordering::Relaxed);
        metrics.http_latency.record_us(1_234);

        let json = serde_json::to_value(metrics.snapshot()).unwrap();
        assert_eq!(json["http_requests"], 5);
        // Untouched histograms serialize to null, never NaN.
        assert!(json["recall_latency"].is_null());
        assert!(json["retain_latency"].is_null());
        assert!(json["http_latency"].is_object());
        // retain_tokens_saved/ratio stay null until both raw and capped
        // are nonzero.
        assert!(json["retain_tokens_saved"].is_null());
        assert!(json["retain_saving_ratio"].is_null());

        metrics.retain_tokens_raw.store(400, Ordering::Relaxed);
        metrics.retain_tokens_capped.store(100, Ordering::Relaxed);
        let json = serde_json::to_value(metrics.snapshot()).unwrap();
        assert_eq!(json["retain_tokens_saved"], 300);
        assert_eq!(json["retain_saving_ratio"], 0.75);
    }
}
