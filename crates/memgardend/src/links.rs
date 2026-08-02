//! Typed link construction (CE-7, PR B5) — the *rules*, as pure functions.
//! The inserts live in `memgarden_store::graph::insert_links`.
//!
//! Legacy writes only three of the seven link types at retain
//! (`link_utils.py`, brief §9): `temporal`, `semantic` and `caused_by`.
//! **`'entity'` rows are never written** — entity retrieval joins
//! `node_entities` instead, which is why the graph arm expands through that
//! table rather than through an entity edge.

use std::collections::HashMap;

use memgarden_store::graph::NewLink;

use crate::extract::parse::ParsedFact;

/// Temporal-link window (`link_utils.py:291`, `time_window_hours = 24`).
pub const TEMPORAL_WINDOW_HOURS: f64 = 24.0;

/// `MAX_TEMPORAL_LINKS_PER_UNIT` (`link_utils.py:30`). Retrieval reads at
/// most 10-20 neighbours per node, so more is write amplification.
pub const MAX_TEMPORAL_LINKS_PER_NODE: usize = 20;

/// `DEFAULT_SEMANTIC_LINK_MIN_SIMILARITY` (`config.py:929`). Compared with
/// `>=` (`link_utils.py:561,619`).
pub const SEMANTIC_LINK_THRESHOLD: f64 = 0.7;

/// Neighbours per node in the streaming semantic pass. **20, not 50**: the
/// signature default is 50 but the only live caller passes 20
/// (`orchestrator.py:1232`, brief gotcha #9).
pub const SEMANTIC_LINK_TOP_K: usize = 20;

/// `DEFAULT_CAUSAL_LINK_WEIGHT` (`causal_links.py:18`).
pub const CAUSAL_LINK_WEIGHT: f64 = 1.0;

const MS_PER_HOUR: f64 = 3_600_000.0;

/// `max(0.3, 1.0 - hours_diff / 24)` — `link_utils.py:377`.
pub fn temporal_weight(hours_diff: f64) -> f64 {
    (1.0 - hours_diff / TEMPORAL_WINDOW_HOURS).max(0.3)
}

/// A node as the temporal pass sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedNode {
    pub id: i64,
    pub fact_type: String,
    pub event_date: i64,
}

/// Temporal links from each node in `new_nodes` to every node in `window`
/// that shares its `fact_type` (`link_utils.py:394-395`) and sits within 24 h.
///
/// `window` is the whole candidate window *including* the new nodes, so the
/// within-batch case (which legacy handles in a separate loop, `:381-405`)
/// falls out of the same pass. Links are bidirectional within the batch
/// because both endpoints are in `new_nodes` and each side emits its own
/// direction; a link to a pre-existing node is one-directional, matching the
/// LATERAL half of legacy.
///
/// Capped at `MAX_TEMPORAL_LINKS_PER_NODE` per `from` node, ranked by weight
/// descending (`_cap_links_per_unit`, `link_utils.py:33`).
pub fn temporal_links(new_nodes: &[TimedNode], window: &[TimedNode]) -> Vec<NewLink> {
    let mut out = Vec::new();
    for node in new_nodes {
        let mut mine: Vec<NewLink> = window
            .iter()
            .filter(|other| other.id != node.id && other.fact_type == node.fact_type)
            .filter_map(|other| {
                let hours = (node.event_date - other.event_date).abs() as f64 / MS_PER_HOUR;
                (hours <= TEMPORAL_WINDOW_HOURS).then(|| NewLink {
                    from_node_id: node.id,
                    to_node_id: other.id,
                    link_type: "temporal",
                    weight: temporal_weight(hours),
                })
            })
            .collect();
        // Stable sort on weight, then id, so the cap is deterministic when
        // several neighbours share a timestamp (retain's +10ms offsets make
        // exact ties rare but not impossible across chunks).
        mine.sort_by(|a, b| {
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.to_node_id.cmp(&b.to_node_id))
        });
        mine.truncate(MAX_TEMPORAL_LINKS_PER_NODE);
        out.append(&mut mine);
    }
    out
}

