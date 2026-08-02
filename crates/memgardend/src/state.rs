use std::sync::{Arc, RwLock};

use memgarden_core::config::Config;
use memgarden_store::Db;

use crate::embed::Embedder;

/// Shared app state, cheap to clone (all fields are `Arc` or `Copy`).
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub cfg: Arc<Config>,
    pub started_at_ms: i64,
    /// `None` until the startup loader (spawned after the listener binds —
    /// see main.rs / decision #1) finishes; `embed::embed_status()` carries
    /// the more granular loading/ready/disabled/error state `/healthz`
    /// reports. A `std::sync::RwLock` is enough here — the critical section
    /// is just cloning an `Arc`, never held across an `.await`.
    pub embedder: Arc<RwLock<Option<Arc<Embedder>>>>,
}
