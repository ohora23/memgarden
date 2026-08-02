-- MemGarden schema v5 (AX-1): vector-space versioning.
--
-- A stored vector is only comparable with vectors produced the same way.
-- Until now nothing on disk recorded *how* — `memory_nodes.embedding` is a
-- bare 1536-byte BLOB and `vec_nodes` a bare FLOAT[384] — so a future import
-- of the legacy Python bank (MG-1, Phase D) would mix two embedding spaces
-- into one cosine comparison with no way to tell them apart afterwards.
--
-- `NULL` means "producer unknown" (an untagged legacy import), the same
-- convention jcode's `LEGACY_EMBEDDING_MODEL` uses. Both NULL and a foreign
-- id are excluded from dense comparison; neither is excluded from FTS/BM25,
-- the graph arm, or hydration. Hybrid search IS the migration strategy.
ALTER TABLE memory_nodes ADD COLUMN embedding_model TEXT;

-- Backfill, not NULL. Every vector in every existing database was written by
-- this codebase's own embed path — there is no import path yet (MG-1 is
-- Phase D and unwritten), so `embedding IS NOT NULL` and "produced by
-- fastembed BGESmallENV15" are the same set today. Leaving them NULL would
-- silently drop every existing row out of the dense arm on upgrade, which is
-- a recall regression disguised as caution.
--
-- The literal must equal `memgarden_core::EMBEDDING_MODEL_ID`; SQL cannot
-- reference a Rust const, so `backfill_literal_matches_the_active_model_id`
-- in tests/schema.rs pins the two together.
UPDATE memory_nodes
SET embedding_model = 'fastembed:BAAI/bge-small-en-v1.5'
WHERE embedding IS NOT NULL;
