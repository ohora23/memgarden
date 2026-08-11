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

use rusqlite::{OptionalExtension, params};

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

/// Candidate ceiling per bank (Critic Revision R6 + security MED-5). The
/// resolver is O(mentions x candidates), so the matrix has to be bounded by
/// something other than how long the bank has been alive. Ordered by
/// `last_seen DESC`, because the temporal term already says a stale entity is
/// the least likely match.
///
/// `// ponytail: newest-N full scan. A per-bank FTS table over entity names
/// is the upgrade if a bank ever needs to resolve against more than this.`
pub const MAX_RESOLUTION_CANDIDATES: usize = 5_000;

/// Co-occurrence partners kept per entity (security MED-6). Loading the whole
/// table is quadratic in bank age; the overlap term only ever asks "is this
/// nearby name one of yours", and a partner past the 64th strongest is not
/// the one carrying a resolution over the gate. `idx_entity_cooc_count`
/// serves the ranking.
pub const MAX_COOCCURRENCE_PARTNERS: usize = 64;

/// Everything the resolver needs about a bank, loaded once per retain chunk.
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
        .prepare(
            "SELECT id, canonical_name, last_seen FROM entities WHERE bank_id = ?1
             ORDER BY last_seen DESC LIMIT ?2",
        )
        .map_err(store_err)?;
    let candidates = stmt
        .query_map(params![bank_id, MAX_RESOLUTION_CANDIDATES as i64], |r| {
            Ok(EntityCandidate {
                id: r.get(0)?,
                canonical_name: r.get(1)?,
                last_seen: r.get(2)?,
            })
        })
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    // Both directions: the table stores each pair once, canonically ordered,
    // and each side keeps only its strongest partners.
    let mut stmt = conn
        .prepare(
            "WITH ranked AS (
               SELECT c.entity_id_1, c.entity_id_2, c.cooccurrence_count,
                      row_number() OVER (PARTITION BY c.entity_id_1
                                         ORDER BY c.cooccurrence_count DESC) AS rn1,
                      row_number() OVER (PARTITION BY c.entity_id_2
                                         ORDER BY c.cooccurrence_count DESC) AS rn2
               FROM entity_cooccurrences c
               JOIN entities e1 ON e1.id = c.entity_id_1
               JOIN entities e2 ON e2.id = c.entity_id_2
               WHERE e1.bank_id = ?1 AND e2.bank_id = ?1
             )
             SELECT r.entity_id_1, e2.canonical_name, r.entity_id_2, e1.canonical_name,
                    r.rn1, r.rn2
             FROM ranked r
             JOIN entities e1 ON e1.id = r.entity_id_1
             JOIN entities e2 ON e2.id = r.entity_id_2
             WHERE r.rn1 <= ?2 OR r.rn2 <= ?2",
        )
        .map_err(store_err)?;
    let mut cooccurring: HashMap<i64, HashSet<String>> = HashMap::new();
    let rows = stmt
        .query_map(params![bank_id, MAX_COOCCURRENCE_PARTNERS as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })
        .map_err(store_err)?;
    for row in rows {
        let (id1, name2, id2, name1, rn1, rn2) = row.map_err(store_err)?;
        // A pair can qualify on one side only — take just that side's entry.
        if rn1 <= MAX_COOCCURRENCE_PARTNERS as i64 {
            cooccurring.entry(id1).or_default().insert(name2);
        }
        if rn2 <= MAX_COOCCURRENCE_PARTNERS as i64 {
            cooccurring.entry(id2).or_default().insert(name1);
        }
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
/// names (`memgardend::entities`) plus that fact's own date. Returns
/// `canonical_name -> entity id` for the whole batch.
///
/// `entity_type` is deliberately left NULL: legacy hardcodes `"CONCEPT"` for
/// every LLM-extracted entity (`entity_processing.py:32`) and never reads it
/// back, so persisting a constant would only cost a column.
pub fn write_entities(
    db: &Db,
    bank_id: &str,
    per_node: &[EntityMentions],
    now: i64,
) -> Result<HashMap<String, i64>> {
    // Mentions are counted per occurrence (legacy appends one _EntityStat per
    // resolved mention, `entity_resolver.py:718`), so a name naming two facts
    // in the same chunk bumps mention_count by two. first_seen/last_seen come
    // from the *fact's own* date (`entity_processing.py:28`), not a chunk-wide
    // stamp — the 0.2 temporal term is often what carries a resolution over
    // the 0.6 gate, so a whole chunk sharing one date would distort it.
    let mut mentions: HashMap<&str, (i64, i64, i64)> = HashMap::new();
    for (_, names, seen_at) in per_node {
        for name in names {
            let slot = mentions
                .entry(name.as_str())
                .or_insert((0, *seen_at, *seen_at));
            slot.0 += 1;
            slot.1 = slot.1.min(*seen_at);
            slot.2 = slot.2.max(*seen_at);
        }
    }
    if mentions.is_empty() {
        return Ok(HashMap::new());
    }
    // Deterministic upsert order, same reasoning as the link insert below.
    let mut ordered: Vec<(&str, (i64, i64, i64))> = mentions.into_iter().collect();
    ordered.sort_unstable();

    db.write(|tx| {
        let mut ids: HashMap<String, i64> = HashMap::new();
        for (name, (count, first_seen, last_seen)) in &ordered {
            let id: i64 = tx
                .query_row(
                    "INSERT INTO entities
                       (bank_id, canonical_name, created_at, mention_count, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (bank_id, canonical_name) DO UPDATE SET
                       mention_count = entities.mention_count + excluded.mention_count,
                       first_seen    = min(coalesce(entities.first_seen, excluded.first_seen),
                                           excluded.first_seen),
                       last_seen     = max(coalesce(entities.last_seen, excluded.last_seen),
                                           excluded.last_seen)
                     RETURNING id",
                    params![bank_id, name, now, count, first_seen, last_seen],
                    |r| r.get(0),
                )
                .map_err(store_err)?;
            ids.insert((*name).to_string(), id);
        }

        for (node_id, names, _) in per_node {
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
        let mut pairs: HashMap<(i64, i64), (i64, i64)> = HashMap::new();
        for (_, names, seen_at) in per_node {
            let mut node_ids: Vec<i64> = names.iter().filter_map(|n| ids.get(n).copied()).collect();
            node_ids.sort_unstable();
            node_ids.dedup();
            for (i, a) in node_ids.iter().enumerate() {
                for b in &node_ids[i + 1..] {
                    let slot = pairs.entry((*a, *b)).or_insert((0, *seen_at));
                    slot.0 += 1;
                    slot.1 = slot.1.max(*seen_at);
                }
            }
        }
        let mut pairs: Vec<((i64, i64), (i64, i64))> = pairs.into_iter().collect();
        pairs.sort_unstable(); // deterministic statement order
        for ((a, b), (count, seen_at)) in pairs {
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

/// `(node_id, that node's resolved canonical names, that fact's date in ms)`.
pub type EntityMentions = (i64, Vec<String>, i64);

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
/// `(from_node_id, to_node_id)`** (`link_utils.py:92`, where it buys
/// deadlock-free row-lock ordering). SQLite has no row locks — `BEGIN
/// IMMEDIATE` is one global write lock — so here the sort buys determinism
/// only: the same batch always produces the same statement order, which makes
/// failures reproducible. Kept because it costs one `sort_by_key`. Existing
/// edges are left alone
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
/// per-unit top-N, bounded by MAX_TEMPORAL_WINDOW_NODES. "One session's
/// facts" undersells it — the CE-6 R7 load bench wrote 35,832 nodes in 47
/// seconds, all inside one 24h window, so a backfill or a bulk import can
/// fill this. The pairing itself is O(new × window) with new ≤ one chunk's
/// facts, which is why a flat row ceiling is enough; push the ranking into
/// SQL if the truncation ever costs a link that mattered.`
///
/// Rows come back **newest first** so the truncation drops the far end of the
/// window rather than an arbitrary slice: retain's own facts sit at the
/// recent edge, and the nearest neighbours are the ones the per-node cap
/// keeps anyway.
pub const MAX_TEMPORAL_WINDOW_NODES: usize = 20_000;

pub fn nodes_in_window(
    db: &Db,
    bank_id: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<(i64, String, i64)>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            // idx_memory_nodes_bank_date is (bank_id, event_date DESC), so
            // the ORDER BY is the index order — no sort.
            "SELECT id, fact_type, event_date FROM memory_nodes
             WHERE bank_id = ?1 AND event_date IS NOT NULL AND event_date BETWEEN ?2 AND ?3
             ORDER BY event_date DESC
             LIMIT ?4",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(
            params![bank_id, from_ms, to_ms, MAX_TEMPORAL_WINDOW_NODES as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
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

/// An entity mentioned more than this often stops being a graph edge
/// (`graph_per_entity_limit`, `link_expansion_retrieval.py:8-11`). Both a
/// cost ceiling and a quality one: "ollama", in a bank about MemGarden, is
/// on every node and therefore connects nothing to anything.
pub const MAX_ENTITY_FANOUT: i64 = 200;

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

    // Per-entity fan-out cap, legacy's `graph_per_entity_limit` (200,
    // `link_expansion_retrieval.py:8-11`). The outer LIMIT lands *after* the
    // GROUP BY, so without this a single hub entity makes the self-join
    // seeds x |entity|: measured 10.3ms for one entity naming all 3000 nodes
    // versus 0.10ms on the uniform distribution — alone over the 5ms budget.
    // `mention_count` is the cheap proxy (this migration already adds it, and
    // it is written by the same transaction that writes node_entities); it
    // counts mentions rather than distinct nodes, which only ever makes the
    // gate stricter. A hub that names more than 200 nodes carries no
    // discriminating signal anyway — that is why legacy caps it too.
    let mut stmt = conn
        .prepare(
            "SELECT ne2.node_id, count(DISTINCT ne2.entity_id) AS shared
             FROM json_each(?1) s
             CROSS JOIN node_entities ne1 ON ne1.node_id = s.value
             CROSS JOIN entities pe ON pe.id = ne1.entity_id AND pe.mention_count <= ?4
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
        .query_map(
            params![seeds_json, bank_id, edge_limit as i64, MAX_ENTITY_FANOUT],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
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
/// `since` / `until` bound `event_date`, inclusive, in epoch ms. They are in
/// the query rather than applied to its result because this orders by
/// `id DESC` and takes `limit`: a range narrowed afterwards could only ever
/// cut into the newest window, and could never reach a memory older than it —
/// which is the entire reason to ask for a date (E3).
/// `ids`, when non-empty, replaces the newest-`limit` selection with exactly
/// those nodes — the induced subgraph over a set the caller already holds.
///
/// E3 needs it because `nodes/{id}` answers "what is adjacent to *this* one"
/// and nothing else: an ego view built from it is a star, with no edge drawn
/// between two neighbours that are themselves linked. Walking the graph then
/// shows the path taken and not the fabric around it. The other filters still
/// apply, so a set can be narrowed by type or date on the way back.
pub fn graph_view(
    db: &Db,
    bank_id: &str,
    limit: usize,
    fact_types: &[FactType],
    session: Option<&str>,
    since: Option<i64>,
    until: Option<i64>,
    ids: &[i64],
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
    let ids_filter: Option<String> = if ids.is_empty() {
        None
    } else {
        Some(ids_json(ids))
    };

    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT n.id, n.uuid, n.fact_type, n.text, n.mentioned_at
             FROM memory_nodes n
             WHERE n.bank_id = ?1
               AND (?3 IS NULL OR n.fact_type IN (SELECT value FROM json_each(?3)))
               AND (?4 IS NULL OR EXISTS (
                     SELECT 1 FROM node_tags t WHERE t.node_id = n.id AND t.tag = ?4))
               AND (?5 IS NULL OR n.event_date >= ?5)
               AND (?6 IS NULL OR n.event_date <= ?6)
               AND (?7 IS NULL OR n.id IN (SELECT value FROM json_each(?7)))
             ORDER BY n.id DESC
             LIMIT ?2",
        )
        .map_err(store_err)?;
    let mut nodes = stmt
        .query_map(
            params![
                bank_id,
                limit as i64,
                types_json,
                session_tag,
                since,
                until,
                ids_filter
            ],
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
             ORDER BY from_node_id, to_node_id, link_type
             LIMIT ?2",
        )
        .map_err(store_err)?;
    // Node count alone does not bound the payload: n nodes can carry n^2
    // edges. 50 per node is well past what a readable layout draws.
    let edge_limit = (ids.len() * 50) as i64;
    let edges = stmt
        .query_map(params![ids_json, edge_limit], |r| {
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

/// One node's full text, provenance and adjacency (E1).
///
/// `graph_view` hands the viewer a 160-character label; this is what the
/// viewer calls when the user clicks one. Everything the detail panel shows
/// arrives in one round trip.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeDetail {
    pub node: NodeSummary,
    pub context: Option<String>,
    pub event_date: Option<i64>,
    pub occurred_start: Option<i64>,
    pub occurred_end: Option<i64>,
    pub created_at: i64,
    pub proof_count: i64,
    pub tags: Vec<String>,
    pub entities: Vec<String>,
    /// `node_sources.observation_id = id` — the facts this observation was
    /// consolidated from. Empty for a fact.
    pub sources: Vec<NodeSummary>,
    /// `node_sources.source_id = id` — observations that cite this node.
    /// Indexed by `idx_node_sources_source`, so it costs the same as the
    /// forward direction, and it is what makes a `proof_count` auditable from
    /// the screen instead of from hand-written SQL.
    pub cited_by: Vec<NodeSummary>,
    /// Adjacent nodes by link type, each list ordered by descending weight.
    pub neighbors: Vec<AdjacentNode>,
}

/// A node reduced to what a list row draws. `text` is truncated by the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeSummary {
    pub id: i64,
    pub uuid: String,
    pub fact_type: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdjacentNode {
    pub node: NodeSummary,
    pub link_type: String,
    pub weight: f64,
}

/// Per link type, so one dense class cannot crowd out the others. A migrated
/// node carries ~20 semantic and dozens of temporal edges; the panel lists the
/// strongest and the graph pass (E2) is what shows the rest.
const MAX_NEIGHBORS_PER_TYPE: usize = 20;

/// Provenance in either direction is bounded too: an observation merged from
/// many facts, or a fact cited by many observations, must not return a payload
/// the panel cannot draw.
const MAX_PROVENANCE: usize = 50;

/// `None` when the node does not exist **or** belongs to another bank. The two
/// are deliberately indistinguishable: `bank_id` comes from the URL, so
/// separating them would let a caller probe ids across banks.
pub fn node_detail(db: &Db, bank_id: &str, id: i64) -> Result<Option<NodeDetail>> {
    let conn = db.read()?;

    type NodeRow = (
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
    );
    let row: Option<NodeRow> = conn
        .query_row(
            "SELECT uuid, fact_type, text, context, event_date, occurred_start,
                    occurred_end, created_at, proof_count
             FROM memory_nodes WHERE id = ?1 AND bank_id = ?2",
            params![id, bank_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            },
        )
        .optional()
        .map_err(store_err)?;
    let Some((uuid, fact_type, text, context, event_date, start, end, created_at, proof_count)) =
        row
    else {
        return Ok(None);
    };

    let mut stmt = conn
        .prepare("SELECT tag FROM node_tags WHERE node_id = ?1 ORDER BY tag")
        .map_err(store_err)?;
    let tags = stmt
        .query_map(params![id], |r| r.get::<_, String>(0))
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    let mut stmt = conn
        .prepare(
            "SELECT e.canonical_name FROM node_entities ne
             JOIN entities e ON e.id = ne.entity_id
             WHERE ne.node_id = ?1 ORDER BY e.canonical_name",
        )
        .map_err(store_err)?;
    let entities = stmt
        .query_map(params![id], |r| r.get::<_, String>(0))
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    let provenance = |sql: &str| -> Result<Vec<NodeSummary>> {
        let mut stmt = conn.prepare(sql).map_err(store_err)?;
        let out = stmt
            .query_map(params![id, MAX_PROVENANCE as i64], |r| {
                Ok(NodeSummary {
                    id: r.get(0)?,
                    uuid: r.get(1)?,
                    fact_type: r.get(2)?,
                    text: r.get(3)?,
                })
            })
            .map_err(store_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(store_err)?;
        Ok(out)
    };

    let sources = provenance(
        "SELECT n.id, n.uuid, n.fact_type, n.text FROM node_sources s
         JOIN memory_nodes n ON n.id = s.source_id
         WHERE s.observation_id = ?1 ORDER BY n.id LIMIT ?2",
    )?;
    let cited_by = provenance(
        "SELECT n.id, n.uuid, n.fact_type, n.text FROM node_sources s
         JOIN memory_nodes n ON n.id = s.observation_id
         WHERE s.source_id = ?1 ORDER BY n.id LIMIT ?2",
    )?;

    // **Both directions, unioned.** `links` is keyed
    // `(from_node_id, to_node_id, link_type, entity_id)` and the semantic pass
    // writes edges only *out of* the nodes it was just handed, so a pair
    // embedded in one batch has two rows while a pair spanning batches has one
    // (`embed_task::on_batch_embedded`, and the assertion in
    // `graph_api.rs::a_semantic_link_reaches_a_node_embedded_in_an_earlier_batch`).
    // Reading one direction would make a node's neighbourhood depend on when it
    // happened to be embedded, which is not a property of the memory.
    //
    // Both are index-served: forward on the PK prefix, reverse on
    // `idx_links_to (to_node_id, link_type)`.
    //
    // `MAX(weight)` because the same pair can appear as two rows whose weights
    // were computed in different passes; the strongest is the honest one.
    let mut stmt = conn
        .prepare(
            "SELECT other, link_type, MAX(w) AS weight, uuid, fact_type, text FROM (
               SELECT l.to_node_id AS other, l.link_type AS link_type, l.weight AS w,
                      n.uuid AS uuid, n.fact_type AS fact_type, n.text AS text
                 FROM links l JOIN memory_nodes n ON n.id = l.to_node_id
                WHERE l.from_node_id = ?1
               UNION ALL
               SELECT l.from_node_id, l.link_type, l.weight,
                      n.uuid, n.fact_type, n.text
                 FROM links l JOIN memory_nodes n ON n.id = l.from_node_id
                WHERE l.to_node_id = ?1
             )
             WHERE other != ?1
             GROUP BY other, link_type
             ORDER BY link_type, weight DESC, other",
        )
        .map_err(store_err)?;
    let all = stmt
        .query_map(params![id], |r| {
            Ok(AdjacentNode {
                node: NodeSummary {
                    id: r.get(0)?,
                    uuid: r.get(3)?,
                    fact_type: r.get(4)?,
                    text: r.get(5)?,
                },
                link_type: r.get(1)?,
                weight: r.get(2)?,
            })
        })
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    // Capped per type rather than overall: `ORDER BY link_type, weight DESC`
    // already groups them, so a running count does it without a query per type.
    let mut neighbors: Vec<AdjacentNode> = Vec::new();
    let mut seen_type: Option<String> = None;
    let mut n_of_type = 0usize;
    for neighbor in all {
        if seen_type.as_deref() != Some(neighbor.link_type.as_str()) {
            seen_type = Some(neighbor.link_type.clone());
            n_of_type = 0;
        }
        if n_of_type < MAX_NEIGHBORS_PER_TYPE {
            n_of_type += 1;
            neighbors.push(neighbor);
        }
    }

    Ok(Some(NodeDetail {
        node: NodeSummary {
            id,
            uuid,
            fact_type,
            text,
        },
        context,
        event_date,
        occurred_start: start,
        occurred_end: end,
        created_at,
        proof_count,
        tags,
        entities,
        sources,
        cited_by,
        neighbors,
    }))
}

/// i64s as a JSON array — no injection surface and one statement shape
/// regardless of how many ids there are (same trick as `search::hydrate`).
fn ids_json(ids: &[i64]) -> String {
    format!(
        "[{}]",
        ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    )
}
