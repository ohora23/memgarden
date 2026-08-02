-- MemGarden schema v3 (CE-7 / PR B5): entity statistics, co-occurrence
-- tracking, reverse graph traversal, and the deferred links.weight CHECK.
--
-- `entities` and `links` already exist from 0001; this migration only adds
-- what entity resolution and the graph recall arm need on top.

-- Resolution reads last_seen (temporal-proximity term) and mention_count is
-- what makes a repeatedly-seen entity a stable anchor. legacy: models.py
-- entity columns, engine/retain/entity_resolver.py:684-717.
ALTER TABLE entities ADD COLUMN mention_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE entities ADD COLUMN first_seen    INTEGER;
ALTER TABLE entities ADD COLUMN last_seen     INTEGER;

-- legacy: models.py:240-258. No decay column — legacy has none, only a count
-- and the last time the pair was seen together.
CREATE TABLE entity_cooccurrences (
  entity_id_1        INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
  entity_id_2        INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
  cooccurrence_count INTEGER NOT NULL DEFAULT 1,
  last_cooccurred    INTEGER NOT NULL,
  PRIMARY KEY (entity_id_1, entity_id_2),
  CHECK (entity_id_1 < entity_id_2)       -- canonical order, legacy entity_cooccurrence_order_check
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_entity_cooc_e2    ON entity_cooccurrences(entity_id_2);
CREATE INDEX idx_entity_cooc_count ON entity_cooccurrences(cooccurrence_count DESC);

-- Critic Revision NIT 18: link weights are clamped to [0, 1] in Rust and the
-- database now enforces it. SQLite has no ADD CONSTRAINT, so this is the
-- standard rename-copy-drop rebuild. Safe here specifically because nothing
-- has a foreign key *pointing at* `links` (only outgoing FKs to
-- memory_nodes), so the RENAME cannot rewrite another table's references.
-- CE-7 is also the first writer of this table, so in practice the copy moves
-- zero rows in every deployed database.
ALTER TABLE links RENAME TO links_pre0003;

CREATE TABLE links (
  from_node_id INTEGER NOT NULL REFERENCES memory_nodes(id) ON DELETE CASCADE,
  to_node_id   INTEGER NOT NULL REFERENCES memory_nodes(id) ON DELETE CASCADE,
  link_type    TEXT    NOT NULL CHECK (link_type IN
                 ('semantic', 'temporal', 'entity', 'caused_by', 'causes', 'enables', 'prevents')),
  entity_id    INTEGER NOT NULL DEFAULT 0,
  weight       REAL    NOT NULL DEFAULT 1.0 CHECK (weight >= 0.0 AND weight <= 1.0),
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (from_node_id, to_node_id, link_type, entity_id)
) STRICT, WITHOUT ROWID;

INSERT INTO links (from_node_id, to_node_id, link_type, entity_id, weight, created_at)
SELECT from_node_id, to_node_id, link_type, entity_id,
       max(0.0, min(1.0, weight)), created_at
FROM links_pre0003;

DROP TABLE links_pre0003;

-- Reverse traversal for the graph arm; the 4-column PK covers the forward
-- direction because the table is WITHOUT ROWID (the PK *is* the table).
CREATE INDEX idx_links_to ON links(to_node_id, link_type);

-- Entity co-membership expansion joins node_entities on entity_id, which the
-- (node_id, entity_id) PK cannot serve. legacy parity:
-- idx_unit_entities_entity_unit (link_expansion_retrieval.py:296-297).
CREATE INDEX idx_node_entities_entity ON node_entities(entity_id, node_id);
