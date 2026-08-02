use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use memgarden_core::config::Config;
use memgarden_core::metrics::METRICS;
use memgarden_store::Db;
use memgardend::{embed_task, metrics_task, ollama, routes, state::AppState};

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
    let ollama_client = Arc::new(ollama::OllamaClient::new(cfg.ollama.clone())?);

    // The retain queue lives in memory, so anything left `pending`/`running`
    // by the previous process will never be picked up — close those rows out
    // now rather than leave the Phase C hook's progress view stuck.
    match memgarden_store::retain_jobs::fail_stale(&db, "daemon restarted before the job finished")
    {
        Ok(0) => {}
        Ok(n) => tracing::warn!(count = n, "closed out retain jobs orphaned by a restart"),
        Err(e) => tracing::warn!(error = %e, "failed to close out orphaned retain jobs"),
    }
    // Same for a consolidation round the previous process died inside: its
    // in-memory single-flight guard is gone, but its `running` ledger row is
    // not. The watermark is NULL on those rows so nothing is lost — this only
    // stops `GET /consolidation` reporting a round that cannot finish.
    match memgarden_store::consolidate::fail_stale_runs(
        &db,
        "daemon restarted before the round finished",
    ) {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            count = n,
            "closed out consolidation runs orphaned by a restart"
        ),
        Err(e) => tracing::warn!(error = %e, "failed to close out orphaned consolidation runs"),
    }
    // 21ms one-time cl100k_base init, paid here instead of on the first
    // retain request (decision #5).
    memgardend::retain::warm_tokenizer();

    let (retain_tx, retain_rx) = tokio::sync::mpsc::channel(cfg.retain.queue_capacity);
    let state = AppState {
        db: db.clone(),
        cfg: cfg.clone(),
        started_at_ms,
        embedder: Arc::new(RwLock::new(None)),
        ollama: ollama_client.clone(),
        consolidating: Default::default(),
        retain_tx,
    };

    let app = routes::router(state.clone());
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, "listening");

    let metrics_task_handle = tokio::spawn(metrics_task::run(
        db.clone(),
        cfg.metrics_snapshot_interval_secs,
    ));
    // Spawned *after* the listener binds (decision #1): a first-run model
    // download must not delay the port bind.
    tokio::spawn(embed_task::load_at_startup(state.clone()));
    let embed_backlog_handle = tokio::spawn(embed_task::run_backlog(db.clone(), state.clone()));
    tokio::spawn(ollama::run_prober(ollama_client));
    let consolidation_handle =
        tokio::spawn(memgardend::consolidate::round::run_task(state.clone()));
    let retain_worker_handle = tokio::spawn(memgardend::retain::run_worker(state, retain_rx));

    axum::serve(listener, app)
        .with_graceful_shutdown(memgardend::shutdown_signal())
        .await?;

    if let Err(e) = metrics_task_handle.await {
        tracing::warn!(error = %e, "metrics snapshot task join error during shutdown");
    }
    if let Err(e) = embed_backlog_handle.await {
        tracing::warn!(error = %e, "embed backlog task join error during shutdown");
    }
    if let Err(e) = retain_worker_handle.await {
        tracing::warn!(error = %e, "retain worker join error during shutdown");
    }
    if let Err(e) = consolidation_handle.await {
        tracing::warn!(error = %e, "consolidation task join error during shutdown");
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
