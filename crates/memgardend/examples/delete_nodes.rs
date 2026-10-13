//! Deletes memory nodes by id through the store's own `nodes::delete`, so the
//! FTS5 and `vec0` triggers and the four `ON DELETE CASCADE` tables all fire
//! exactly as they do in the daemon. Raw SQL from outside cannot do this: the
//! `memory_nodes_vec_ad` trigger touches a `vec0` virtual table that only the
//! app's connection has the extension for.
//!
//! Reads ids from a JSON file: `{"delete": [1, 2, 3]}`.
//! Dry run by default; pass `--apply` to actually delete.
//!
//! Optional `"pairs": [{"delete": 1, "keep": 2}, ...]` marks a deletion as a
//! duplicate of a kept node: before the delete, every observation citing the
//! deleted node is re-pointed at the kept one, so the `node_sources` CASCADE
//! does not strip the observation of that evidence. Same transaction as the
//! delete.
//!
//!   cargo run -p memgardend --example delete_nodes -- <plan.json> [--apply]

use memgarden_store::{Db, nodes};
use rusqlite::params;
use serde_json::Value;
use std::collections::HashMap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let apply = args.iter().any(|a| a == "--apply");
    let plan_path = args
        .iter()
        .find(|a| a.ends_with(".json"))
        .expect("give a plan .json");

    let plan: Value = serde_json::from_str(&std::fs::read_to_string(plan_path).expect("read plan"))
        .expect("json");
    let ids: Vec<i64> = plan["delete"]
        .as_array()
        .expect("plan.delete must be an array")
        .iter()
        .map(|v| v.as_i64().expect("id must be an integer"))
        .collect();

    let keep_for: HashMap<i64, i64> = plan["pairs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p["delete"].as_i64()?, p["keep"].as_i64()?)))
                .collect()
        })
        .unwrap_or_default();

    let db_path = memgarden_core::config::Config::load()
        .expect("load config")
        .db_path;
    println!("database: {}", db_path.display());
    let db = Db::open(&db_path).expect("open db");

    let before = count(&db);
    println!("nodes before: {before}");
    println!("ids to delete: {}", ids.len());
    println!("re-point citations for: {}", keep_for.len());

    if !apply {
        println!("DRY RUN — pass --apply to delete");
        return;
    }

    let mut done = 0usize;
    let mut missing = 0usize;
    let mut repointed = 0usize;
    for id in &ids {
        if let Some(keep) = keep_for.get(id) {
            let moved = db.write(|tx| {
                let n = tx
                    .execute(
                        "INSERT OR IGNORE INTO node_sources (observation_id, source_id, created_at)
                         SELECT observation_id, ?2, created_at FROM node_sources WHERE source_id = ?1",
                        params![id, keep],
                    )
                    .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
                tx.execute("DELETE FROM memory_nodes WHERE id = ?1", params![id])
                    .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
                Ok(n)
            });
            match moved {
                Ok(n) => {
                    repointed += n;
                    done += 1;
                }
                Err(e) => {
                    missing += 1;
                    eprintln!("  id {id}: {e}");
                }
            }
            continue;
        }
        match nodes::delete(&db, *id) {
            Ok(()) => done += 1,
            Err(e) => {
                missing += 1;
                eprintln!("  id {id}: {e}");
            }
        }
    }
    let after = count(&db);
    println!("deleted calls ok: {done}, errors: {missing}, citations re-pointed: {repointed}");
    println!("nodes after: {after}  (delta {})", after - before);
}

fn count(db: &Db) -> i64 {
    let conn = db.read().expect("read conn");
    conn.query_row("SELECT count(*) FROM memory_nodes", [], |r| r.get(0))
        .expect("count")
}
