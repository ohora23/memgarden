pub mod consolidate;
pub mod embed;
pub mod embed_task;
pub mod entities;
pub mod error;
pub mod extract;
pub mod json;
pub mod links;
pub mod metrics_task;
pub mod middleware;
pub mod ollama;
pub mod recall;
pub mod retain;
pub mod routes;
pub mod state;
pub mod temporal;

/// Resolves when either Ctrl+C or SIGTERM is received. Each call installs
/// its own listener, so the server and the metrics snapshot task can each
/// await their own copy independently and both exit on the same signal.
pub async fn shutdown_signal() {
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
