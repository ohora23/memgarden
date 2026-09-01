-- MemGarden schema v11 (CE-12): a fact can be retracted, and a temporary fact
-- can stop being true on its own.
--
-- Why this is a column and not a link row. The graph already carries typed
-- links, and `docs/evidence/mental-model-supersession.md` names "a supersedes
-- edge" as the obvious home. It is the wrong home for this one relation:
-- every reader of a fact has to know whether it is live, so the answer must
-- travel with the row that `search::hydrate` already selects. As a link it
-- would be a join on the hot recall path, and every caller that forgot the
-- join would silently serve retracted facts — which is the exact failure this
-- migration exists to end.
--
--   superseded_by  the node that replaced this one. NULL = live.
--                  ON DELETE SET NULL because deleting the replacement must
--                  un-retract the original rather than orphan it: a fact
--                  whose retraction was itself deleted is live again.
--   expires_at     wall-clock ms after which the fact stops being true on its
--                  own ("the exam is tomorrow"). NULL = does not expire, which
--                  is almost every fact.
--
-- Both are NULL for all 7,761 existing rows, so recall behaviour is unchanged
-- until something is marked — see `docs/evidence/supersession-detection.md`
-- for what the before/after gold run is worth given exactly that.
ALTER TABLE memory_nodes
  ADD COLUMN superseded_by INTEGER REFERENCES memory_nodes(id) ON DELETE SET NULL;

ALTER TABLE memory_nodes
  ADD COLUMN expires_at INTEGER;

-- Partial index: the recall filter asks `superseded_by IS NULL` of an already
-- id-restricted set, which needs no index at all. This one serves the opposite
-- question — "what has been retracted" — which the operator surface and the
-- supersession audit both ask over a whole bank, where a scan of 7,761 rows to
-- find a handful is the wrong shape.
CREATE INDEX idx_memory_nodes_superseded
  ON memory_nodes(bank_id, superseded_by)
  WHERE superseded_by IS NOT NULL;
