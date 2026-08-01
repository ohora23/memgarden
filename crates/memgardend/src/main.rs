use std::sync::Arc;
use std::sync::atomic::Ordering;

use memgarden_core::config::Config;
use memgarden_core::metrics::METRICS;
use memgarden_store::Db;
use memgardend::{metrics_task, routes, state::AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    init_tracing(&cfg.log_level);

    tracing::info!(bind = %cfg.bind, db_path = %cfg.db_path.display(), "starting memgardend");

    let db = Arc::new(Db::open(&cfg.db_path)?);
    let cfg = Arc::new(cfg);
    let started_at_ms = memgarden_core::now_ms();
    METRICS
        .started_at_ms
        .store(started_at_ms as u64, Ordering::Relaxed);
    let state = AppState {
        db: db.clone(),
        cfg: cfg.clone(),
        started_at_ms,
    };

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, "listening");

    let metrics_task_handle = tokio::spawn(metrics_task::run(
        db.clone(),
        cfg.metrics_snapshot_interval_secs,
    ));

    axum::serve(listener, app)
        .with_graceful_shutdown(memgardend::shutdown_signal())
        .await?;

    let _ = metrics_task_handle.await;

    tracing::info!("shutting down: checkpointing WAL");
    let checkpoint_db = db.clone();
    let _ = tokio::task::spawn_blocking(move || -> memgarden_core::error::Result<()> {
        let conn = checkpoint_db.read()?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|e| memgarden_core::Error::Storage(e.to_string()))
    })
    .await;
    drop(db);

    Ok(())
}

fn init_tracing(cfg_level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cfg_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
