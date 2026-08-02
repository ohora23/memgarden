-- MemGarden schema v6 (CE-10 / PR B9): mental models — curated, named
-- summaries that a refresh regenerates from the bank's own facts.
--
-- Columns are taken from the only two places legacy actually reads
-- (`memory_engine.py:11073-11077` for the list query, `_row_to_mental_model`
-- `:12688-12734` for the row mapping). The six dead columns legacy still
-- carries — `subtype`, `description`, `entity_id`, `observations`, `links`,
-- `last_updated` — are deliberately NOT ported, and neither is
-- `mental_model_versions` (dropped upstream at
-- `o0j1k2l3m4n5_migrate_mental_models_data.py:83`).

CREATE TABLE mental_models (
  id                 TEXT    NOT NULL,          -- "mm-<uuid4hex>" (memory_engine.py:11269)
  bank_id            TEXT    NOT NULL REFERENCES banks(bank_id) ON DELETE CASCADE,
  name               TEXT    NOT NULL,
  source_query       TEXT,                      -- the recall query a refresh runs
  content            TEXT    NOT NULL DEFAULT '',
  -- Reserved, always NULL in this PR: the structured-document port
  -- (StructuredDocument / delta ops, `memory_engine.py:11620-11710`) is
  -- Phase C+ — see docs/parity-gaps.md. The column exists now because adding
  -- it later to a populated table is another migration for nothing.
  structured_content TEXT    CHECK (structured_content IS NULL OR json_valid(structured_content)),
  -- Audit trail of the last refresh attempt, including the two no-write
  -- outcomes (`no_new_facts`, `empty_candidate`) that legacy records the same
  -- way (`memory_engine.py:11651-11657, 11724-11743`).
  reflect_response   TEXT    CHECK (reflect_response IS NULL OR json_valid(reflect_response)),
  max_tokens         INTEGER,                   -- document budget; legacy COALESCEs to 2048 (:11291)
  trigger            TEXT,                      -- 5-field cron expression, UTC
  embedding          BLOB    CHECK (embedding IS NULL OR length(embedding) = 1536),
  -- AX-1: the producer of `embedding`, same convention and same literal as
  -- `memory_nodes.embedding_model`. NULL means "unknown producer" and is
  -- excluded from KNN, exactly as it is for nodes.
  embedding_model    TEXT,
  last_refreshed_at  INTEGER,                   -- the cron watermark (maintenance.py:417-425)
  created_at         INTEGER NOT NULL,
  PRIMARY KEY (bank_id, id)                     -- legacy composite PK
) STRICT;

-- The list order (`memory_engine.py:11077`: ORDER BY last_refreshed_at DESC).
CREATE INDEX idx_mental_models_refreshed ON mental_models(bank_id, last_refreshed_at DESC);

-- Second vector space, partitioned like `vec_nodes`. Its rowid is the
-- `mental_models` **implicit rowid**, not the TEXT id — vec0 rowids are
-- integers, and the composite PK gives this table no integer key of its own
-- (Critic Revision R5).
CREATE VIRTUAL TABLE vec_mental_models USING vec0(
  bank_id   TEXT PARTITION KEY,
  embedding FLOAT[384] distance_metric=cosine
);

-- Cleanup on delete is a trigger, not Rust, so it also fires on the
-- FK-cascade path (a bank delete cascading into mental_models), which a
-- Rust-side delete cannot see. Same reasoning as `memory_nodes_vec_ad`
-- (0001_init.sql:91).
CREATE TRIGGER mental_models_vec_ad AFTER DELETE ON mental_models BEGIN
  DELETE FROM vec_mental_models WHERE rowid = old.rowid;
END;
