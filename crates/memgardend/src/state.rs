use std::sync::{Arc, RwLock};

use memgarden_core::config::Config;
use memgarden_store::Db;
use tokio::sync::mpsc;

use crate::embed::Embedder;
use crate::ollama::OllamaClient;
use crate::rerank::Reranker;
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
    /// The CE-11 cross-encoder. `None` while it loads, when `[reranker]
    /// enabled` is false (the default — see `RerankerConfig`), or if loading
    /// failed; recall reads it as "stay on the RRF passthrough", which is the
    /// configuration every Phase B number was measured against. Same
    /// `RwLock<Option<Arc<_>>>` shape as `embedder`, and for the same reason:
    /// the critical section is one `Arc` clone, never held across an `.await`.
    pub reranker: Arc<RwLock<Option<Arc<Reranker>>>>,
    /// Always present (unlike `embedder`): building a `reqwest::Client`
    /// touches no network, so there's no loading state to model. Actual
    /// reachability lives in `ollama::ollama_status()`, updated by the
    /// background prober.
    pub ollama: Arc<OllamaClient>,
    /// Banks with a consolidation round in flight (CE-9b).
    ///
    /// `run_round`'s watermark read, fact selection and `start_run` are three
    /// separate transactions, so two overlapping rounds on one bank read the
    /// same watermark, select the same facts and both apply plans — duplicate
    /// observations that the advanced watermark then guarantees are never
    /// revisited. Two POSTs, or one POST landing on the 300s tick, is enough.
    ///
    /// // ponytail: a set of bank ids under a std Mutex, not a job table. The
    /// // critical section is one `insert`/`remove` and is never held across
    /// // an await; single-process by construction, so nothing here survives a
    /// // restart — and nothing needs to, since an abandoned `running` row has
    /// // a NULL watermark and is ignored.
    pub consolidating: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Mental models with a refresh in flight, keyed `"{bank_id}/{mm_id}"`
    /// (CE-10) — the same single-flight shape as `consolidating`, and there
    /// for the same class of bug.
    ///
    /// A refresh reads `last_refreshed_at`, recalls the facts newer than it,
    /// spends a minute in the LLM, then writes. Two concurrent refreshes of
    /// one model read the same watermark and both write: the loser's summary
    /// is overwritten while its watermark advance stands, so that window's
    /// facts are never summarised again — a silent lost update on the one path
    /// whose contract is "never destroy the working document". A second caller
    /// gets `Conflict` (409).
    ///
    /// Keyed per *model*, not per bank: refreshing two different mental models
    /// at once is fine, and the Ollama semaphore is what serialises the GPU.
    pub refreshing: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Bounded queue into the background retain worker
    /// (`retain::run_worker`). Capacity is `retain.queue_capacity`; the
    /// endpoint reserves a slot with `try_reserve` and answers 429 when the
    /// queue is full rather than buffering transcripts in RAM.
    pub retain_tx: mpsc::Sender<RetainTask>,
}
