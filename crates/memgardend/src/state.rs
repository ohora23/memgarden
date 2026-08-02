use std::sync::{Arc, RwLock};

use memgarden_core::config::Config;
use memgarden_store::Db;
use tokio::sync::mpsc;

use crate::embed::Embedder;
use crate::ollama::OllamaClient;
use crate::retain::RetainTask;

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
    /// Always present (unlike `embedder`): building a `reqwest::Client`
    /// touches no network, so there's no loading state to model. Actual
    /// reachability lives in `ollama::ollama_status()`, updated by the
    /// background prober.
    pub ollama: Arc<OllamaClient>,
    /// Bounded queue into the background retain worker
    /// (`retain::run_worker`). Capacity is `retain.queue_capacity`; the
    /// endpoint reserves a slot with `try_reserve` and answers 429 when the
    /// queue is full rather than buffering transcripts in RAM.
    pub retain_tx: mpsc::Sender<RetainTask>,
}
