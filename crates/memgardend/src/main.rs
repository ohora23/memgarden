use std::sync::Arc;

use memgarden_core::config::Config;
use memgarden_store::Db;
use memgardend::{routes, state::AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    init_tracing(&cfg.log_level);

    tracing::info!(bind = %cfg.bind, db_path = %cfg.db_path.display(), "starting memgardend");

    let db = Arc::new(Db::open(&cfg.db_path)?);
    let cfg = Arc::new(cfg);
    let state = AppState {
        db: db.clone(),
        cfg: cfg.clone(),
        started_at_ms: memgarden_core::now_ms(),
    };

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

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

/// Resolves when either Ctrl+C or SIGTERM is received.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
