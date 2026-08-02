//! Entity, co-occurrence and link storage — the SQL half of CE-7 (PR B5).
//!
//! The *policy* half (name normalization, Ratcliff/Obershelp resolution
//! scoring, which pairs become links) lives in `memgardend::entities` /
//! `memgardend::links`; this module only reads and writes rows.
//!
//! Legacy references: `engine/retain/entity_resolver.py` (resolution and
//! co-occurrence), `engine/retain/link_utils.py` (bulk link insert),
//! `engine/search/link_expansion_retrieval.py` (graph expansion).

use std::collections::{HashMap, HashSet};

use rusqlite::params;

use memgarden_core::error::Result;
use memgarden_core::types::FactType;

use crate::{Db, store_err};

/// One existing entity, as the resolver sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityCandidate {
    pub id: i64,
    pub canonical_name: String,
    pub last_seen: Option<i64>,
}

/// Everything the resolver needs about a bank, loaded once per retain chunk.
///
/// `// ponytail: full-scan candidates (Critic Revision R6) — a bank holds
/// hundreds to a few thousand entities and this is one indexed scan per
/// chunk, not per mention. Add a per-bank FTS table over entity names if a
/// bank ever passes ~10k entities.`
#[derive(Debug, Default)]
pub struct ResolutionContext {
    pub candidates: Vec<EntityCandidate>,
    /// entity id -> canonical names it has co-occurred with. Feeds the
    /// overlap term of `entity_resolver.py:691-696`.
    pub cooccurring: HashMap<i64, HashSet<String>>,
}

