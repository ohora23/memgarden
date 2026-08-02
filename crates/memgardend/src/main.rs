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

    let db = Arc::new(open_db_secured(&cfg.db_path)?);
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

    if let Err(e) = metrics_task_handle.await {
        tracing::warn!(error = %e, "metrics snapshot task join error during shutdown");
    }

    tracing::info!("shutting down: checkpointing WAL");
    let checkpoint_db = db.clone();
    let checkpoint_result =
        tokio::task::spawn_blocking(move || -> memgarden_core::error::Result<()> {
            let conn = checkpoint_db.read()?;
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .map_err(|e| memgarden_core::Error::Storage(e.to_string()))
        })
        .await;
    match checkpoint_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "WAL checkpoint failed"),
        Err(e) => tracing::warn!(error = %e, "WAL checkpoint task panicked"),
    }
    drop(db);

    Ok(())
}

/// Fresh-install-safe DB open: ensures `db_path`'s parent directory exists
/// (mode 0700, via `paths::ensure_data_dir`) *before* `Db::open` — SQLite
/// can't create a missing directory itself — then locks the db file itself
/// down to mode 0600 once it exists.
fn open_db_secured(db_path: &std::path::Path) -> anyhow::Result<Db> {
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        memgarden_core::paths::ensure_data_dir(parent)?;
    }
    let db = Db::open(db_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(db_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(db)
}

fn init_tracing(cfg_level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cfg_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_install_creates_dir_and_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("newdir").join("x.db");

        let _db = open_db_secured(&db_path).unwrap();

        assert!(db_path.parent().unwrap().is_dir());
        assert!(db_path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
