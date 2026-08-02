# CE-4: Embedded embeddings + `rebuild_vec_index`

## What

In-binary CPU text embeddings for `memgardend`, replacing the legacy
Python/`sentence-transformers` service call with an in-process ONNX model,
plus the vector-index rebuild that CE-2 deferred.

- `Embedder` (`memgardend/src/embed.rs`): a `fastembed::TextEmbedding`
  (`bge-small-en-v1.5`, fp32 ONNX) behind a `Mutex`, loaded from a persistent
  on-disk cache (`paths::models_dir()`, `$XDG_DATA_HOME/memgarden/models`).
- `augment_for_embedding`: a pure function that builds the string actually
  fed to the model — different from the stored `text` — porting legacy
  date/entity augmentation.
- A startup loader that runs *after* the HTTP listener binds, and a backlog
  worker that embeds newly-inserted nodes (`memory_nodes.embedding IS NULL`)
  a few seconds later, in small batches.
- `store::nodes::pending_embeddings` / `set_embeddings_batch` and
  `store::search::rebuild_vec_index` (the CE-2 deferral: rebuilding the
  `vec_nodes` sqlite-vec index from `memory_nodes.embedding`, the source of
  truth).
- `/healthz` gains an `"embedding"` field (`loading|ready|disabled|error`),
  with `DEGRADED` on `error` — the first real use of the `DEGRADED` enum
  value CE-3 reserved.
- `POST /v1/banks/{id}/reindex` and a debug-gated `POST /v1/embed`.

## Why this design

**Async backlog embedding, not synchronous retain-time embedding.**
Legacy embeds inline during retain (`orchestrator.py:579,617`). Retain
doesn't exist yet in this PR (CE-5, B2/B3), but the schema and the backlog
index (`idx_memory_nodes_embed_backlog`, `0001_init.sql:52`) were built for
exactly this: nodes land with `embedding = NULL` and a background worker
catches up. This keeps whatever writes a node (retain, later consolidation)
fast and short-transaction, and reuses one embedding code path instead of
two. The cost: a node is briefly unsearchable by vector between insert and
the next backlog tick (≤`backlog_poll_secs`, default 5s) — acceptable since
FTS and (once B5 lands) the entity/graph arms still find it immediately.

**Startup load happens after the listener binds.** A first-run model
download measured ~9s (133MB from Hugging Face); a warm load is ~100ms
(measured in this PR's manual verification, see below). Blocking the port
bind on either would fail liveness checks for no reason. `/healthz` reports
`embedding: "loading"` in between, and (once recall/retain exist) those
endpoints will 503 until ready — decision already documented in the Phase B
plan.

**Drain loop, batch size 8, lock yield between batches (Critic Revision
R9+R10).** The backlog worker doesn't process one node per tick — it drains
while full batches keep coming back, then sleeps. Batch size 8 was chosen
over the plan body's original 32 specifically to cap one batch's ONNX
mutex hold at ~18ms (measured: ~2.2ms/short-English-sentence × 8), so a
concurrent interactive embed request (`/v1/embed`, and later recall's
query-embed) never queues behind a long drain for more than one batch.
`tokio::task::yield_now().await` between batches gives any such request a
chance to run.

```rust
// ponytail: single embedder instance, so a big backlog stalls concurrent
// query embeds for ~18ms per batch; add a second instance if p99 ever
// needs it — RAM-first principle (R9).
```

**Semantic-link hook point, not semantic-link logic (Critic Revision R2).**
The original plan had B3 (retain) write `embedding = NULL` and assumed B5
(entity/graph) would create semantic links at retain time — but retain-time
linking would then always see a NULL embedding and permanently produce zero
semantic links. The fix, adopted from the legacy streaming design
(`orchestrator.py:418-420,2163`), is to link right after a node's embedding
is actually computed — i.e., in this PR's backlog worker, immediately after
`set_embeddings_batch` commits. B1 ships the call site as a no-op:

```rust
fn on_batch_embedded(_db: &Arc<Db>, _embedded: &[(i64, String)]) {}
```

B5 fills in the body with per-`fact_type` KNN (top-k 20, threshold 0.7);
nothing about the worker's control flow needs to change.

**Explicit L2 normalization despite fastembed already returning unit
vectors.** Measured `‖v‖ = 1.000000` for `bge-small-en-v1.5` output, but the
0.7 (semantic-link, B5) and 0.97 (dedup, B7) cosine thresholds are
meaningless without a *guaranteed* unit vector — the legacy port brief flags
exactly this as a gotcha. `normalize_l2` runs unconditionally in
`Embedder::embed_batch`; it's cheap (single pass, no allocation beyond the
output `Vec`) and the vectors are used for real product logic downstream, not
just doc similarity.

**`std::sync::RwLock<Option<Arc<Embedder>>>` in `AppState`, not
`tokio::sync::RwLock` or a channel.** The critical section is "clone an
`Arc`," never awaited across, so a blocking `std::sync::RwLock` is fine and
adds no async ceremony. No new dependency either way — reused what's already
in scope.

## Alternatives rejected

