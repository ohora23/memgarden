//! Reciprocal Rank Fusion, ported from `engine/search/fusion.py:8-110`.

use std::collections::HashMap;

/// RRF constant. `engine/search/fusion.py:29` (`k: int = 60`).
pub const RRF_K: f64 = 60.0;

/// Arm order is part of the contract, not cosmetics: `fusion.py:56` indexes
/// this list positionally, so which arm a raw score is attributed to (and
/// which arm's row wins the first-occurrence tie) depends on it. `graph`
/// arrives in CE-7, `temporal` in CE-8; passing fewer lists than names is
/// fine, passing them out of order is not.
pub const SOURCE_NAMES: [&str; 4] = ["semantic", "bm25", "graph", "temporal"];

/// One arm's hit: a node id and that arm's own raw score (cosine similarity
/// for semantic, `bm25()` for keyword). Arms hand these over already sorted
/// best-first — RRF only reads the position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArmHit {
    pub id: i64,
    pub score: f64,
}

/// A fused candidate. `semantic`/`keyword` are the raw per-arm scores kept
/// separately (`fusion.py:88-92`) because the merged row keeps only the
/// first arm's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Merged {
    pub id: i64,
    pub rrf_score: f64,
    /// 1-based (`fusion.py:97` `start=1`).
    pub rrf_rank: usize,
    pub semantic: Option<f64>,
    pub keyword: Option<f64>,
}

/// Truncates one arm to its top `cap` (`fusion.py:8-26`). `0` disables,
/// matching legacy's default (`config.py:940`).
pub fn cap_per_source(results: &[ArmHit], cap: usize) -> &[ArmHit] {
    if cap == 0 || results.len() <= cap {
        results
    } else {
        &results[..cap]
    }
}

