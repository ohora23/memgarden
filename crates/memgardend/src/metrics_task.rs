//! Periodic METRICS.snapshot() -> metric_snapshots row, replacing the
//! legacy memdash history.jsonl. `interval_secs == 0` disables the task
//! entirely; otherwise it ticks until the same shutdown signal the server
//! itself listens for.

use std::sync::Arc;
use std::time::Duration;

use memgarden_core::error::Result;
use memgarden_core::metrics::METRICS;
use memgarden_store::{Db, metrics_store};

/// Serializes the current METRICS snapshot and inserts it as a row. A
/// blocking call (spawn_blocking it from async code) — separated out so
/// tests can call it directly without spinning up the interval loop.
pub fn tick(db: &Db) -> Result<()> {
    let payload = serde_json::to_string(&METRICS.snapshot())
        .map_err(|e| memgarden_core::Error::Storage(e.to_string()))?;
    metrics_store::insert_snapshot(db, &payload)
}

/// Runs `tick` every `interval_secs` until shutdown. A snapshot failure is
/// logged, not fatal — losing one metrics row must never take the daemon
/// down.
pub async fn run(db: Arc<Db>, interval_secs: u64) {
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
                match tokio::task::spawn_blocking(move || tick(&db)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(error = %e, "metrics snapshot insert failed"),
                    Err(e) => tracing::warn!(error = %e, "metrics snapshot task panicked"),
                }
            }
            _ = &mut shutdown => break,
        }
    }
}
