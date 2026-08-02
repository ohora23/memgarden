//! Startup model load + backlog embedding worker.

use std::sync::Arc;
use std::time::Duration;

use memgarden_store::{Db, nodes};

use crate::embed::{self, EmbedStatus, Embedder};
use crate::state::AppState;

/// Loads the embedding model in a blocking thread and publishes it into
/// `AppState.embedder` + the process-wide `embed::EmbedStatus`. Spawned from
/// main.rs *after* the listener binds (decision #1) — a first-run model
/// download (measured ~9s) must not delay the port bind; `/healthz` reports
/// `"loading"` until this completes.
pub async fn load_at_startup(state: AppState) {
    if !state.cfg.embedding.enabled {
        embed::set_embed_status(EmbedStatus::Disabled);
        return;
    }
    let cfg = state.cfg.embedding.clone();
    match tokio::task::spawn_blocking(move || Embedder::load(&cfg)).await {
        Ok(Ok(embedder)) => {
            *state.embedder.write().expect("embedder lock poisoned") = Some(Arc::new(embedder));
            embed::set_embed_status(EmbedStatus::Ready);
            tracing::info!("embedding model ready");
        }
        Ok(Err(e)) => {
            embed::set_embed_status(EmbedStatus::Error);
            tracing::error!(error = %e, "embedding model failed to load");
        }
        Err(e) => {
            embed::set_embed_status(EmbedStatus::Error);
            tracing::error!(error = %e, "embedding model load task panicked");
        }
    }
}

/// Backlog worker: every `backlog_poll_secs`, drains `pending_embeddings` in
/// batches of `batch_size` until fewer than a full batch remains, then sleeps
/// for the next tick (Critic Revision R9+R10).
///
/// // ponytail: single embedder instance, so a big backlog stalls concurrent
/// // query embeds for ~18ms per batch (measured, batch_size=8); add a
/// // second instance if p99 ever needs it — RAM-first principle (R9).
pub async fn run_backlog(db: Arc<Db>, state: AppState) {
    if !state.cfg.embedding.enabled {
        return;
    }
    let mut ticker =
        tokio::time::interval(Duration::from_secs(state.cfg.embedding.backlog_poll_secs));
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = ticker.tick() => drain_once(&db, &state).await,
            _ = &mut shutdown => break,
        }
    }
}

/// One tick's drain loop: keep pulling and embedding batches while full
/// batches keep coming back; stop after a partial (or empty) batch. Yields
/// between batches (R10) so an interactive `/v1/embed` request queued behind
/// a long drain isn't stuck waiting for the whole backlog.
async fn drain_once(db: &Arc<Db>, state: &AppState) {
    let batch_size = state.cfg.embedding.batch_size;
    loop {
        let embedder = state
            .embedder
            .read()
            .expect("embedder lock poisoned")
            .clone();
        let Some(embedder) = embedder else {
            return; // model still loading (or failed) — try again next tick.
        };

        let db2 = db.clone();
        let pending =
            match tokio::task::spawn_blocking(move || nodes::pending_embeddings(&db2, batch_size))
                .await
            {
                Ok(Ok(rows)) => rows,
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "pending_embeddings query failed");
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "pending_embeddings task panicked");
                    return;
                }
            };
        if pending.is_empty() {
            return;
        }
        let n = pending.len();

        let texts: Vec<String> = pending
            .iter()
            .map(|(_, _, text, start, end, mentioned)| {
                embed::augment_for_embedding(text, *start, *end, *mentioned, &[])
            })
            .collect();

        let embedder2 = embedder.clone();
        let vectors = match tokio::task::spawn_blocking(move || embedder2.embed_batch(&texts)).await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "embed_batch failed");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "embed_batch task panicked");
                return;
            }
        };

        let ids_and_banks: Vec<(i64, String)> = pending
            .iter()
            .map(|(id, bank_id, ..)| (*id, bank_id.clone()))
            .collect();
        let batch: Vec<(i64, String, Vec<f32>)> = ids_and_banks
            .iter()
            .cloned()
            .zip(vectors)
            .map(|((id, bank_id), v)| (id, bank_id, v))
            .collect();

        let db3 = db.clone();
        match tokio::task::spawn_blocking(move || nodes::set_embeddings_batch(&db3, &batch)).await {
            Ok(Ok(())) => on_batch_embedded(db, &ids_and_banks),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "set_embeddings_batch failed");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "set_embeddings_batch task panicked");
                return;
            }
        }

        if n < batch_size {
            return; // backlog drained.
        }
        tokio::task::yield_now().await;
    }
}

/// Hook point for B5 (Critic Revision R2): the legacy design streams
/// semantic-link creation right after each embedding commit
/// (orchestrator.py:418-420, :2163), not at retain time — B3 writes
/// `embedding = NULL`, so retain-time linking would be permanently empty.
/// B5 fills this in with per-fact_type KNN (top-k 20, threshold 0.7) over
/// `embedded`. No-op in B1.
fn on_batch_embedded(_db: &Arc<Db>, _embedded: &[(i64, String)]) {}
