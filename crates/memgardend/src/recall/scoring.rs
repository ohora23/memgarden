//! Combined scoring, ported from `engine/search/reranking.py:10-196`.
//!
//! MemGarden ships no cross-encoder, so the production path is legacy's
//! *passthrough* path: the base relevance signal is derived from the RRF
//! rank and the three multiplicative boosts modulate it.

/// `reranking.py:15-17`.
pub const RECENCY_ALPHA: f64 = 0.2;
pub const TEMPORAL_ALPHA: f64 = 0.2;
pub const PROOF_COUNT_ALPHA: f64 = 0.1;

/// Linear decay window, `reranking.py:32` (`_RECENCY_DECAY_LINEAR_WINDOW_DAYS`).
pub const RECENCY_WINDOW_DAYS: f64 = 365.0;

/// Neutral value for a signal that is not available: it makes the
/// corresponding boost exactly 1.0.
pub const NEUTRAL: f64 = 0.5;

const MS_PER_DAY: f64 = 86_400_000.0;

/// Base relevance for the passthrough reranker (`reranking.py:134-145`):
/// maps a 0-based rank within `n` results onto `[0.1, 1.0]`. `denom` is
/// guarded at 1 so a single result scores 1.0 instead of dividing by zero.
pub fn passthrough_base(n: usize, rank0: usize) -> f64 {
    let denom = std::cmp::max(1, n.saturating_sub(1)) as f64;
    1.0 - (0.9 * rank0 as f64 / denom)
}

/// `occurred_start ?? mentioned_at ?? occurred_end` (`reranking.py:156`) —
/// the same COALESCE order retrieval uses, so a fact carrying only a
/// `mentioned_at` still gets real recency instead of a flat neutral.
pub fn effective_time(
    occurred_start: Option<i64>,
    mentioned_at: Option<i64>,
    occurred_end: Option<i64>,
) -> Option<i64> {
    occurred_start.or(mentioned_at).or(occurred_end)
}

/// Linear age -> freshness in `[0.1, 1.0]` (`reranking.py:54`). Future dates
/// (negative `days_ago`) clamp to 1.0 rather than being penalised.
pub fn recency_decay(days_ago: f64) -> f64 {
    (1.0 - days_ago / RECENCY_WINDOW_DAYS).clamp(0.1, 1.0)
}

/// Recency for a node at `now_ms`; `NEUTRAL` when it carries no date at all
/// (`reranking.py:153`).
pub fn recency(effective_ms: Option<i64>, now_ms: i64) -> f64 {
    match effective_ms {
        Some(ms) => recency_decay((now_ms - ms) as f64 / MS_PER_DAY),
        None => NEUTRAL,
    }
}

/// `combined = base * recency_boost * temporal_boost * proof_boost`
/// (`reranking.py:190`). Each boost is `1 + alpha * (signal - 0.5)`, so a
/// neutral signal contributes exactly 1.0.
pub fn combined(base: f64, recency: f64, temporal: f64, proof_norm: f64) -> f64 {
    base * (1.0 + RECENCY_ALPHA * (recency - NEUTRAL))
        * (1.0 + TEMPORAL_ALPHA * (temporal - NEUTRAL))
        * (1.0 + PROOF_COUNT_ALPHA * (proof_norm - NEUTRAL))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: i64 = 86_400_000;
    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn passthrough_base_endpoints() {
        // n = 1: the denominator guard, not a divide-by-zero.
        assert_eq!(passthrough_base(1, 0), 1.0);
        // n = 2: full [1.0, 0.1] span.
        assert_eq!(passthrough_base(2, 0), 1.0);
        assert!((passthrough_base(2, 1) - 0.1).abs() < 1e-12);
        // n = 0 must not panic either (saturating_sub).
        assert_eq!(passthrough_base(0, 0), 1.0);
        // Midpoint of an odd-sized list.
        assert!((passthrough_base(11, 5) - 0.55).abs() < 1e-12);
    }

    #[test]
    fn recency_over_the_window() {
        assert_eq!(recency(Some(NOW), NOW), 1.0);
        assert!((recency(Some(NOW - 182 * DAY_MS), NOW) - (1.0 - 182.0 / 365.0)).abs() < 1e-12);
        // Exactly at the window the linear curve hits 0, which the floor
        // lifts to 0.1.
        assert_eq!(recency(Some(NOW - 365 * DAY_MS), NOW), 0.1);
        assert_eq!(recency(Some(NOW - 400 * DAY_MS), NOW), 0.1);
        // Future date clamps to the maximum, never a penalty.
        assert_eq!(recency(Some(NOW + 30 * DAY_MS), NOW), 1.0);
        // No date at all is neutral, which is NOT the same as "very old".
        assert_eq!(recency(None, NOW), NEUTRAL);
    }

    #[test]
    fn effective_time_coalesce_order() {
        assert_eq!(effective_time(Some(1), Some(2), Some(3)), Some(1));
        assert_eq!(effective_time(None, Some(2), Some(3)), Some(2));
        assert_eq!(effective_time(None, None, Some(3)), Some(3));
        assert_eq!(effective_time(None, None, None), None);
    }

    #[test]
    fn combined_is_neutral_when_every_signal_is() {
        assert_eq!(combined(0.7, NEUTRAL, NEUTRAL, NEUTRAL), 0.7);
        // Documented envelope: max ≈ +21%, min ≈ -19% over the base.
        let hi = combined(1.0, 1.0, 1.0, 1.0);
        let lo = combined(1.0, 0.0, 0.0, 0.0);
        assert!((hi - 1.1 * 1.1 * 1.05).abs() < 1e-12, "{hi}");
        assert!((lo - 0.9 * 0.9 * 0.95).abs() < 1e-12, "{lo}");
    }
}
