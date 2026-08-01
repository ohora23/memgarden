use rusqlite::params;

use memgarden_core::error::Result;

use crate::{Db, store_err, vecblob};

/// Builds an FTS5 MATCH expression from raw user input: splits on
/// non-alphanumeric boundaries (this doubles as escaping — no FTS5 special
/// characters survive the split) and appends `*` to every term for prefix
/// matching against the `prefix='2 3 4'` index. Consumed by recall (CE-6).
pub fn fts_query_string(raw: &str) -> String {
    raw.split(|c: char| !c.is_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("{tok}*"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Full-text candidate node ids for `bank_id`, ranked by BM25 (best first).
/// `match_query` is an FTS5 MATCH expression — see `fts_query_string`.
pub fn fts_candidates(db: &Db, bank_id: &str, match_query: &str, limit: usize) -> Result<Vec<i64>> {
    let conn = db.read()?;
    let mut stmt = conn
        .prepare(
            "SELECT memory_nodes_fts.rowid
             FROM memory_nodes_fts
             JOIN memory_nodes ON memory_nodes.id = memory_nodes_fts.rowid
             WHERE memory_nodes_fts MATCH ?1 AND memory_nodes.bank_id = ?2
             ORDER BY bm25(memory_nodes_fts)
             LIMIT ?3",
        )
        .map_err(store_err)?;
    let rows = stmt
        .query_map(params![match_query, bank_id, limit as i64], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(store_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(store_err)
}

/// K-nearest-neighbor node ids (with cosine distance, best first) for
/// `bank_id`, via the `vec_nodes` vec0 partitioned index. Brute-force
/// (no ANN) — fine at current scale, see Trade-offs in the plan.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_string_tokenizes_and_appends_star() {
        assert_eq!(fts_query_string("데몬"), "데몬*");
        assert_eq!(fts_query_string("hello world"), "hello* world*");
        assert_eq!(fts_query_string("  foo,bar  "), "foo* bar*");
    }
}