/// `caused_by` edges from each fact to the fact it names, weight 1.0.
///
/// `ids[i]` is the node id written for `facts[i]`; `causal_relations`
/// `target_index` values have already been remapped to survivor ordinals by
/// `parse_facts`. Self-links are dropped — "this fact was caused by itself"
/// is extraction noise, and the row would be a self-loop in the graph.
/// The relation type is not read: retain writes only the canonical
/// `caused_by` (`causal_links.py:11`, `fact_extraction.py:177`).
pub fn causal_links(facts: &[ParsedFact], ids: &[i64]) -> Vec<NewLink> {
    facts
        .iter()
        .zip(ids)
        .flat_map(|(fact, &from)| {
            fact.causal_relations.iter().filter_map(move |rel| {
                let to = *ids.get(rel.target_index)?;
                (to != from).then_some(NewLink {
                    from_node_id: from,
                    to_node_id: to,
                    link_type: "caused_by",
                    weight: CAUSAL_LINK_WEIGHT,
                })
            })
        })
        .collect()
}

/// Semantic links for one just-embedded node: its top-`SEMANTIC_LINK_TOP_K`
/// same-`fact_type` neighbours at cosine similarity `>= 0.7`.
///
/// `neighbors` is `(id, cosine similarity)` best-first, as `search::knn`
/// returns after its distance is converted. `types` maps a candidate id to
/// its `fact_type`; a candidate missing from it (deleted between the KNN and
/// this call) is skipped rather than linked blind.
///
/// `// ponytail: the fact_type filter runs after the KNN (vec0 partitions on
/// bank_id only), so a caller over-fetching k = TOP_K * 5 can still come back
/// with fewer than 20 links in a bank dominated by one other fact_type. That
/// is a thinner graph, never a wrong one; widen the over-fetch if a bank ever
/// skews that hard.`
pub fn semantic_links(
    node_id: i64,
    fact_type: &str,
    neighbors: &[(i64, f64)],
    types: &HashMap<i64, (String, String)>,
) -> Vec<NewLink> {
    neighbors
        .iter()
        .filter(|(id, _)| *id != node_id)
        .filter(|(_, sim)| *sim >= SEMANTIC_LINK_THRESHOLD)
        .filter(|(id, _)| types.get(id).is_some_and(|(_, ft)| ft == fact_type))
        .take(SEMANTIC_LINK_TOP_K)
        .map(|&(id, sim)| NewLink {
            from_node_id: node_id,
            to_node_id: id,
            link_type: "semantic",
            weight: sim.clamp(0.0, 1.0),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use memgarden_core::types::FactType;

    const HOUR: i64 = 3_600_000;

    #[test]
    fn temporal_weight_at_the_documented_points() {
        assert_eq!(temporal_weight(0.0), 1.0);
        assert_eq!(temporal_weight(12.0), 0.5);
        // At exactly the window edge the formula would give 0.0; the 0.3
        // floor is what legacy writes.
        assert_eq!(temporal_weight(24.0), 0.3);
        // Beyond the window the floor still applies — the *window* check is
        // what excludes 48h, not the weight.
        assert_eq!(temporal_weight(48.0), 0.3);
    }

    fn node(id: i64, hours: i64) -> TimedNode {
        TimedNode {
            id,
            fact_type: FactType::World.as_str().to_string(),
            event_date: hours * HOUR,
        }
    }

    #[test]
    fn temporal_links_respect_the_window_and_are_bidirectional_in_batch() {
        let a = node(1, 0);
        let b = node(2, 12); // inside
        let c = node(3, 48); // outside
        let new = vec![a.clone(), b.clone(), c.clone()];
        let links = temporal_links(&new, &new);

        let has = |from: i64, to: i64| {
            links
                .iter()
                .any(|l| l.from_node_id == from && l.to_node_id == to)
        };
        assert!(
            has(1, 2) && has(2, 1),
            "within-batch links are bidirectional"
        );
        assert!(!has(1, 3) && !has(3, 1), "48h apart is outside the window");
        // 12h apart -> 0.5, and exactly 24h apart (b..c is 36h) stays out.
        let ab = links
            .iter()
            .find(|l| l.from_node_id == 1 && l.to_node_id == 2)
            .unwrap();
        assert_eq!(ab.weight, 0.5);
        assert_eq!(ab.link_type, "temporal");
        assert!(links.iter().all(|l| l.from_node_id != l.to_node_id));
    }

    #[test]
    fn temporal_links_only_pair_the_same_fact_type() {
        let mut other = node(2, 1);
        other.fact_type = FactType::Observation.as_str().to_string();
        let new = vec![node(1, 0), other];
        assert!(
            temporal_links(&new, &new).is_empty(),
            "a world fact and an observation an hour apart must not link"
        );
    }

    #[test]
    fn temporal_links_cap_at_twenty_per_node() {
        // One new node, 30 candidates all inside the window at increasing
        // distance: the 20 nearest (highest weight) survive.
        let new = vec![node(1, 0)];
        let mut window = vec![node(1, 0)];
        for i in 1..=30 {
            window.push(node(100 + i, i));
        }
        let links = temporal_links(&new, &window);
        assert_eq!(links.len(), MAX_TEMPORAL_LINKS_PER_NODE);
        let kept: Vec<i64> = links.iter().map(|l| l.to_node_id).collect();
        assert!(kept.contains(&101), "the closest neighbour must survive");
        // 24 candidates fall inside the window (hours 1..=24); the 20
        // closest survive, so hour 20 is in and hour 21 is out.
        assert!(kept.contains(&120));
        assert!(
            !kept.contains(&121),
            "the 21st-closest neighbour loses to 20 closer ones"
        );
        // Two new nodes get 20 links each, not 20 between them.
        let two = vec![node(1, 0), node(2, 1)];
        window.push(node(2, 1));
        assert_eq!(
            temporal_links(&two, &window).len(),
            2 * MAX_TEMPORAL_LINKS_PER_NODE
        );
    }

    fn fact(text: &str, targets: &[usize]) -> ParsedFact {
        ParsedFact {
            text: text.to_string(),
            fact_type: FactType::World,
            fact_kind: "conversation".to_string(),
            occurred_start: None,
            occurred_end: None,
            where_field: None,
            entities: vec![],
            causal_relations: targets
                .iter()
                .map(|&target_index| crate::extract::parse::CausalRelation {
                    target_index,
                    relation_type: "caused_by".to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn causal_links_drop_self_references() {
        let facts = vec![fact("a", &[1]), fact("b", &[1]), fact("c", &[9])];
        let ids = vec![10i64, 11, 12];
        let links = causal_links(&facts, &ids);
        assert_eq!(links.len(), 1, "self-link and out-of-range target dropped");
        assert_eq!(links[0].from_node_id, 10);
        assert_eq!(links[0].to_node_id, 11);
        assert_eq!(links[0].link_type, "caused_by");
        assert_eq!(links[0].weight, CAUSAL_LINK_WEIGHT);
    }

    fn types(pairs: &[(i64, &str)]) -> HashMap<i64, (String, String)> {
        pairs
            .iter()
            .map(|(id, ft)| (*id, ("b1".to_string(), (*ft).to_string())))
            .collect()
    }

    #[test]
    fn semantic_links_threshold_is_inclusive_at_zero_point_seven() {
        let t = types(&[(2, "world"), (3, "world"), (4, "world")]);
        let links = semantic_links(1, "world", &[(2, 0.700_1), (3, 0.7), (4, 0.699_9)], &t);
        let ids: Vec<i64> = links.iter().map(|l| l.to_node_id).collect();
        assert_eq!(ids, vec![2, 3], "0.7 links, just under 0.7 does not");
        assert_eq!(links[1].weight, 0.7);
        assert!(links.iter().all(|l| l.link_type == "semantic"));
    }

    #[test]
    fn semantic_links_are_per_fact_type_capped_at_twenty_and_skip_self() {
        let mut pairs: Vec<(i64, &str)> = vec![(1, "world")];
        let mut neighbors = vec![(1i64, 0.99f64)]; // self
        for i in 2..=40 {
            pairs.push((i, if i % 2 == 0 { "world" } else { "observation" }));
            neighbors.push((i, 0.9));
        }
        let links = semantic_links(1, "world", &neighbors, &types(&pairs));
        assert_eq!(links.len(), SEMANTIC_LINK_TOP_K);
        assert!(links.iter().all(|l| l.to_node_id % 2 == 0));
        assert!(links.iter().all(|l| l.to_node_id != 1), "no self-link");
        // An id the type lookup does not know is skipped, not linked blind.
        assert!(semantic_links(1, "world", &[(99, 0.95)], &types(&pairs)).is_empty());
    }
}
