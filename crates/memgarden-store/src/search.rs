use std::str::FromStr;

use rusqlite::params;

use memgarden_core::EMBEDDING_MODEL_ID;
use memgarden_core::error::Result;
use memgarden_core::types::FactType;

use crate::{Db, store_err, vecblob};

/// Query terms kept by `fts_query_string`, longest first (Critic Revision
/// R1). Real prompts run long; every extra `OR` term widens the candidate
/// pool for no precision gain, and FTS5 itself caps an expression at
/// SQLITE_MAX_EXPR_DEPTH. Longer terms are the more selective ones.
pub const MAX_QUERY_TERMS: usize = 12;

/// Builds an FTS5 MATCH expression from raw user input: splits on
/// non-alphanumeric boundaries (this doubles as escaping — no FTS5 special
/// characters survive the split), keeps the `MAX_QUERY_TERMS` longest terms,
/// and appends `*` to each for prefix matching against the `prefix='2 3 4'`
/// index. Consumed by recall (CE-6).
///
/// Terms are joined with **` OR `**, not whitespace (Critic Revision R1).
/// FTS5 reads a bare space as an implicit AND, which measured 0 hits for any
/// realistic multi-token prompt (16-token English: AND 0 / OR 100; 5-token
/// Korean: AND 0). Ranking still favours documents matching more terms —
/// that is what `bm25()` is for — so OR loses nothing but the empty result.
pub fn fts_query_string(raw: &str) -> String {
    let mut terms: Vec<&str> = raw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .collect();

    if terms.len() > MAX_QUERY_TERMS {
        // Rank by length (stable, so equal-length terms keep query order),
        // take the top N, then restore query order for a readable expression.
        let mut ranked: Vec<usize> = (0..terms.len()).collect();
        ranked.sort_by_key(|&i| std::cmp::Reverse(terms[i].chars().count()));
        ranked.truncate(MAX_QUERY_TERMS);
        ranked.sort_unstable();
        terms = ranked.into_iter().map(|i| terms[i]).collect();
    }

    terms
        .iter()
        // Quoted: FTS5 lexes bare uppercase AND/OR/NOT as operators even
        // though they survive the alphanumeric split ("cats AND dogs" →
        // syntax error). A quoted string is always a phrase term, and `"`
        // itself cannot survive the split, so this closes the injection.
        .map(|tok| format!("\"{tok}\"*"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Full-text candidate node ids for `bank_id`, ranked by BM25 (best first).
/// `match_query` is an FTS5 MATCH expression — see `fts_query_string`.
pub fn fts_candidates(db: &Db, bank_id: &str, match_query: &str, limit: usize) -> Result<Vec<i64>> {
    Ok(
        fts_candidates_filtered(db, bank_id, match_query, &[], limit)?
            .into_iter()
            .map(|(id, _)| id)
            .collect(),
    )
}

/// `fts_candidates` plus a `fact_type` restriction and the raw `bm25()`
/// score (more negative = better match; the recall arm keeps it for
/// `scores.keyword`). An empty `fact_types` means no type filter.
///
/// Tag filtering is deliberately NOT here — see `recall::filter` in the
/// daemon: the vector arm cannot filter tags in SQL (vec0 partitions on
/// `bank_id` only), so keeping one Rust implementation for both arms beats
/// two dialects of the same four-mode semantics.
/// `// ponytail: post-filter + over-fetch; push tags into SQL if a
/// tag-narrow recall starts under-returning at scale.`
pub fn fts_candidates_filtered(
    db: &Db,
    bank_id: &str,
    match_query: &str,
    fact_types: &[FactType],
    limit: usize,
) -> Result<Vec<(i64, f64)>> {
    if match_query.is_empty() {
        // `MATCH ''` is a SQLite/FTS5 syntax error, not "no results" — an
        // empty query (e.g. from fts_query_string on punctuation-only
        // input) has no candidates by definition, so short-circuit.
        return Ok(vec![]);
    }
    // A JSON array rather than dynamically built `IN (?,?,?)` placeholders:
    // one prepared statement shape for every filter combination. The values
    // are `FactType::as_str` literals, never user input.
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

    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT memory_nodes_fts.rowid, bm25(memory_nodes_fts)
             FROM memory_nodes_fts
             JOIN memory_nodes ON memory_nodes.id = memory_nodes_fts.rowid
             WHERE memory_nodes_fts MATCH ?1 AND memory_nodes.bank_id = ?2
               AND (?4 IS NULL
                    OR memory_nodes.fact_type IN (SELECT value FROM json_each(?4)))
             ORDER BY bm25(memory_nodes_fts)
             LIMIT ?3",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(
            params![match_query, bank_id, limit as i64, types_json],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)),
        )
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// The CE-8 temporal recall arm's entry predicate: nodes whose effective
/// time falls inside the query's constraint, most recent first (the arm feeds
/// RRF, which reads position only).
///
/// The COALESCE order here — `occurred_start ?? mentioned_at` — is the
/// **second** of the three orders in this codebase and is deliberately not
/// the same as the other two; see `recall::scoring` for the full list and the
/// test that pins them apart.
///
/// `event_date` is never in this predicate. It exists for temporal *link*
/// creation (`links::temporal`) and nothing else; filtering entries on it
/// would silently exclude every node whose occurred/mentioned pair disagrees
/// with it.
///
/// `// ponytail: the COALESCE defeats idx_memory_nodes_occurred, so this is a
/// bank-partition scan — measured well under the 3ms arm budget at 3k nodes.
/// Upgrade path if a bank gets large: a stored generated column
/// `effective_at` with its own (bank_id, effective_at) index.`
pub fn temporal_candidates(
    db: &Db,
    bank_id: &str,
    start_ms: i64,
    end_ms: i64,
    limit: usize,
) -> Result<Vec<i64>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT id FROM memory_nodes
             WHERE bank_id = ?1
               AND coalesce(occurred_start, mentioned_at) BETWEEN ?2 AND ?3
             ORDER BY coalesce(occurred_start, mentioned_at) DESC
             LIMIT ?4",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![bank_id, start_ms, end_ms, limit as i64], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// Everything the recall pipeline needs about a candidate node, fetched for
/// the whole fused id set in two queries (no N+1): the union of both arms is
/// typically 100-200 ids and this measures in the low tens of microseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRow {
    pub id: i64,
    pub uuid: String,
    pub fact_type: FactType,
    pub text: String,
    pub context: Option<String>,
    pub occurred_start: Option<i64>,
    pub occurred_end: Option<i64>,
    pub mentioned_at: Option<i64>,
    pub tags: Vec<String>,
    /// Distinct source facts backing an observation (CE-9a). 0 for every
    /// other fact type, which `recall::scoring::proof_norm` reads as neutral.
    pub proof_count: i64,
    /// `node_sources.source_id` — the facts this observation was consolidated
    /// from. Empty for anything that is not a sourced observation.
    ///
    /// Loaded here rather than in a later pass because recall needs it to
    /// avoid injecting one fact twice (an observation and the source it
    /// restates can both rank), and `hydrate` is the one place every
    /// candidate passes through — including the ones the graph arm adds.
    /// Fetching it later would cost a whole extra `spawn_blocking` on the hot
    /// path, which this module's own comments warn against.
    pub sources: Vec<i64>,
}

