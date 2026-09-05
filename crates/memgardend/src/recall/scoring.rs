//! Combined scoring, ported from `engine/search/reranking.py:10-196`.
//!
//! The production path is legacy's *passthrough* path: the base relevance
//! signal is derived from the RRF rank and the three multiplicative boosts
//! modulate it. CE-11 ships an embedded cross-encoder that replaces
//! [`passthrough_base`] with a sigmoid-normalized logit, but it is off by
//! default — which is parity, since the live legacy daemon runs
//! `RERANKER_PROVIDER=rrf`. The boosts below are identical either way.

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
/// (`retrieval.py:698-699`): 1.0 at the midpoint, 0.0 at either edge and
/// beyond, `NEUTRAL` for a node carrying no date at all (`:701`).
///
/// A zero-width window (a single instant) is 1.0 for anything dated, matching
/// legacy's `if total_days > 0 else 1.0`. An **inverted** window is not: it
/// is a bug upstream, and legacy cannot produce one (`since_constraint`
/// returns the sentinel rather than a backwards range,
/// `chinese_temporal_periods.py:451-454`, which `extract_constraint` ports).
/// Returning 1.0 there would hand every dated candidate a uniform +10% over
/// every dateless one on a query where the arm contributed nothing — so the
/// zero-width shortcut is spelled `start == end`, and anything backwards is
/// neutral. Defence in depth: the guard upstream is the fix, this is the
/// blast radius if it ever regresses.
pub fn temporal_proximity(best_time: Option<i64>, start_ms: i64, end_ms: i64) -> f64 {
    let Some(best) = best_time else {
        return NEUTRAL;
    };
    if start_ms > end_ms {
        return NEUTRAL;
    }
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

/// Evidence strength for an observation, log-normalised (`reranking.py:173-176`).
///
/// `min(1, max(0, 0.5 + ln(proof_count)/10))`. One source is `ln(1)/10 = 0`
/// above the neutral baseline, i.e. **exactly** `NEUTRAL` — an observation
/// backed by a single fact must not out-rank a plain fact. The clamp bites at
/// `proof_count = e^5 ≈ 149`, which is what keeps the boost inside its
/// documented ±5% band.
///
/// `proof_count = 0` is "not a sourced observation" (legacy's `is not None
/// and >= 1`, `:174`) and returns `NEUTRAL`, so every world/experience fact —
/// where the column is 0 by DDL default — keeps a 1.0 multiplier. CE-9b's
/// batch round is what gives observations counts above 1.
pub fn proof_norm(proof_count: i64) -> f64 {
    if proof_count < 1 {
        return NEUTRAL;
    }
    (0.5 + (proof_count as f64).ln() / 10.0).clamp(0.0, 1.0)
}

/// `combined = base * recency_boost * temporal_boost * proof_boost`
/// (`reranking.py:190`). Each boost is `1 + alpha * (signal - 0.5)`, so a
/// neutral signal contributes exactly 1.0.
///
/// `proof_alpha` is a parameter rather than the constant because
/// `proof_count` is the one signal recall *writes back*: consolidation pools
/// existing observations by recall, the LLM UPDATEs what it was shown, and
/// the UPDATE grows `proof_count`. With the boost on inside that pooling
/// recall the loop has no damping — the first live round's ten-source
/// observation dissolving into "Multiple components…" is the recorded
/// failure. The pooling site passes `0.0`; the injection keeps
/// [`PROOF_COUNT_ALPHA`].
pub fn combined(base: f64, recency: f64, temporal: f64, proof_norm: f64, proof_alpha: f64) -> f64 {
    base * (1.0 + RECENCY_ALPHA * (recency - NEUTRAL))
        * (1.0 + TEMPORAL_ALPHA * (temporal - NEUTRAL))
        * (1.0 + proof_alpha * (proof_norm - NEUTRAL))
}

/// Where a candidate's cosine sits inside the spread this query actually
/// produced, on `[0, 1]`. `None` — a candidate the semantic arm never scored —
/// is [`NEUTRAL`], which makes the boost exactly 1.0.
///
/// **Normalised per query, and it has to be.** Raw cosine over a real bank
/// occupies a narrow high band: measured across four live queries the spread
/// was 0.63-0.94, so feeding it in raw would be a near-constant multiplier —
/// the same way [`recency`] is inert against a two-week bank inside a 365-day
/// window. What carries information is a candidate's position *within this
/// query's* range, and min-max is the cheapest statement of that.
///
/// A degenerate range (every candidate identical, or one candidate) returns
/// NEUTRAL rather than dividing by zero, so the boost stays off exactly when
/// there is nothing to say.
pub fn semantic_norm(semantic: Option<f64>, lo: f64, hi: f64) -> f64 {
    let Some(sem) = semantic else { return NEUTRAL };
    let span = hi - lo;
    if span <= f64::EPSILON {
        return NEUTRAL;
    }
    ((sem - lo) / span).clamp(0.0, 1.0)
}

/// [`combined`] with the semantic boost applied on top. `alpha` of `0.0` is
/// exactly [`combined`], which is how it ships until a measurement says
/// otherwise.
///
/// **A deliberate divergence from legacy**, unlike everything else in this
/// module. Legacy fuses the arms by RRF and then scores on rank alone, so the
/// cosine — the one retrieval signal with real spread — reaches the final
/// order only as an ordinal, and a keyword arm matching a command log
/// literally carries the same weight as a semantic match on the answer.
/// Measured on the four queries the blind panel scored as losses: every one of
/// the twelve relevant items MemGarden retrieved but ranked below its cut on
/// `@agentmemory/mcp`, and all three on `hindsight`, hold a *higher* cosine
/// than the worst item that was injected instead.
pub fn combined_with_semantic(
    base: f64,
    recency: f64,
    temporal: f64,
    proof_norm: f64,
    proof_alpha: f64,
    semantic: f64,
    alpha: f64,
) -> f64 {
    combined(base, recency, temporal, proof_norm, proof_alpha)
        * (1.0 + alpha * (semantic - NEUTRAL))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: i64 = 86_400_000;
    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn semantic_norm_places_a_score_inside_the_span_this_query_produced() {
        // The live spread is narrow and high, which is the whole reason for
        // normalising: 0.72 is the floor of this query's range, not a bad
        // score in absolute terms.
        assert_eq!(semantic_norm(Some(0.72), 0.72, 0.94), 0.0);
        assert_eq!(semantic_norm(Some(0.94), 0.72, 0.94), 1.0);
        assert!((semantic_norm(Some(0.83), 0.72, 0.94) - 0.5).abs() < 1e-12);
        // A candidate the semantic arm never scored is neutral, not zero:
        // keyword-only hits must not be pushed down for lacking a cosine.
        assert_eq!(semantic_norm(None, 0.72, 0.94), NEUTRAL);
        // Degenerate spans say nothing rather than dividing by zero. The
        // `lo > hi` case is what a candidate set with no semantic arm leaves
        // behind, since the fold starts at (MAX, MIN).
        assert_eq!(semantic_norm(Some(0.8), 0.8, 0.8), NEUTRAL);
        assert_eq!(semantic_norm(Some(0.8), f64::MAX, f64::MIN), NEUTRAL);
        // Outside the observed span cannot escape the unit interval.
        assert_eq!(semantic_norm(Some(1.5), 0.72, 0.94), 1.0);
        assert_eq!(semantic_norm(Some(0.1), 0.72, 0.94), 0.0);
    }

    /// `alpha = 0.0` has to be `combined` to the last bit, because that is the
    /// ledger's whole baseline arm and every row written before this term
    /// existed is a `0.0` row.
    #[test]
    fn a_zero_alpha_is_legacy_scoring_exactly() {
        for &(b, r, t, p, sem) in &[
            (1.0, 0.9, 0.5, 0.5, 1.0),
            (0.55, 0.1, 0.0, 1.0, 0.0),
            (0.31, 0.5, 0.5, 0.5, 0.5),
        ] {
            assert_eq!(
                combined_with_semantic(b, r, t, p, PROOF_COUNT_ALPHA, sem, 0.0).to_bits(),
                combined(b, r, t, p, PROOF_COUNT_ALPHA).to_bits()
            );
        }
    }

    #[test]
    fn the_semantic_boost_is_symmetric_around_neutral() {
        let base = combined(1.0, NEUTRAL, NEUTRAL, NEUTRAL, PROOF_COUNT_ALPHA);
        // NEUTRAL is exactly no boost, whatever alpha says.
        assert_eq!(
            combined_with_semantic(
                1.0,
                NEUTRAL,
                NEUTRAL,
                NEUTRAL,
                PROOF_COUNT_ALPHA,
                NEUTRAL,
                0.6
            )
            .to_bits(),
            base.to_bits()
        );
        let top =
            combined_with_semantic(1.0, NEUTRAL, NEUTRAL, NEUTRAL, PROOF_COUNT_ALPHA, 1.0, 0.6);
        let bottom =
            combined_with_semantic(1.0, NEUTRAL, NEUTRAL, NEUTRAL, PROOF_COUNT_ALPHA, 0.0, 0.6);
        assert!(top > base && base > bottom);
        assert!(((top - base) - (base - bottom)).abs() < 1e-12);
    }

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
        // Inverted window: neutral, NOT the zero-width 1.0. `extract_constraint`
        // cannot produce one; if it ever does, the damage is nothing rather
        // than a uniform boost for every dated candidate.
        assert_eq!(temporal_proximity(Some(NOW), end, start), NEUTRAL);
        assert_eq!(temporal_proximity(Some(NOW), NOW + 1, NOW), NEUTRAL);
    }

    /// The boosts at the three named proximities. `combined` multiplies by
    /// `1 + 0.2 * (temporal - 0.5)`, so 0.0 -> 0.9x, 0.5 -> 1.0x, 1.0 -> 1.1x.
    #[test]
    fn temporal_boost_at_zero_half_and_one() {
        let base = 0.8;
        for (temporal, want) in [(0.0, 0.9), (0.5, 1.0), (1.0, 1.1)] {
            let got = combined(base, NEUTRAL, temporal, NEUTRAL, PROOF_COUNT_ALPHA);
            assert!(
                (got - base * want).abs() < 1e-12,
                "temporal={temporal} -> {got}"
            );
        }
    }

    /// `proof_norm` at the counts the plan names, plus the edges.
    #[test]
    fn proof_norm_curve_and_clamp() {
        // One source is EXACTLY neutral: 0.5 + ln(1)/10 = 0.5.
        assert_eq!(proof_norm(1), 0.5);
        // Three: 0.5 + ln(3)/10 = 0.6098612...
        assert!((proof_norm(3) - (0.5 + 3f64.ln() / 10.0)).abs() < 1e-12);
        assert!((proof_norm(3) - 0.609_861_228_866_810_9).abs() < 1e-12);
        // 150 overshoots (0.5 + 5.0106/10 = 1.00106) and is clamped.
        assert_eq!(proof_norm(150), 1.0);
        // The clamp turns on just past e^5 ~= 148.41.
        assert!(proof_norm(148) < 1.0);
        assert_eq!(proof_norm(149), 1.0);
        // Not an observation (or an unsourced one): neutral, never a penalty.
        assert_eq!(proof_norm(0), NEUTRAL);
        assert_eq!(proof_norm(-7), NEUTRAL);
        // Monotone in between.
        assert!(proof_norm(2) > proof_norm(1) && proof_norm(20) > proof_norm(2));
    }

    /// `combined` multiplies by `1 + 0.1 * (proof_norm - 0.5)`: the whole
    /// proof signal is worth ±5%, and one source is worth exactly nothing.
    #[test]
    fn proof_boost_at_one_and_at_the_clamp() {
        let base = 0.8;
        let a = PROOF_COUNT_ALPHA;
        assert!((combined(base, NEUTRAL, NEUTRAL, proof_norm(1), a) - base).abs() < 1e-12);
        assert!(
            (combined(base, NEUTRAL, NEUTRAL, proof_norm(150), a) - base * 1.05).abs() < 1e-12,
            "the clamp caps the boost at +5%"
        );
    }

    /// `proof_alpha = 0.0` is the damped arm: the proof signal is inert at
    /// every count, and the other boosts are untouched. This is what the
    /// consolidation pooling recall runs with.
    #[test]
    fn a_zero_proof_alpha_makes_the_count_inert() {
        for count in [0, 1, 2, 26, 150] {
            assert_eq!(
                combined(0.8, 0.9, 0.2, proof_norm(count), 0.0).to_bits(),
                combined(0.8, 0.9, 0.2, NEUTRAL, 0.0).to_bits()
            );
        }
        assert!(combined(0.8, NEUTRAL, NEUTRAL, proof_norm(26), PROOF_COUNT_ALPHA) > 0.8);
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
        //    `memgarden_store::search::temporal_candidates`). This leg is
        //    written in Rust rather than executed: the SQL's own behaviour —
        //    the same order, plus boundaries, bank scoping and the
        //    `event_date` exclusion — is pinned by
        //    `search::tests::temporal_candidates_range_boundaries_and_coalesce_order`.
        //    Change one and that test fails, not this one.
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
        assert_eq!(
            combined(0.7, NEUTRAL, NEUTRAL, NEUTRAL, PROOF_COUNT_ALPHA),
            0.7
        );
        // Documented envelope: max ≈ +21%, min ≈ -19% over the base.
        let hi = combined(1.0, 1.0, 1.0, 1.0, PROOF_COUNT_ALPHA);
        let lo = combined(1.0, 0.0, 0.0, 0.0, PROOF_COUNT_ALPHA);
        assert!((hi - 1.1 * 1.1 * 1.05).abs() < 1e-12, "{hi}");
        assert!((lo - 0.9 * 0.9 * 0.95).abs() < 1e-12, "{lo}");
    }
}
