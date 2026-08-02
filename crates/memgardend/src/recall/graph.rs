//! The graph recall arm (CE-7, PR B5) — slot 3 of `fusion::SOURCE_NAMES`.
//!
//! Two-pass topology, fixed by Critic Revision R13:
//!
//! ```text
//! pass 1: RRF(semantic, bm25, -, temporal) -> top-N seeds
//!         -> 1-hop expansion (links both ways + node_entities co-membership)
//! pass 2: RRF(semantic, bm25, graph, temporal)   <- first-occurrence decided here
//! ```
//!
//! Legacy reference: `engine/search/link_expansion_retrieval.py` (the three
//! signals and their score transforms, `:216-228`).

use memgarden_store::Db;
use memgarden_store::graph::{self, Neighbor};

use super::fusion::ArmHit;

/// Seeds handed to the expansion. `GRAPH_SEED_LIMIT`,
/// `link_expansion_retrieval.py:43`.
pub const GRAPH_SEEDS: usize = 20;

/// Hard ceiling on nodes the expansion may add, whatever the fan-out.
pub const GRAPH_EXPANSION_CAP: usize = 200;

/// Rows the two expansion queries may return before ranking. Four edges per
/// admitted node is slack for the many-edges-to-one-node case.
///
/// `// ponytail: one flat cap instead of legacy's per-entity LATERAL limit.
/// Measured at 3k nodes the whole arm is well inside its 5ms budget; add the
/// per-entity cap if a single entity ever names five digits of nodes.`
const EDGE_FETCH_CAP: usize = GRAPH_EXPANSION_CAP * 4;

/// The four causal link types. `caused_by` is the only one retain writes;
/// the other three exist for transfer-imported banks
/// (`causal_links.py:12`) and are boosted the same way.
fn is_causal(link_type: &str) -> bool {
    matches!(
        link_type,
        "caused_by" | "causes" | "enables" | "prevents"
    )
}

/// Runs the arm: expand `seeds` inside `bank_id`, rank, cap. Returns hits
/// best-first, ready for RRF. An empty seed list is an empty arm — recall
/// with no BM25/vector candidates has nothing to expand from.
pub fn arm(db: &Db, bank_id: &str, seeds: &[i64]) -> memgarden_core::error::Result<Vec<ArmHit>> {
    if seeds.is_empty() {
        return Ok(vec![]);
    }
    let (links, shared) = graph::expand(db, bank_id, seeds, EDGE_FETCH_CAP)?;
    Ok(rank(&links, &shared))
}

/// `link_expansion_retrieval.py:216-228`: the three signals are scored
/// separately and **added**, so a node reached by more than one of them
/// outranks a node reached by just the strongest.
///
/// * entity co-membership → `tanh(shared_count * 0.5)` (1 shared entity 0.46,
///   2 → 0.76, 3 → 0.91 — saturates on its own)
/// * causal link → `weight + 1.0` (legacy boosts it as the highest-quality
///   signal)
/// * semantic/temporal link → `weight`
///
/// Each link bucket keeps its `max`, matching legacy's `MAX(weight)` /
/// `DISTINCT ON` shapes.
pub fn rank(links: &[Neighbor], shared: &[(i64, i64)]) -> Vec<ArmHit> {
    use std::collections::HashMap;

    let mut entity: HashMap<i64, f64> = HashMap::new();
    let mut causal: HashMap<i64, f64> = HashMap::new();
    let mut plain: HashMap<i64, f64> = HashMap::new();

    for (node_id, count) in shared {
        entity.insert(*node_id, (*count as f64 * 0.5).tanh());
    }
    for n in links {
        let bucket = if is_causal(&n.link_type) {
            &mut causal
        } else {
            &mut plain
        };
        let boost = if is_causal(&n.link_type) { 1.0 } else { 0.0 };
        let slot = bucket.entry(n.node_id).or_insert(f64::MIN);
        *slot = slot.max(n.weight + boost);
    }

    let mut ids: Vec<i64> = entity
        .keys()
        .chain(causal.keys())
        .chain(plain.keys())
        .copied()
        .collect();
    ids.sort_unstable();
    ids.dedup();

    let mut hits: Vec<ArmHit> = ids
        .into_iter()
        .map(|id| ArmHit {
            id,
            score: entity.get(&id).copied().unwrap_or(0.0)
                + causal.get(&id).copied().unwrap_or(0.0)
                + plain.get(&id).copied().unwrap_or(0.0),
        })
        .collect();
    // Descending score; ties break on id so the arm's rank order (and
    // therefore its RRF contribution) is deterministic.
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });
    hits.truncate(GRAPH_EXPANSION_CAP);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(node_id: i64, link_type: &str, weight: f64) -> Neighbor {
        Neighbor {
            node_id,
            link_type: link_type.to_string(),
            weight,
        }
    }

    #[test]
    fn causal_outranks_semantic_of_the_same_weight() {
        let hits = rank(
            &[link(1, "semantic", 0.9), link(2, "caused_by", 0.9)],
            &[],
        );
        assert_eq!(hits[0].id, 2);
        assert_eq!(hits[0].score, 1.9);
        assert_eq!(hits[1].score, 0.9);
    }

    #[test]
    fn signals_add_up_and_each_bucket_keeps_its_max() {
        // Node 1: two semantic links (max 0.8), one causal (1.5), and two
        // shared entities (tanh(1.0) = 0.7616).
        let hits = rank(
            &[
                link(1, "semantic", 0.4),
                link(1, "semantic", 0.8),
                link(1, "caused_by", 0.5),
                link(1, "temporal", 0.3),
            ],
            &[(1, 2)],
        );
        assert_eq!(hits.len(), 1);
        let expected = 1.0f64.tanh() + 1.5 + 0.8;
        assert!((hits[0].score - expected).abs() < 1e-12, "{}", hits[0].score);
    }

    #[test]
    fn entity_score_saturates_as_legacy_documents() {
        let hits = rank(&[], &[(1, 1), (2, 2), (3, 3), (4, 4)]);
        let score = |id: i64| hits.iter().find(|h| h.id == id).unwrap().score;
        for (id, want) in [(1, 0.46), (2, 0.76), (3, 0.91), (4, 0.96)] {
            assert!(
                (score(id) - want).abs() < 0.005,
                "{id} shared entities scored {}",
                score(id)
            );
        }
    }

    #[test]
    fn expansion_is_capped_at_two_hundred_nodes() {
        let links: Vec<Neighbor> = (0..500).map(|i| link(i, "semantic", 0.5)).collect();
        let hits = rank(&links, &[]);
        assert_eq!(hits.len(), GRAPH_EXPANSION_CAP);
    }

    #[test]
    fn no_seeds_no_arm() {
        let db = Db::open_memory().unwrap();
        assert!(arm(&db, "b1", &[]).unwrap().is_empty());
    }
}