/// `score(d) = Σ_arms 1 / (k + rank)`, rank **1-based**.
///
/// Ties keep insertion order — the order a doc was first seen, walking arms
/// in `SOURCE_NAMES` order — because the sort below is stable over an
/// insertion-ordered `Vec`. That reproduces CPython's stable `sorted()` over
/// an insertion-ordered dict (`fusion.py:96-98`) without an `indexmap` dep.
pub fn reciprocal_rank_fusion(arms: &[Vec<ArmHit>], k: f64) -> Vec<Merged> {
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut merged: Vec<Merged> = Vec::new();

    for (arm_idx, results) in arms.iter().enumerate() {
        let source = SOURCE_NAMES.get(arm_idx).copied().unwrap_or("unknown");
        for (rank0, hit) in results.iter().enumerate() {
            let rank = rank0 + 1;
            let slot = *index.entry(hit.id).or_insert_with(|| {
                merged.push(Merged {
                    id: hit.id,
                    rrf_score: 0.0,
                    rrf_rank: 0,
                    semantic: None,
                    keyword: None,
                });
                merged.len() - 1
            });
            merged[slot].rrf_score += 1.0 / (k + rank as f64);
            match source {
                "semantic" => merged[slot].semantic = Some(hit.score),
                "bm25" => merged[slot].keyword = Some(hit.score),
                _ => {}
            }
        }
    }

    // Stable descending sort: equal scores keep first-seen order.
    merged.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (i, m) in merged.iter_mut().enumerate() {
        m.rrf_rank = i + 1;
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(ids: &[i64]) -> Vec<ArmHit> {
        ids.iter()
            .map(|&id| ArmHit {
                id,
                score: id as f64,
            })
            .collect()
    }

    #[test]
    fn rrf_exact_arithmetic() {
        // semantic: [1, 2]; bm25: [2, 3]
        let merged = reciprocal_rank_fusion(&[hits(&[1, 2]), hits(&[2, 3])], RRF_K);

        let by_id = |id: i64| *merged.iter().find(|m| m.id == id).unwrap();
        // doc 2 is rank 2 in semantic and rank 1 in bm25.
        assert_eq!(by_id(2).rrf_score, 1.0 / 62.0 + 1.0 / 61.0);
        assert_eq!(by_id(1).rrf_score, 1.0 / 61.0);
        assert_eq!(by_id(3).rrf_score, 1.0 / 62.0);

        // Highest score first, ranks 1-based.
        assert_eq!(merged[0].id, 2);
        assert_eq!(merged[0].rrf_rank, 1);
        assert_eq!(merged[1].rrf_rank, 2);
        assert_eq!(merged[2].rrf_rank, 3);
    }

    #[test]
    fn doc_in_three_arms_accumulates_all_three() {
        // Arm 3 (graph) contributes to the score but not to a named raw
        // score field — that is fusion.py's behaviour, not an omission.
        let merged = reciprocal_rank_fusion(&[hits(&[7]), hits(&[9, 7]), hits(&[8, 9, 7])], RRF_K);
        let seven = merged.iter().find(|m| m.id == 7).unwrap();
        assert_eq!(seven.rrf_score, 1.0 / 61.0 + 1.0 / 62.0 + 1.0 / 63.0);
        assert_eq!(seven.rrf_rank, 1);
        assert_eq!(seven.semantic, Some(7.0));
        assert_eq!(seven.keyword, Some(7.0));
    }

    #[test]
    fn arm_order_decides_score_attribution_and_ties() {
        let a = vec![ArmHit { id: 1, score: 0.9 }];
        let b = vec![ArmHit { id: 2, score: -3.5 }];

        // [semantic, bm25]: doc 1's score is semantic, doc 2's is keyword,
        // and the two tie at rank 1 so insertion order (semantic first) wins.
        let merged = reciprocal_rank_fusion(&[a.clone(), b.clone()], RRF_K);
        assert_eq!(merged[0].id, 1);
        assert_eq!(merged[0].semantic, Some(0.9));
        assert_eq!(merged[0].keyword, None);
        assert_eq!(merged[1].keyword, Some(-3.5));

        // Swapped: same scores, mirrored attribution and mirrored tie-break.
        let merged = reciprocal_rank_fusion(&[b, a], RRF_K);
        assert_eq!(merged[0].id, 2);
        assert_eq!(merged[0].semantic, Some(-3.5));
        assert_eq!(merged[1].keyword, Some(0.9));
    }

    /// Critic Revision R13 pins the four-arm order. Nothing else asserts
    /// that `graph` is slot 2 and `temporal` slot 3 — swap them today and the
    /// suite stays green, which stops being harmless the moment CE-8 fills
    /// slot 3 and every graph hit starts being attributed to the temporal
    /// arm's position.
    #[test]
    fn arm_slots_are_pinned_for_ce7_and_ce8() {
        assert_eq!(SOURCE_NAMES, ["semantic", "bm25", "graph", "temporal"]);

        // The pipeline's real shape: a graph-only hit in slot 2, with the
        // temporal slot still empty.
        let merged = reciprocal_rank_fusion(&[hits(&[1]), hits(&[1]), hits(&[5]), vec![]], RRF_K);
        let five = merged.iter().find(|m| m.id == 5).unwrap();
        assert_eq!(five.rrf_score, 1.0 / 61.0, "a graph hit still scores");
        assert_eq!(
            (five.semantic, five.keyword),
            (None, None),
            "slot 2 must not be attributed to a named raw-score field"
        );
        // Doc 1 was found by the two retrieval arms and outranks it.
        let one = merged.iter().find(|m| m.id == 1).unwrap();
        assert_eq!(one.semantic, Some(1.0));
        assert_eq!(one.keyword, Some(1.0));
        assert_eq!(merged[0].id, 1);

        // Putting the same hit in slot 3 instead must change nothing about
        // attribution — both are unnamed — but the arm it came from is the
        // caller's contract, so the *positions* are what this asserts.
        assert_eq!(SOURCE_NAMES[2], "graph");
        assert_eq!(SOURCE_NAMES[3], "temporal");
    }

    #[test]
    fn empty_arms_fuse_to_nothing() {
        assert!(reciprocal_rank_fusion(&[vec![], vec![]], RRF_K).is_empty());
        assert!(reciprocal_rank_fusion(&[], RRF_K).is_empty());
    }

    #[test]
    fn cap_per_source_zero_disables() {
        let h = hits(&[1, 2, 3]);
        assert_eq!(cap_per_source(&h, 0).len(), 3);
        assert_eq!(cap_per_source(&h, 5).len(), 3);
        assert_eq!(cap_per_source(&h, 2), &h[..2]);
    }
}
