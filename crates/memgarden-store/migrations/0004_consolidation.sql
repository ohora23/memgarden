-- MemGarden schema v4 (CE-9 / PR B7): consolidation storage — observation
-- provenance, evidence counting, and the run ledger B8 will write.
--
-- `node_sources` is the relational form of legacy's
-- `memory_units.source_memory_ids` uuid[] column; `proof_count` is the same
-- column legacy carries on that table and reads back in
-- `engine/search/reranking.py:173-176`.

-- Evidence strength for an observation: how many distinct source facts back
-- it. 0 for every non-observation node (and for an observation created with
-- no sources), which `recall::scoring::proof_norm` maps to the neutral 0.5.
ALTER TABLE memory_nodes ADD COLUMN proof_count INTEGER NOT NULL DEFAULT 0;

CREATE TABLE node_sources (               -- observation -> source facts (legacy source_memory_ids array)
  observation_id INTEGER NOT NULL REFERENCES memory_nodes(id) ON DELETE CASCADE,
  source_id      INTEGER NOT NULL REFERENCES memory_nodes(id) ON DELETE CASCADE,
  created_at     INTEGER NOT NULL,
  PRIMARY KEY (observation_id, source_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX idx_node_sources_source ON node_sources(source_id);

-- `proof_count` is derived, so any path that removes provenance must recount.
-- Rust recounts on the write paths it owns (insert, merge), but a source fact
-- deleted anywhere — directly, or by a bank/document FK cascade — removes a
-- node_sources row that no Rust code sees, and the count would stay high
-- forever. A trigger fires on the cascade too, which is why this is not in
-- `nodes::delete` (same reasoning as `memory_nodes_vec_ad`, 0001_init.sql:91).
-- When the observation itself is what was deleted, the UPDATE matches no row
-- and is a harmless no-op.
CREATE TRIGGER node_sources_ad AFTER DELETE ON node_sources BEGIN
  UPDATE memory_nodes
  SET proof_count = (SELECT count(*) FROM node_sources WHERE observation_id = old.observation_id)
  WHERE id = old.observation_id;
END;

CREATE TABLE consolidation_runs (
  id            INTEGER NOT NULL PRIMARY KEY,
  bank_id       TEXT    NOT NULL REFERENCES banks(bank_id) ON DELETE CASCADE,
  status        TEXT    NOT NULL CHECK (status IN ('running','done','failed')),
  facts_seen    INTEGER NOT NULL DEFAULT 0,
  created_n     INTEGER NOT NULL DEFAULT 0,
  updated_n     INTEGER NOT NULL DEFAULT 0,
  deleted_n     INTEGER NOT NULL DEFAULT 0,
  merged_n      INTEGER NOT NULL DEFAULT 0,
  watermark     INTEGER,                 -- max memory_nodes.id consolidated
  error         TEXT,
  started_at    INTEGER NOT NULL,
  finished_at   INTEGER
) STRICT;
