-- CE-10 usage signal: which mental models `/reflect` actually cites.
--
-- The model whose refresh is worth its GPU is the one an answer draws on. That
-- was unmeasurable until now: `reflect` returns `mental_model_ids` — the models
-- it cited, already filtered to ids that exist (`keep_known`) — and the route
-- returned them to the caller and recorded nothing. So a model nobody ever
-- reads looked exactly like one read on every call.
--
-- Two columns rather than an events table. The question this has to answer is
-- "is this model ever used, and how recently" and both are answered by a
-- counter and a timestamp. A per-citation row would support windowed analysis
-- nobody has asked for, and it would grow without bound on the hot path.
--
-- `cited_count` is monotonic and never reset: a promotion or demotion rule
-- wants "has this ever earned its keep", and dividing by age is something a
-- reader can do with `created_at`, which is already here.
ALTER TABLE mental_models ADD COLUMN cited_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE mental_models ADD COLUMN last_cited_at INTEGER;