/// Loads `CandidateRow`s for `ids` **within `bank_id`**, in arbitrary order
/// (callers index by id). Unknown or other-bank ids are silently absent —
/// the bank predicate is defence in depth: both arms already scope to the
/// bank, so an id from another bank reaching here would be a bug, and this
/// makes that bug return nothing instead of leaking a row.
///
/// Tags come back from a **second** query rather than a `group_concat`: any
/// separator character can legally appear inside a tag, and an ambiguous
/// split is a data-integrity bug waiting for the one tag that contains it.
/// Two statements, still no N+1. `ORDER BY` makes the tag order
/// deterministic, so response bodies and test fixtures are stable.
pub fn hydrate(db: &Db, bank_id: &str, ids: &[i64]) -> Result<Vec<CandidateRow>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    // i64s formatted into a JSON array — no injection surface, and one
    // statement shape regardless of how many ids the fusion produced.
    let ids_json = format!(
        "[{}]",
        ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    );

    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT n.id, n.uuid, n.fact_type, n.text, n.context,
                    n.occurred_start, n.occurred_end, n.mentioned_at, n.proof_count
             FROM memory_nodes n
             WHERE n.id IN (SELECT value FROM json_each(?1)) AND n.bank_id = ?2",
        )
        .map_err(store_err)?;
    let raw = stmt
        .query_map(params![ids_json, bank_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })
        .map_err(store_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)?;

    let mut stmt = conn
        .prepare(
            "SELECT node_id, tag FROM node_tags
             WHERE node_id IN (SELECT value FROM json_each(?1))
             ORDER BY node_id, tag",
        )
        .map_err(store_err)?;
    let mut tags_by_node: std::collections::HashMap<i64, Vec<String>> =
        std::collections::HashMap::new();
    let tag_rows = stmt
        .query_map(params![ids_json], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(store_err)?;
    for row in tag_rows {
        let (node_id, tag) = row.map_err(store_err)?;
        tags_by_node.entry(node_id).or_default().push(tag);
    }

    // Same shape as the tag pass above, and cheap for the same reason: the
    // `node_sources` primary key serves it. Measured at 0.078 ms for 400 ids
    // on the live database.
    let mut stmt = conn
        .prepare(
            "SELECT observation_id, source_id FROM node_sources
             WHERE observation_id IN (SELECT value FROM json_each(?1))
             ORDER BY observation_id, source_id",
        )
        .map_err(store_err)?;
    let mut sources_by_node: std::collections::HashMap<i64, Vec<i64>> =
        std::collections::HashMap::new();
    let source_rows = stmt
        .query_map(params![ids_json], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(store_err)?;
    for row in source_rows {
        let (observation_id, source_id) = row.map_err(store_err)?;
        sources_by_node
            .entry(observation_id)
            .or_default()
            .push(source_id);
    }

    raw.into_iter()
        .map(
            |(id, uuid, fact_type, text, context, start, end, mentioned, proof_count)| {
                Ok(CandidateRow {
                    id,
                    uuid,
                    fact_type: FactType::from_str(&fact_type)?,
                    text,
                    context,
                    occurred_start: start,
                    occurred_end: end,
                    mentioned_at: mentioned,
                    tags: tags_by_node.remove(&id).unwrap_or_default(),
                    proof_count,
                    sources: sources_by_node.remove(&id).unwrap_or_default(),
                })
            },
        )
        .collect()
}

/// K-nearest-neighbor node ids (with cosine distance, best first) for
/// `bank_id`, via the `vec_nodes` vec0 partitioned index. Brute-force
/// (no ANN) — fine at current scale, see Trade-offs in the plan.
///
/// `// ponytail: brute-force scan of the whole bank partition. Measured on
/// the CE-6 bench: recall p95 9.7ms at 3k nodes, 40.7ms at ~32k against a
/// 60ms budget — this scan is most of that growth. Upgrade path when a bank
/// approaches ~50k nodes: sqlite-vec ANN once it ships, or pre-filter the
/// partition (e.g. a fact_type/recency-scoped shadow vec0 table) so the
/// scan is over a slice rather than the bank.`
///
/// **Only vectors from the active producer are returned** (AX-1). `vec_nodes`
/// carries no producer of its own — it is a derived index over
/// `memory_nodes.embedding` — so the tag is read back off the source row
/// through the rowid, which is that table's `INTEGER PRIMARY KEY`. A cosine
/// distance between two embedding spaces is a number with no meaning; a row
/// excluded here is still fully reachable through FTS/BM25, the graph arm and
/// hydrate, which is what makes hybrid recall the migration strategy rather
/// than a re-embed.
///
/// The filter is applied **after** the vec0 scan picks its top `k`, so a bank
/// holding foreign-producer vectors yields fewer than `k` dense candidates
/// rather than reaching deeper for `k` matching ones.
/// `// ponytail: post-filter, so a bank that is mostly foreign vectors gets a
/// thin — worst case empty — dense arm: if the top-k by raw distance are all
/// foreign, comparable-space matches just below the cut are missed.
/// Acceptable while the only producer is ours (every row is
/// tagged by 0005's backfill). Upgrade path if MG-1 lands a mixed bank:
/// sqlite-vec's rowid-IN prefilter, or `embedding_model` as a second vec0
/// partition key, either of which returns a full k.`
pub fn knn(db: &Db, bank_id: &str, query_embedding: &[f32], k: usize) -> Result<Vec<(i64, f64)>> {
    let blob = vecblob::encode(query_embedding)?;
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT v.rowid, v.distance FROM vec_nodes v
             JOIN memory_nodes n ON n.id = v.rowid
             WHERE v.bank_id = ?1 AND v.embedding MATCH ?2 AND v.k = ?3
               AND n.embedding_model = ?4
             ORDER BY v.distance",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![bank_id, blob, k as i64, EMBEDDING_MODEL_ID], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        })
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// Rebuilds `vec_nodes` from `memory_nodes.embedding` (the source of truth,
/// `0001_init.sql:81-85`) — the CE-2 deferral. Optionally scoped to one
/// bank. Deletes the (scoped) index in one transaction, then re-inserts in
/// chunks of 500, **committing per chunk** (NIT 17) so a large rebuild
/// doesn't hold the write lock for the whole operation. Returns the number
/// of rows reinserted.
pub fn rebuild_vec_index(db: &Db, bank_id: Option<&str>) -> Result<usize> {
    const CHUNK: usize = 500;

    let rows: Vec<(i64, String, Vec<u8>)> = {
        let conn = db.read()?;
        let mapped = |r: &rusqlite::Row| -> rusqlite::Result<(i64, String, Vec<u8>)> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        };
        let collected = if let Some(b) = bank_id {
            let mut stmt = conn
                .prepare(
                    "SELECT id, bank_id, embedding FROM memory_nodes
                     WHERE bank_id = ?1 AND embedding IS NOT NULL",
                )
                .map_err(store_err)?;
            stmt.query_map(params![b], mapped)
                .map_err(store_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT id, bank_id, embedding FROM memory_nodes WHERE embedding IS NOT NULL",
                )
                .map_err(store_err)?;
            stmt.query_map([], mapped)
                .map_err(store_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
        };
        collected.map_err(store_err)?
    };

    db.write(|tx| {
        match bank_id {
            Some(b) => tx.execute("DELETE FROM vec_nodes WHERE bank_id = ?1", params![b]),
            None => tx.execute("DELETE FROM vec_nodes", []),
        }
        .map_err(store_err)?;
        Ok(())
    })?;

    let mut rebuilt = 0usize;
    for chunk in rows.chunks(CHUNK) {
        db.write(|tx| {
            for (id, bank, blob) in chunk {
                tx.execute(
                    "INSERT INTO vec_nodes (rowid, bank_id, embedding) VALUES (?1, ?2, ?3)",
                    params![id, bank, blob],
                )
                .map_err(store_err)?;
            }
            Ok(())
        })?;
        rebuilt += chunk.len();
    }
    Ok(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_string_tokenizes_and_appends_star() {
        assert_eq!(fts_query_string("데몬"), "\"데몬\"*");
        // R1: OR, not whitespace-AND.
        assert_eq!(fts_query_string("hello world"), "\"hello\"* OR \"world\"*");
        assert_eq!(fts_query_string("  foo,bar  "), "\"foo\"* OR \"bar\"*");
    }

    /// Review HIGH: FTS5 lexes bare uppercase `AND`/`OR`/`NOT` as operators.
    /// They are alphanumeric, so the tokenizer split leaves them intact —
    /// unquoted, `cats AND dogs` became `cats* OR AND* OR dogs*`, an FTS5
    /// syntax error (a 500 on any prompt containing a capitalised AND/OR/NOT).
    /// Quoting makes every term a phrase, which is never an operator.
    #[test]
    fn fts_query_string_neutralizes_fts5_keywords() {
        assert_eq!(
            fts_query_string("cats AND dogs"),
            "\"cats\"* OR \"AND\"* OR \"dogs\"*"
        );
        assert_eq!(fts_query_string("a OR b"), "\"a\"* OR \"OR\"* OR \"b\"*");
        assert_eq!(
            fts_query_string("NOT NEAR cats"),
            "\"NOT\"* OR \"NEAR\"* OR \"cats\"*"
        );
        // The quote character itself is stripped by the alphanumeric split
        // (it is a token boundary, like any other punctuation), so a term
        // can never terminate its own quoting.
        assert_eq!(
            fts_query_string("say \"hi\" now"),
            "\"say\"* OR \"hi\"* OR \"now\"*"
        );
    }

    #[test]
    fn fts_query_string_keeps_longest_terms() {
        // 14 terms -> the 12 longest survive, in original query order.
        let raw = "a bb ccc dddd eeeee ffffff ggggggg hhhhhhhh iiiiiiiii \
                   jjjjjjjjjj kkkkkkkkkkk llllllllllll mmmmmmmmmmmmm nnnnnnnnnnnnnn";
        let q = fts_query_string(raw);
        assert_eq!(q.matches(" OR ").count(), MAX_QUERY_TERMS - 1);
        assert!(
            !q.contains("\"a\"*"),
            "the two shortest terms must be dropped: {q}"
        );
        assert!(!q.contains("\"bb\"*"));
        assert!(
            q.starts_with("\"ccc\"*"),
            "surviving terms keep query order: {q}"
        );
        assert!(q.ends_with("\"nnnnnnnnnnnnnn\"*"));
    }

    /// The temporal arm's SQL, exercised for real: the second COALESCE order
    /// (`occurred_start ?? mentioned_at`), inclusive boundaries, and the
    /// promise that `event_date` is never part of the predicate.
    #[test]
    fn temporal_candidates_range_boundaries_and_coalesce_order() {
        use crate::models::NewNode;
        use memgarden_core::types::FactType;

        let db = Db::open_memory().unwrap();
        crate::banks::create(&db, "b1", None, None).unwrap();
        crate::banks::create(&db, "b2", None, None).unwrap();

        const DAY: i64 = 86_400_000;
        let (lo, hi) = (10 * DAY, 20 * DAY);
        let node = |bank: &str,
                    label: &str,
                    start: Option<i64>,
                    mentioned: Option<i64>,
                    event: Option<i64>| {
            let mut n = NewNode::new(bank, FactType::World, label);
            n.occurred_start = start;
            n.mentioned_at = mentioned;
            n.event_date = event;
            crate::nodes::insert(&db, n).unwrap()
        };
        let at_lo = node("b1", "on the lower bound", Some(lo), None, None);
        let at_hi = node("b1", "on the upper bound", Some(hi), None, None);
        let inside = node("b1", "well inside", Some(15 * DAY), None, None);
        let by_mentioned = node("b1", "no occurred_start", None, Some(16 * DAY), None);
        // occurred_start wins over mentioned_at: this one is OUT even though
        // its mentioned_at sits in the middle of the window.
        let start_out = node("b1", "start outside", Some(30 * DAY), Some(15 * DAY), None);
        // event_date inside, everything else outside: must NOT be selected.
        let only_event = node(
            "b1",
            "event_date only",
            None,
            Some(40 * DAY),
            Some(15 * DAY),
        );
        let below = node("b1", "one ms below", Some(lo - 1), None, None);
        let above = node("b1", "one ms above", Some(hi + 1), None, None);
        let other_bank = node("b2", "another bank", Some(15 * DAY), None, None);
        let dateless = node("b1", "no dates at all", None, None, None);

        let hits = temporal_candidates(&db, "b1", lo, hi, 100).unwrap();
        assert_eq!(
            hits,
            vec![at_hi, by_mentioned, inside, at_lo],
            "most recent first, both bounds inclusive"
        );
        for excluded in [start_out, only_event, below, above, other_bank, dateless] {
            assert!(
                !hits.contains(&excluded),
                "node {excluded} must be excluded"
            );
        }
        // The limit keeps the newest, not the first inserted.
        assert_eq!(
            temporal_candidates(&db, "b1", lo, hi, 1).unwrap(),
            vec![at_hi]
        );
    }

    #[test]
    fn empty_fts_query_no_error() {
        let db = Db::open_memory().unwrap();
        crate::banks::create(&db, "b1", None, None).unwrap();
        // Punctuation-only input tokenizes to an empty match string; that
        // must return no candidates, not a `MATCH ''` syntax error.
        assert_eq!(fts_query_string("!!!"), "");
        assert_eq!(
            fts_candidates(&db, "b1", "", 10).unwrap(),
            Vec::<i64>::new()
        );
    }
}
