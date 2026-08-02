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

/// The date a node is measured *against a temporal constraint* by
/// (`retrieval.py:686-693`): the midpoint of a known interval, else whichever
/// single endpoint exists, else the mention.
///
/// This is the **third** COALESCE order in the codebase and it is not a
/// mistake that it differs from the other two:
///
/// | order | where | why |
/// |---|---|---|
/// | `occurred_start ?? mentioned_at ?? occurred_end` | [`effective_time`] (recency, `reranking.py:156`) | recency wants when we *learned* it before an inferred end date |
/// | `occurred_start ?? mentioned_at` | the temporal arm's SQL (`search::temporal_candidates`) | a two-branch COALESCE is indexable-adjacent and matches the plan's entry predicate |
/// | `midpoint ?? occurred_start ?? occurred_end ?? mentioned_at` | here | proximity to a *window* wants the centre of the interval, and prefers any real occurrence over a mention |
///
/// Unifying them would change ranking for exactly the nodes that carry
/// partial dates — the ones the temporal arm exists for. `the_three_coalesce_
/// orders_are_deliberately_different` pins the divergence.
pub fn temporal_best_time(
    occurred_start: Option<i64>,
    occurred_end: Option<i64>,
    mentioned_at: Option<i64>,
) -> Option<i64> {
    match (occurred_start, occurred_end) {
        (Some(s), Some(e)) => Some(s + (e - s) / 2),
        (Some(s), None) => Some(s),
        (None, Some(e)) => Some(e),
        (None, None) => mentioned_at,
    }
}

/// How close a node sits to the centre of the query's constraint window
/// (`retrieval.py:696-702`): 1.0 at the midpoint, 0.0 at either edge and
/// beyond, `NEUTRAL` for a node carrying no date at all.
///
/// A zero-width window (a single instant) is 1.0 for anything dated, matching
/// legacy's `if total_days > 0 else 1.0`.
pub fn temporal_proximity(best_time: Option<i64>, start_ms: i64, end_ms: i64) -> f64 {
    let Some(best) = best_time else {
        return NEUTRAL;
    };
    let total_ms = (end_ms - start_ms) as f64;
    if total_ms <= 0.0 {
        return 1.0;
    }
    let mid = start_ms + (end_ms - start_ms) / 2;
    let from_mid = (best - mid).abs() as f64;
    1.0 - (from_mid / (total_ms / 2.0)).min(1.0)
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
    fn temporal_proximity_over_the_window() {
        let (start, end) = (NOW, NOW + 10 * DAY_MS);
        let mid = NOW + 5 * DAY_MS;
        // 1.0 dead centre, 0.0 at both edges, 0.5 half way out.
        assert_eq!(temporal_proximity(Some(mid), start, end), 1.0);
        assert_eq!(temporal_proximity(Some(start), start, end), 0.0);
        assert_eq!(temporal_proximity(Some(end), start, end), 0.0);
        assert!(
            (temporal_proximity(Some(mid - 2 * DAY_MS + DAY_MS / 2), start, end) - 0.7).abs()
                < 1e-12
        );
        assert_eq!(
            temporal_proximity(Some(mid + 2 * DAY_MS + DAY_MS / 2), start, end),
            0.5
        );
        // Outside the window clamps at 0, never negative.
        assert_eq!(
            temporal_proximity(Some(start - 400 * DAY_MS), start, end),
            0.0
        );
        // A dateless node is neutral — NOT "maximally far away".
        assert_eq!(temporal_proximity(None, start, end), NEUTRAL);
        // Zero-width window: anything dated is 1.0 (`total_days > 0` guard).
        assert_eq!(temporal_proximity(Some(NOW), NOW, NOW), 1.0);
    }

    /// The boosts at the three named proximities. `combined` multiplies by
    /// `1 + 0.2 * (temporal - 0.5)`, so 0.0 -> 0.9x, 0.5 -> 1.0x, 1.0 -> 1.1x.
    #[test]
    fn temporal_boost_at_zero_half_and_one() {
        let base = 0.8;
        for (temporal, want) in [(0.0, 0.9), (0.5, 1.0), (1.0, 1.1)] {
            let got = combined(base, NEUTRAL, temporal, NEUTRAL);
            assert!(
                (got - base * want).abs() < 1e-12,
                "temporal={temporal} -> {got}"
            );
        }
    }

    /// The three COALESCE orders must coexist. Named explicitly, because the
    /// obvious "cleanup" is to collapse them into one helper — and the inputs
    /// below are exactly the partial-date shapes where that would change
    /// ranking.
    #[test]
    fn the_three_coalesce_orders_are_deliberately_different() {
        // A node with an interval AND a mention: all three disagree.
        let (start, end, mentioned) = (Some(1_000), Some(3_000), Some(9_000));

        // 1. recency (`effective_time`, reranking.py:156):
        //    occurred_start ?? mentioned_at ?? occurred_end
        assert_eq!(effective_time(start, mentioned, end), Some(1_000));

        // 2. the temporal arm's entry predicate (SQL in
        //    `memgarden_store::search::temporal_candidates`, pinned by its own
        //    test there): occurred_start ?? mentioned_at
        let arm_order = start.or(mentioned);
        assert_eq!(arm_order, Some(1_000));

        // 3. temporal proximity (`temporal_best_time`, retrieval.py:686-693):
        //    midpoint ?? occurred_start ?? occurred_end ?? mentioned_at
        assert_eq!(temporal_best_time(start, end, mentioned), Some(2_000));

        // The end-only node is where 1 and 3 part company: recency reaches
        // for the mention, proximity reaches for the occurrence.
        assert_eq!(effective_time(None, mentioned, end), Some(9_000));
        assert_eq!(temporal_best_time(None, end, mentioned), Some(3_000));
        // ...and where 2 parts company with 3: the arm cannot see occurred_end
        // at all, so it falls through to the mention.
        assert_eq!(None.or(mentioned), Some(9_000));

        // With only a start, all three agree — the divergence is specific to
        // partial dates, which is why one helper "looks" sufficient.
        assert_eq!(effective_time(start, None, None), Some(1_000));
        assert_eq!(temporal_best_time(start, None, None), Some(1_000));
        assert_eq!(start.or(None), Some(1_000));
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
