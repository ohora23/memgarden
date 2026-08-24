//! Startup model load + backlog embedding worker.

use std::sync::Arc;
use std::time::Duration;

use memgarden_store::{Db, graph, nodes, search};

use crate::embed::{self, EmbedStatus, Embedder};
use crate::links;
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
///
/// `pub` for testing (same reason as `on_batch_embedded`): CE-9a's R4 nulls
/// an embedding on every text update and CE-9b re-queues nodes that way, so
/// something has to assert the vector actually comes *back* — and only a
/// caller that can drive one tick by hand can do that without a 300s wait.
pub async fn drain_once(db: &Arc<Db>, state: &AppState) {
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
        let embedded = batch.clone();
        match tokio::task::spawn_blocking(move || nodes::set_embeddings_batch(&db3, &batch)).await {
            Ok(Ok(())) => {
                on_batch_embedded(db, embedded).await;
            }
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

/// Semantic-link pass (CE-7, Critic Revision R2). Legacy streams link
/// creation right after each embedding commit (`orchestrator.py:418-420`,
/// `:2163`) rather than at retain time — B3 writes `embedding = NULL`, so a
/// retain-time KNN would find nothing forever.
///
/// For each node just embedded: KNN inside its bank, keep the same-fact_type
/// neighbours at cosine `>= 0.7`, cap at 20 (`orchestrator.py:1232`).
/// Best-effort: the embeddings are already committed and a missing link is a
/// weaker graph, not a lost fact.
///
/// `pub` for testing: the only production caller is `drain_once` above, which
/// cannot run without a loaded 133MB model, and the two things that live
/// *here* rather than in `links::semantic_links` — the `1.0 - distance`
/// cosine conversion and the `TOP_K * 5` over-fetch — need coverage that does
/// not depend on it (architect F1). See `tests/graph_api.rs`.
/// Returns the number of edges actually written — `insert_links` is
/// `ON CONFLICT DO NOTHING`, so on a re-run of already-linked nodes that is 0.
/// `drain_once` ignores it; the relink route (`routes::embed::relink_bank`)
/// reports it.
pub async fn on_batch_embedded(db: &Arc<Db>, embedded: Vec<(i64, String, Vec<f32>)>) -> usize {
    if embedded.is_empty() {
        return 0;
    }
    let db = db.clone();
    let result = tokio::task::spawn_blocking(move || {
        // KNN first, types second, and in that order for a reason. The types
        // map is `semantic_links`' fact_type oracle, and a neighbour missing
        // from it is dropped — `is_some_and` is false for an absent key. Built
        // from the just-embedded ids alone, as it was until CE-7 was measured,
        // it silently turned the fact_type filter into a *same-batch* filter:
        // every semantic edge joined two nodes from one batch of
        // `embedding.batch_size`, capping out-degree at `batch_size - 1`
        // against a `SEMANTIC_LINK_TOP_K` of 20. The migrated corpus showed it
        // as 6,918 edges against legacy's 65,149, out-degree max exactly 7.
        let mut found: Vec<(i64, Vec<(i64, f64)>)> = Vec::with_capacity(embedded.len());
        for (id, bank_id, embedding) in &embedded {
            // Over-fetch: vec0 partitions on bank_id only, so the fact_type
            // restriction is applied in Rust and needs headroom to still
            // yield 20 same-type neighbours.
            let neighbors: Vec<(i64, f64)> =
                search::knn(&db, bank_id, embedding, links::SEMANTIC_LINK_TOP_K * 5)?
                    .into_iter()
                    // vec0's cosine `distance` is `1 - cosine_similarity`.
                    .map(|(id, distance)| (id, 1.0 - distance))
                    .collect();
            found.push((*id, neighbors));
        }

        // One lookup covering the batch *and* everything its KNN turned up.
        let mut type_ids: Vec<i64> = embedded.iter().map(|(id, ..)| *id).collect();
        type_ids.extend(
            found
                .iter()
                .flat_map(|(_, ns)| ns.iter().map(|(id, _)| *id)),
        );
        type_ids.sort_unstable();
        type_ids.dedup();
        let types = graph::node_types(&db, &type_ids)?;

        let mut batch: Vec<graph::NewLink> = Vec::new();
        for (id, neighbors) in &found {
            let Some((_, fact_type)) = types.get(id) else {
                continue;
            };
            batch.extend(links::semantic_links(*id, fact_type, neighbors, &types));
        }
        graph::insert_links(&db, &batch, memgarden_core::now_ms())
    })
    .await;
    match result {
        Ok(Ok(written)) => {
            // Declared since MX-1 and never written until now: the dashboard
            // read `links written 0` while the live database held 235,219 of
            // them. The count was already coming back from `insert_links`
            // and being dropped on the floor.
            memgarden_core::metrics::METRICS
                .links_written
                .fetch_add(written as u64, std::sync::atomic::Ordering::Relaxed);
            written
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "semantic link pass failed");
            0
        }
        Err(e) => {
            tracing::warn!(error = %e, "semantic link task panicked");
            0
        }
    }
}