- **Raw `ort` instead of `fastembed`.** Would mean hand-writing tokenizer
  wiring, CLS pooling, L2 normalization, and HF model download for zero
  measurable gain — `fastembed` already *is* the `ort` path underneath
  (pulls `ort 2.0.0-rc.13` directly) and reproduces the legacy vectors
  bit-identically. Rejected in the Phase B plan itself; not revisited here.
- **A second `Embedder` instance for the backlog worker**, so a big drain
  never blocks an interactive query embed at all. Rejected per R9 — RAM cost
  for a second ONNX session isn't justified without a measured p99 problem;
  the ponytail comment above names the upgrade path if one shows up.
- **`batch_size = 32`** (the plan body's original default). Overridden by
  Critic Revision R9 to 8, to bound per-batch mutex hold time; documented
  above.
- **Synchronous semantic linking inside this PR's backlog worker.** Would
  require entity/graph tables and scoring logic that don't exist until B5;
  building a hook point here (R2) means B5 is a pure addition, not a rework
  of B1's control flow.

## Ported vs. diverged (legacy `file:line`)

| Behavior | Legacy | This PR |
|---|---|---|
| Embedded string augmentation | `engine/retain/embedding_processing.py:15-46` | Ported verbatim: `date = occurred_start ?? mentioned_at`, `"%B %Y"` readable format (`memory_engine.py:3453,3472-3474`), range/point/no-date branches, trailing `" [e1, e2, …]"` entity suffix |
| Embedding model | `sentence-transformers BAAI/bge-small-en-v1.5` (CPU) | `fastembed 5.17.4` / `Xenova/bge-small-en-v1.5` — verified bit-identical to 7 decimals on the same inputs (Phase B plan, Verified Environment Facts) |
| Vector-space parity check (this PR, manual) | — | `cos(migration,migration2)=0.8012`, `cos(migration,banana)=0.4586` — closely tracks the plan's independently-measured `cos(port,banana)=0.4568` |
| Semantic link creation timing | Streaming, right after embed (`orchestrator.py:418-420,2163`) | Same design; hook point lands in B1, logic lands in B5 (R2) |
| Retain-time embedding | Synchronous, inline (`orchestrator.py:579,617`) | **Diverged**: async via the backlog worker — retain doesn't exist yet in this PR, but the divergence is real once B3 lands. See `## Diverged from legacy`. |
| `vec_nodes` rebuild | No equivalent — legacy has no vec0-style external index to rebuild | New: `rebuild_vec_index`, chunked at 500 rows, **committing per chunk** (NIT 17) so a large rebuild doesn't hold the write lock end-to-end |

## Diverged from legacy

- **Async retain-time embedding.** Legacy embeds synchronously inline during
  retain. This system writes `embedding = NULL` at insert time (owned by
  B3) and relies on this PR's backlog worker to catch up within
  `backlog_poll_secs`. Trade-off: short retain transactions and one shared
  embedding code path, at the cost of a brief (≤5s) window where a new node
  isn't vector-searchable yet. Documented as deliberate in the Phase B plan's
  Trade-offs table.
- **`batch_size = 8`, not the plan body's original 32.** Critic Revision R9;
  caps per-batch ONNX mutex hold to ~18ms instead of ~70ms.

## Known limits / follow-ups

- The backlog worker is single-instance (one `Mutex<TextEmbedding>`); a
  large backlog stalls concurrent query embeds by up to one batch
  (~18ms/batch, measured). See the `ponytail:` comment in `embed_task.rs`.
- `augment_for_embedding`'s `entities` parameter is always `&[]` until B5
  lands entity resolution — the function is fully implemented and tested
  now (all branches, pure), but the entity-suffix branch has no real caller
  yet.
- `/v1/embed` is a debug tool (`embedding.debug_endpoint`, default off) —
  not meant to be a stable API; it exists so this PR is manually verifiable
  end-to-end without waiting on B2's retain endpoint.
- `rebuild_vec_index`'s unscoped (`bank_id: None`) path rebuilds the entire
  index across all banks in one call; at current/expected scale (thousands
  of nodes) this is fine, per the plan's Trade-offs table on brute-force KNN
  cost being scan-bound rather than *k*-bound.

## Manual verification (this PR)

Daemon started on port 9100 (never 9077/9090), model loaded from a warm
cache in ~160ms, bank created, two nodes inserted directly via SQL (no
retain endpoint yet), backlog worker picked them up within one tick:

```
$ curl .../healthz   # before insert
{"embedding":"ready","nodes":0,...}

# insert 2 nodes with embedding = NULL directly ...
$ sleep 7   # one backlog_poll_secs tick (default 5s)
$ sqlite3-equivalent check: both nodes' embedding column now 1536 bytes (384 × f32)

$ curl -X POST .../v1/banks/demo/reindex
{"rebuilt":2}   # HTTP 200

$ curl .../healthz
{"banks":1,"nodes":2,"embedding":"ready","status":"HEALTHY",...}
```

**Measured:** `/v1/embed` (debug endpoint enabled for this run only) p50 =
2.41ms, p95 = 3.39ms over 50 requests (target ≤5ms p50 — met with headroom).
`model_smoke` (`#[ignore]`d, run manually): cold load + embed 3 sentences in
8.25s including the 133MB download; `cos(migration,migration2)=0.8012` vs
`cos(migration,banana)=0.4586`, confirming on-topic pairs score higher than
unrelated ones.
