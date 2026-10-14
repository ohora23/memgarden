-- MemGarden schema v15: every vector is re-made by multilingual-e5-small.
--
-- The embedder was bge-small-en-v1.5, which is English-only. 90% of the
-- prompts it served are Korean, and on 1,208 recalled items labelled for
-- usefulness its cosine scored AUC 0.507 — no better than chance — against
-- 0.715 for multilingual-e5-small at the same 384 dimensions
-- (docs/evidence/multilingual-embeddings.md).
--
-- Nothing here touches `vec_nodes`, and nothing has to. `search::knn` only
-- compares vectors whose `embedding_model` matches the running build (AX-1),
-- so a bge row is already invisible to the new binary; nulling `embedding`
-- puts the node back on the embed backlog (`idx_memory_nodes_embed_backlog`),
-- and `set_embedding` replaces the vec0 row when the new vector lands. Until
-- the backlog drains, recall runs on its keyword arm for the nodes not yet
-- re-embedded. Rows with no producer tag at all (pre-AX-1 imports) are
-- re-embedded too: they were already excluded from every dense comparison.
--
-- The literal below is `memgarden_core::EMBEDDING_MODEL_ID`;
-- `tests/schema.rs` fails if the two drift apart.
UPDATE memory_nodes SET embedding = NULL
 WHERE embedding IS NOT NULL
   AND embedding_model IS NOT 'fastembed:intfloat/multilingual-e5-small';

-- Semantic edges were drawn from bge cosines at bge's 0.7 threshold; the two
-- models agree on only ~22% of those pairs. They are derived data: the embed
-- backlog redraws them for each node it re-embeds, and `POST
-- /v1/banks/{bank}/relink` (idempotent) fills in the edges a node embedded
-- early in the drain could not see yet. Run it once per bank after
-- `/v1/stats` reports `unembedded: 0`. Temporal, causal and entity edges do
-- not depend on the embedder and stay.
DELETE FROM links WHERE link_type = 'semantic';