pub fn load_resolution_context(db: &Db, bank_id: &str) -> Result<ResolutionContext> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare("SELECT id, canonical_name, last_seen FROM entities WHERE bank_id = ?1")
        .map_err(store_err)?;
    let candidates = stmt
        .query_map(params![bank_id], |r| {
            Ok(EntityCandidate {
                id: r.get(0)?,
                canonical_name: r.get(1)?,
                last_seen: r.get(2)?,
            })
        })
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    // Both directions: the table stores each pair once, canonically ordered.
    let mut stmt = conn
        .prepare(
            "SELECT c.entity_id_1, e2.canonical_name, c.entity_id_2, e1.canonical_name
             FROM entity_cooccurrences c
             JOIN entities e1 ON e1.id = c.entity_id_1
             JOIN entities e2 ON e2.id = c.entity_id_2
             WHERE e1.bank_id = ?1",
        )
        .map_err(store_err)?;
    let mut cooccurring: HashMap<i64, HashSet<String>> = HashMap::new();
    let rows = stmt
        .query_map(params![bank_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(store_err)?;
    for row in rows {
        let (id1, name2, id2, name1) = row.map_err(store_err)?;
        cooccurring.entry(id1).or_default().insert(name2);
        cooccurring.entry(id2).or_default().insert(name1);
    }

    Ok(ResolutionContext {
        candidates,
        cooccurring,
    })
}

/// Upserts every mentioned entity, attaches it to its node, and folds the
/// batch's co-occurrence pairs — all in one `BEGIN IMMEDIATE`.
///
/// `per_node` carries **already normalized and already resolved** canonical
/// names (`memgardend::entities`). Returns `canonical_name -> entity id` for
/// the whole batch.
///
/// `entity_type` is deliberately left NULL: legacy hardcodes `"CONCEPT"` for
/// every LLM-extracted entity (`entity_processing.py:32`) and never reads it
/// back, so persisting a constant would only cost a column.
pub fn write_entities(
    db: &Db,
    bank_id: &str,
    per_node: &[(i64, Vec<String>)],
    seen_at: i64,
    now: i64,
) -> Result<HashMap<String, i64>> {
    // Mentions are counted per occurrence (legacy appends one _EntityStat per
    // resolved mention, `entity_resolver.py:718`), so a name naming two facts
    // in the same chunk bumps mention_count by two.
    let mut mentions: HashMap<&str, i64> = HashMap::new();
    for (_, names) in per_node {
        for name in names {
            *mentions.entry(name.as_str()).or_insert(0) += 1;
        }
    }
    if mentions.is_empty() {
        return Ok(HashMap::new());
    }
    // Deterministic upsert order (and therefore deterministic lock order),
    // same reasoning as the link insert below.
    let mut ordered: Vec<(&str, i64)> = mentions.into_iter().collect();
    ordered.sort_unstable();

    db.write(|tx| {
        let mut ids: HashMap<String, i64> = HashMap::new();
        for (name, count) in &ordered {
            let id: i64 = tx
                .query_row(
                    "INSERT INTO entities
                       (bank_id, canonical_name, created_at, mention_count, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                     ON CONFLICT (bank_id, canonical_name) DO UPDATE SET
                       mention_count = entities.mention_count + excluded.mention_count,
                       first_seen    = min(coalesce(entities.first_seen, excluded.first_seen),
                                           excluded.first_seen),
                       last_seen     = max(coalesce(entities.last_seen, excluded.last_seen),
                                           excluded.last_seen)
                     RETURNING id",
                    params![bank_id, name, now, count, seen_at],
                    |r| r.get(0),
                )
                .map_err(store_err)?;
            ids.insert((*name).to_string(), id);
        }

        for (node_id, names) in per_node {
            for name in names {
                let Some(entity_id) = ids.get(name) else {
                    continue;
                };
                tx.execute(
                    "INSERT OR IGNORE INTO node_entities (node_id, entity_id) VALUES (?1, ?2)",
                    params![node_id, entity_id],
                )
                .map_err(store_err)?;
            }
        }

        // Co-occurrence: every distinct pair sharing a node, canonicalized
        // `a < b` to match the PK and the CHECK
        // (`entity_resolver.py:80-93`), folded across the batch so one
        // statement per pair (`:220-231`).
        let mut pairs: HashMap<(i64, i64), i64> = HashMap::new();
        for (_, names) in per_node {
            let mut node_ids: Vec<i64> = names.iter().filter_map(|n| ids.get(n).copied()).collect();
            node_ids.sort_unstable();
            node_ids.dedup();
            for (i, a) in node_ids.iter().enumerate() {
                for b in &node_ids[i + 1..] {
                    *pairs.entry((*a, *b)).or_insert(0) += 1;
                }
            }
        }
        let mut pairs: Vec<((i64, i64), i64)> = pairs.into_iter().collect();
        pairs.sort_unstable(); // consistent lock ordering (link_utils.py:92)
        for ((a, b), count) in pairs {
            tx.execute(
                "INSERT INTO entity_cooccurrences
                   (entity_id_1, entity_id_2, cooccurrence_count, last_cooccurred)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (entity_id_1, entity_id_2) DO UPDATE SET
                   cooccurrence_count = entity_cooccurrences.cooccurrence_count
                                        + excluded.cooccurrence_count,
                   last_cooccurred    = max(entity_cooccurrences.last_cooccurred,
                                            excluded.last_cooccurred)",
                params![a, b, count, seen_at],
            )
            .map_err(store_err)?;
        }

        Ok(ids)
    })
}

/// One edge to write. `entity_id` is always the `0` sentinel: retain never
/// writes `'entity'` link rows (legacy grounds entity retrieval in
/// `node_entities` instead — `link_utils` / brief §9), so no edge here is
/// entity-scoped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NewLink {
    pub from_node_id: i64,
    pub to_node_id: i64,
    pub link_type: &'static str,
    pub weight: f64,
}

/// Bulk-inserts links in one transaction, **sorted by
/// `(from_node_id, to_node_id)`** for consistent lock ordering across
/// concurrent writers (`link_utils.py:92`). Existing edges are left alone
/// (legacy's `ON CONFLICT DO NOTHING`, `link_utils.py:452-456`). Returns the
/// number of rows actually inserted.
pub fn insert_links(db: &Db, links: &[NewLink], now: i64) -> Result<usize> {
    if links.is_empty() {
        return Ok(0);
    }
    let mut sorted: Vec<NewLink> = links.to_vec();
    sorted.sort_by_key(|l| (l.from_node_id, l.to_node_id));

    db.write(|tx| {
        let mut written = 0usize;
        for link in &sorted {
            written += tx
                .execute(
                    "INSERT INTO links (from_node_id, to_node_id, link_type, entity_id, weight, created_at)
                     VALUES (?1, ?2, ?3, 0, ?4, ?5)
                     ON CONFLICT DO NOTHING",
                    params![
                        link.from_node_id,
                        link.to_node_id,
                        link.link_type,
                        link.weight.clamp(0.0, 1.0),
                        now
                    ],
                )
                .map_err(store_err)?;
        }
        Ok(written)
    })
}

/// `(id, fact_type, event_date)` for every node in `bank_id` whose
/// `event_date` falls inside `[from_ms, to_ms]` — the temporal-link candidate
/// window (`link_utils.py:291`, 24 h).
///
/// `// ponytail: loads the window into Rust instead of legacy's LATERAL
/// per-unit top-N. A 24h window holds one session's facts (tens to low
/// thousands of 24-byte rows) and the pairing is O(new × window) with new ≤
/// the facts in one chunk. Push the cap into SQL if a bank ever retains
/// enough in one day for this to show up.`
pub fn nodes_in_window(
    db: &Db,
    bank_id: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<(i64, String, i64)>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, fact_type, event_date FROM memory_nodes
             WHERE bank_id = ?1 AND event_date IS NOT NULL AND event_date BETWEEN ?2 AND ?3",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![bank_id, from_ms, to_ms], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// `(id, bank_id, fact_type)` for `ids` — the backlog worker knows the ids it
/// just embedded but not their types, and semantic links are per-fact_type
/// (`orchestrator.py:1232`).
pub fn node_types(db: &Db, ids: &[i64]) -> Result<HashMap<i64, (String, String)>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, bank_id, fact_type FROM memory_nodes
             WHERE id IN (SELECT value FROM json_each(?1))",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![ids_json(ids)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, String>(2)?),
            ))
        })
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<HashMap<_, _>>>()
        .map_err(store_err)
}

