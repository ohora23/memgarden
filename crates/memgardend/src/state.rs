use std::sync::Arc;

use memgarden_core::config::Config;
use memgarden_store::Db;

/// Shared app state, cheap to clone (all fields are `Arc` or `Copy`).
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub cfg: Arc<Config>,
    pub started_at_ms: i64,
}
