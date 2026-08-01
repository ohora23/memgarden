-- MemGarden schema v1: banks, documents, memory nodes (FTS5 + sqlite-vec),
-- typed graph (entities/links), tags, and metrics/ledger tables.
-- All timestamps are INTEGER unix-ms. All real tables are STRICT.

CREATE TABLE schema_migrations (
  version    INTEGER NOT NULL PRIMARY KEY,
  applied_at INTEGER NOT NULL
) STRICT;

CREATE TABLE banks (
  bank_id     TEXT    NOT NULL PRIMARY KEY,
  mission     TEXT,
  disposition TEXT    CHECK (disposition IS NULL OR json_valid(disposition)),
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE documents (
  id         INTEGER NOT NULL PRIMARY KEY,
  bank_id    TEXT    NOT NULL REFERENCES banks(bank_id) ON DELETE CASCADE,
  doc_key    TEXT    NOT NULL,
  title      TEXT,
  metadata   TEXT    CHECK (metadata IS NULL OR json_valid(metadata)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (bank_id, doc_key)
) STRICT;

CREATE TABLE memory_nodes (
  id              INTEGER NOT NULL PRIMARY KEY,
  uuid            TEXT    NOT NULL UNIQUE,
  bank_id         TEXT    NOT NULL REFERENCES banks(bank_id) ON DELETE CASCADE,
  document_id     INTEGER REFERENCES documents(id) ON DELETE SET NULL,
  fact_type       TEXT    NOT NULL CHECK (fact_type IN ('world', 'observation', 'experience')),
  text            TEXT    NOT NULL,
  context         TEXT,
  embedding       BLOB    CHECK (embedding IS NULL OR length(embedding) = 1536),
  event_date      INTEGER,
  occurred_start  INTEGER,
  occurred_end    INTEGER,
  mentioned_at    INTEGER,
  metadata        TEXT    CHECK (metadata IS NULL OR json_valid(metadata)),
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_memory_nodes_bank_date ON memory_nodes(bank_id, event_date DESC);
CREATE INDEX idx_memory_nodes_bank_type_date ON memory_nodes(bank_id, fact_type, event_date DESC);
CREATE INDEX idx_memory_nodes_document ON memory_nodes(document_id);
CREATE INDEX idx_memory_nodes_occurred ON memory_nodes(occurred_start, occurred_end);
CREATE INDEX idx_memory_nodes_mentioned ON memory_nodes(mentioned_at);
CREATE INDEX idx_memory_nodes_embed_backlog ON memory_nodes(bank_id) WHERE embedding IS NULL;

-- FTS5 external-content index over memory_nodes.text. unicode61 alone fails
-- to match Korean particles glued to a root word (e.g. a bare query for
-- "데몬" won't match the token "데몬에"); prefix='2 3 4' plus a '*' suffix on
-- every query term (see search::fts_query_string) fixes exact-root lookups
-- but still can't reach *inside* an unsegmented compound word (see
-- fts_korean_compound_negative test) — a documented limit, not a bug.
CREATE VIRTUAL TABLE memory_nodes_fts USING fts5(
  text,
  content = 'memory_nodes',
  content_rowid = 'id',
  tokenize = 'unicode61 remove_diacritics 2',
  prefix = '2 3 4'
);

CREATE TRIGGER memory_nodes_fts_ai AFTER INSERT ON memory_nodes BEGIN
  INSERT INTO memory_nodes_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE TRIGGER memory_nodes_fts_ad AFTER DELETE ON memory_nodes BEGIN
  INSERT INTO memory_nodes_fts(memory_nodes_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;

CREATE TRIGGER memory_nodes_fts_au AFTER UPDATE ON memory_nodes BEGIN
  INSERT INTO memory_nodes_fts(memory_nodes_fts, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO memory_nodes_fts(rowid, text) VALUES (new.id, new.text);
END;

-- vec0 rows are a rebuildable derived index over memory_nodes.embedding, kept
-- in sync with the source of truth (the BLOB column) from Rust (nodes::set_embedding),
-- not a trigger, since embeddings arrive asynchronously after insert. Cleanup
-- on delete IS a trigger so it also fires on FK-cascade deletes (e.g. a bank
-- delete cascading into memory_nodes), which a Rust-side delete_node cannot see.
CREATE VIRTUAL TABLE vec_nodes USING vec0(
  bank_id   TEXT PARTITION KEY,
  embedding FLOAT[384] distance_metric=cosine
);

CREATE TRIGGER memory_nodes_vec_ad AFTER DELETE ON memory_nodes BEGIN
  DELETE FROM vec_nodes WHERE rowid = old.id;
END;

CREATE TABLE entities (
  id             INTEGER NOT NULL PRIMARY KEY,
  bank_id        TEXT    NOT NULL REFERENCES banks(bank_id) ON DELETE CASCADE,
  canonical_name TEXT    NOT NULL,
  entity_type    TEXT,
  created_at     INTEGER NOT NULL,
  UNIQUE (bank_id, canonical_name)
) STRICT;

CREATE TABLE node_entities (
  node_id   INTEGER NOT NULL REFERENCES memory_nodes(id) ON DELETE CASCADE,
  entity_id INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
  PRIMARY KEY (node_id, entity_id)
) STRICT, WITHOUT ROWID;

-- 4-column PK (legacy parity): the same (from, to, link_type) pair can carry
-- multiple distinct entity-grounded edges. entity_id has no FK (documented,
-- not enforced) — 0 is the sentinel for "no entity", since a WITHOUT ROWID
-- PK column cannot be NULL.
CREATE TABLE links (
  from_node_id INTEGER NOT NULL REFERENCES memory_nodes(id) ON DELETE CASCADE,
  to_node_id   INTEGER NOT NULL REFERENCES memory_nodes(id) ON DELETE CASCADE,
  link_type    TEXT    NOT NULL CHECK (link_type IN
                 ('semantic', 'temporal', 'entity', 'caused_by', 'causes', 'enables', 'prevents')),
  entity_id    INTEGER NOT NULL DEFAULT 0,
  weight       REAL    NOT NULL DEFAULT 1.0,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (from_node_id, to_node_id, link_type, entity_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE node_tags (
  node_id INTEGER NOT NULL REFERENCES memory_nodes(id) ON DELETE CASCADE,
  tag     TEXT    NOT NULL,
  PRIMARY KEY (node_id, tag)
) STRICT, WITHOUT ROWID;

CREATE TABLE document_tags (
  document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  tag         TEXT    NOT NULL,
  PRIMARY KEY (document_id, tag)
) STRICT, WITHOUT ROWID;

CREATE TABLE metric_snapshots (
  id         INTEGER NOT NULL PRIMARY KEY,
  created_at INTEGER NOT NULL,
  payload    TEXT    NOT NULL CHECK (json_valid(payload))
) STRICT;

CREATE TABLE benefit_ledger (
  id         INTEGER NOT NULL PRIMARY KEY,
  kind       TEXT    NOT NULL CHECK (kind IN ('recall_substitution', 'retain_cap_saving', 'manual')),
  bank_id    TEXT    REFERENCES banks(bank_id) ON DELETE SET NULL,
  detail     TEXT    CHECK (detail IS NULL OR json_valid(detail)),
  created_at INTEGER NOT NULL
) STRICT;
