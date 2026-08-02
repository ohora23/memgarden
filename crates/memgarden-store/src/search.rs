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
    if match_query.is_empty() {
        // `MATCH ''` is a SQLite/FTS5 syntax error, not "no results" — an
        // empty query (e.g. from fts_query_string on punctuation-only
        // input) has no candidates by definition, so short-circuit.
        return Ok(vec![]);
    }
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
        assert_eq!(fts_query_string("데몬"), "데몬*");
        assert_eq!(fts_query_string("hello world"), "hello* world*");
        assert_eq!(fts_query_string("  foo,bar  "), "foo* bar*");
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