/// One 1-hop edge out of the seed set.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbor {
    pub node_id: i64,
    pub link_type: String,
    pub weight: f64,
}

/// One-hop expansion of `seeds` inside `bank_id`, in both link directions
/// plus `node_entities` co-membership. Seeds themselves are excluded, matching
/// legacy (`link_expansion_retrieval.py:626,670` — `id != ALL(seeds)`).
///
/// `(node_id, number of entities it shares with the seed set)`.
pub type SharedEntities = Vec<(i64, i64)>;

/// Returns `(link neighbours, (node_id, shared entity count))`. Ranking and
/// the 200-node cap are the caller's (`memgardend::recall::graph`), which is
/// where the scoring formula lives.
pub fn expand(
    db: &Db,
    bank_id: &str,
    seeds: &[i64],
    edge_limit: usize,
) -> Result<(Vec<Neighbor>, SharedEntities)> {
    if seeds.is_empty() {
        return Ok((vec![], vec![]));
    }
    let seeds_json = ids_json(seeds);
    let conn = db.read()?;

    let mut stmt = conn
        .prepare(
            // CROSS JOIN is an ordering directive in SQLite, not a
            // different join: it pins the seed list as the outermost loop.
            // Left to itself the planner drove from `memory_nodes` and
            // scanned the whole bank partition (measured 15ms at 3k nodes vs
            // 0.2ms pinned) — the seeds are 20 rows and everything else is an
            // index probe off them.
            "SELECT node_id, link_type, weight FROM (
               SELECT l.to_node_id AS node_id, l.link_type AS link_type, l.weight AS weight
               FROM json_each(?1) s
               CROSS JOIN links l ON l.from_node_id = s.value
               CROSS JOIN memory_nodes n ON n.id = l.to_node_id
               WHERE n.bank_id = ?2
                 AND l.to_node_id NOT IN (SELECT value FROM json_each(?1))
               UNION ALL
               SELECT l.from_node_id, l.link_type, l.weight
               FROM json_each(?1) s
               CROSS JOIN links l ON l.to_node_id = s.value
               CROSS JOIN memory_nodes n ON n.id = l.from_node_id
               WHERE n.bank_id = ?2
                 AND l.from_node_id NOT IN (SELECT value FROM json_each(?1))
             )
             ORDER BY weight DESC
             LIMIT ?3",
        )
        .map_err(store_err)?;
    let links = stmt
        .query_map(params![seeds_json, bank_id, edge_limit as i64], |r| {
            Ok(Neighbor {
                node_id: r.get(0)?,
                link_type: r.get(1)?,
                weight: r.get(2)?,
            })
        })
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    // `// ponytail: no per-entity fan-out cap (legacy's LATERAL
    // graph_per_entity_limit = 200). The outer LIMIT bounds the result, not
    // the join; add the per-entity cap if one entity ever names a five-digit
    // number of nodes.`
    let mut stmt = conn
        .prepare(
            "SELECT ne2.node_id, count(DISTINCT ne2.entity_id) AS shared
             FROM json_each(?1) s
             CROSS JOIN node_entities ne1 ON ne1.node_id = s.value
             CROSS JOIN node_entities ne2 ON ne2.entity_id = ne1.entity_id
             CROSS JOIN memory_nodes n ON n.id = ne2.node_id
             WHERE n.bank_id = ?2
               AND ne2.node_id NOT IN (SELECT value FROM json_each(?1))
             GROUP BY ne2.node_id
             ORDER BY shared DESC
             LIMIT ?3",
        )
        .map_err(store_err)?;
    let shared = stmt
        .query_map(params![seeds_json, bank_id, edge_limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    Ok((links, shared))
}

/// A node as the graph viewer sees it (`GET /v1/banks/{id}/graph`).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphNode {
    pub id: i64,
    pub uuid: String,
    pub fact_type: String,
    pub text: String,
    pub mentioned_at: Option<i64>,
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphEdge {
    pub from_node_id: i64,
    pub to_node_id: i64,
    pub link_type: String,
    pub weight: f64,
}

/// Nodes + the links between them for the GV-1..3 viewer. `fact_types` empty
/// means all; `session` filters on the `session:{id}` tag B3 writes (Critic
/// Revision R15). Only edges whose *both* endpoints are in the returned node
/// set come back, so the viewer never draws a dangling edge.
pub fn graph_view(
    db: &Db,
    bank_id: &str,
    limit: usize,
    fact_types: &[FactType],
    session: Option<&str>,
) -> Result<(Vec<GraphNode>, Vec<GraphEdge>)> {
    // Same shape as `search::fts_candidates_filtered`: one prepared statement
    // for every filter combination, values are `FactType::as_str` literals.
    let types_json: Option<String> = if fact_types.is_empty() {
        None
    } else {
        Some(format!(
            "[{}]",
            fact_types
                .iter()
                .map(|t| format!("\"{}\"", t.as_str()))
                .collect::<Vec<_>>()
                .join(",")
        ))
    };
    let session_tag = session.map(|s| format!("session:{s}"));

    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT n.id, n.uuid, n.fact_type, n.text, n.mentioned_at
             FROM memory_nodes n
             WHERE n.bank_id = ?1
               AND (?3 IS NULL OR n.fact_type IN (SELECT value FROM json_each(?3)))
               AND (?4 IS NULL OR EXISTS (
                     SELECT 1 FROM node_tags t WHERE t.node_id = n.id AND t.tag = ?4))
             ORDER BY n.id DESC
             LIMIT ?2",
        )
        .map_err(store_err)?;
    let mut nodes = stmt
        .query_map(
            params![bank_id, limit as i64, types_json, session_tag],
            |r| {
                Ok(GraphNode {
                    id: r.get(0)?,
                    uuid: r.get(1)?,
                    fact_type: r.get(2)?,
                    text: r.get(3)?,
                    mentioned_at: r.get(4)?,
                    entities: vec![],
                })
            },
        )
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    let ids: Vec<i64> = nodes.iter().map(|n| n.id).collect();
    if ids.is_empty() {
        return Ok((nodes, vec![]));
    }
    let ids_json = ids_json(&ids);

    let mut stmt = conn
        .prepare(
            "SELECT ne.node_id, e.canonical_name
             FROM node_entities ne JOIN entities e ON e.id = ne.entity_id
             WHERE ne.node_id IN (SELECT value FROM json_each(?1))
             ORDER BY ne.node_id, e.canonical_name",
        )
        .map_err(store_err)?;
    let mut by_node: HashMap<i64, Vec<String>> = HashMap::new();
    let rows = stmt
        .query_map(params![ids_json], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(store_err)?;
    for row in rows {
        let (node_id, name) = row.map_err(store_err)?;
        by_node.entry(node_id).or_default().push(name);
    }
    for node in &mut nodes {
        node.entities = by_node.remove(&node.id).unwrap_or_default();
    }

    let mut stmt = conn
        .prepare(
            "SELECT from_node_id, to_node_id, link_type, weight FROM links
             WHERE from_node_id IN (SELECT value FROM json_each(?1))
               AND to_node_id   IN (SELECT value FROM json_each(?1))
             ORDER BY from_node_id, to_node_id, link_type",
        )
        .map_err(store_err)?;
    let edges = stmt
        .query_map(params![ids_json], |r| {
            Ok(GraphEdge {
                from_node_id: r.get(0)?,
                to_node_id: r.get(1)?,
                link_type: r.get(2)?,
                weight: r.get(3)?,
            })
        })
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    Ok((nodes, edges))
}

/// i64s as a JSON array — no injection surface and one statement shape
/// regardless of how many ids there are (same trick as `search::hydrate`).
fn ids_json(ids: &[i64]) -> String {
    format!(
        "[{}]",
        ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    )
}
