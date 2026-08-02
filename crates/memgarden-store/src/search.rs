use std::str::FromStr;

use rusqlite::params;

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
                    n.occurred_start, n.occurred_end, n.mentioned_at
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

    raw.into_iter()
        .map(
            |(id, uuid, fact_type, text, context, start, end, mentioned)| {
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
pub fn knn(db: &Db, bank_id: &str, query_embedding: &[f32], k: usize) -> Result<Vec<(i64, f64)>> {
    let blob = vecblob::encode(query_embedding)?;
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT rowid, distance FROM vec_nodes
             WHERE bank_id = ?1 AND embedding MATCH ?2 AND k = ?3
             ORDER BY distance",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![bank_id, blob, k as i64], |r| {
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
