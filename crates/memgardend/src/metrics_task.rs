//! Periodic METRICS.snapshot() -> metric_snapshots row, replacing the
//! legacy memdash history.jsonl, plus the HK-1a `sessions` GC.
//! `interval_secs == 0` disables the task entirely; otherwise it ticks until
//! the same shutdown signal the server itself listens for.

use std::sync::Arc;
use std::time::Duration;

use memgarden_core::error::Result;
use memgarden_core::metrics::METRICS;
use memgarden_store::{Db, metrics_store, sessions};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Serializes the current METRICS snapshot, inserts it as a row, and expires
/// stale `sessions` rows. A blocking call (spawn_blocking it from async code)
/// — separated out so tests can call it directly without spinning up the
/// interval loop.
///
/// The GC lives on this tick rather than on its own task because it is one
/// indexed `DELETE` and there is already a timer running: unbounded session
/// accumulation is what pushed legacy into its 10,000-entry truncation hack
/// (`state.py:111-114`), and a slower, simpler answer is enough to prevent it.
///
/// `session_retention_days` is `[hooks] session_retention_days` (C2a). It is
/// a parameter rather than the constant C1 shipped so that the number the CLI
/// documents and the number that reaches this `DELETE` cannot drift apart.
///
/// // ponytail: the GC rides the metrics timer rather than owning one, so
/// // `[metrics] snapshot_interval_secs = 0` disables it too and sessions then
/// // accumulate forever. Acceptable while the coupling is one line and one
/// // process; the upgrade path is its own interval, not a second timer bolted
/// // to the same tick.
pub fn tick(db: &Db, session_retention_days: u64) -> Result<()> {
    let payload = serde_json::to_string(&METRICS.snapshot())
        .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
    metrics_store::insert_snapshot(db, &payload)?;

    let cutoff = memgarden_core::now_ms() - session_retention_days as i64 * DAY_MS;
    let dropped = sessions::gc(db, cutoff)?;
    if dropped > 0 {
        tracing::info!(dropped, cutoff, "expired stale session rows");
    }
    Ok(())
}

/// Runs `tick` every `interval_secs` until shutdown. A tick failure is
/// logged, not fatal — losing one metrics row (or one GC pass) must never
/// take the daemon down.
///
/// `interval_secs == 0` disables the task, and with it the session GC. See
/// `tick`.
pub async fn run(db: Arc<Db>, interval_secs: u64, session_retention_days: u64) {
    if interval_secs == 0 {
        return;
    }
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    // Pinned once, outside the loop: awaiting `crate::shutdown_signal()`
    // fresh inside each `select!` would create (and immediately drop) a new
    // listener every tick, so a signal arriving mid-tick could be missed
    // entirely instead of breaking the loop.
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let db = db.clone();
                match tokio::task::spawn_blocking(move || tick(&db, session_retention_days)).await {
                    Ok(Ok(())) => {}
                    // `tick` does the snapshot AND the session GC, so the message
                    // names the tick rather than guessing which half failed.
                    Ok(Err(e)) => tracing::warn!(error = %e, "metrics tick failed"),
                    Err(e) => tracing::warn!(error = %e, "metrics tick panicked"),
                }
            }
            _ = &mut shutdown => break,
        }
    }
}
