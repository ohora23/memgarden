# CE-10 — Mental models and single-shot reflect (PR B9)

Branch `feat/ce-10-mental-models`. Migration `0006_mental_models.sql`
(`LATEST_VERSION` = 6 — the plan said 0005, but AX-1 took that number first).

Legacy ports: `engine/memory_engine.py:103` (the pending sentinel),
`:11073-11077` and `:12688-12734` (the columns anything actually reads),
`:11263` (the embedded text), `:11269` (the id format), `:11646-11660` (the
no-new-facts skip), `:11724-11743` (the empty render), `maintenance.py:417-425`
(cron due-ness), `reflect/agent.py:1312-1314` (citation filtering).

## What this adds

* `0006_mental_models.sql` — `mental_models` (composite PK `(bank_id, id)`),
  `vec_mental_models` (a second vec0 space, `bank_id` partition key), the
  refresh index, and `mental_models_vec_ad`, the AFTER DELETE trigger that
  keeps the vector table clean on the FK-cascade path Rust cannot see
  (Critic Revision R5).
* `memgarden_store::mental_models` — insert / get / list / update / delete /
  `knn`, all bank-scoped, with the vector written in the **same transaction**
  as the text it was computed from.
* `memgardend::mental` — create, patch, and `refresh`: one recall over
  `source_query`, one bounded LLM call, and the three ported outcomes.
* `memgardend::mental::cron` — a 5-field cron parser and `is_due`.
* `memgardend::mental::reflect` — single-shot reflect: one recall (budget
  `low`) plus the nearest mental models, one bounded LLM call, citations
  filtered against what was retrieved.
* Routes: `GET|POST /v1/banks/{id}/mental-models`,
  `GET|PATCH|DELETE …/{mm_id}`, `POST …/{mm_id}/refresh`,
  `POST /v1/banks/{id}/reflect`. `?q=` on the list turns it into a KNN search.
* `OllamaClient::chat_json_bounded` — the interactive acquire path with the
  per-call `num_predict`/`num_ctx` ceilings the background path already had.
* `docs/parity-gaps.md` (Critic Revision NIT 26).

## The prompt is token-bounded by construction

The 2026-08-02 GPU-pinning incident's guard, third time: a `const` ceiling on
the prompt counted with `retain::token_count` over **system + user** before the
call, a `const` `num_predict` ceiling plus `maxLength` on every free-text
schema field for the reply, and an explicitly requested `num_ctx` so the two
fit whatever window the server would otherwise pick.

| | prompt | reply | window |
|---|---|---|---|
| refresh | 4096 | 2048 | 8192 |
| reflect | 4096 | 1024 | 8192 |

Nothing is ever truncated. Over-budget input is shed **whole**, in a documented
order — refresh drops `current_content` first (which degrades to legacy's full
synthesis mode) then memories from the tail; reflect drops memories from the
tail, then mental models, which are last because a curated summary is the
highest information per token in the payload. If not even one item fits, **no
call is made at all** and the request fails loudly. `assemble_refresh` and
`reflect::assemble` are the only paths from these modules to Ollama.

A stored `max_tokens` is caller data, so `reply_cap` clamps it to the const
rather than trusting it.

## Key decisions

* **Refresh uses the background acquire, reflect the interactive one.** Refresh
  mutates stored memory on an explicit request, like `/consolidate`: queueing
  behind a busy GPU beats losing the write. Reflect is the route Critic
  Revision R11 named as fail-fast, so it takes `ACQUIRE_TIMEOUT` and answers
  503.
* **Mental models are embedded inline, not through CE-4's backlog.** The
  backlog exists to keep retain's write transaction short; a mental model is
  created one at a time by a human decision. If the embedder is still loading
  the row is stored without a vector — invisible to KNN, visible everywhere
  else — and the next write with a vector fixes it.
* **The embedded text is `"{name} {content}"`** (`:11263`), so a name-only
  patch re-embeds too.
* **`vec_mental_models` carries the AX-1 producer tag.** `mental_models` has
  its own `embedding_model` column and `knn` filters on it, so the two vector
  spaces in this database obey one rule.
* **Ids are `"mm-<uuid4hex>"`** (`:11269`), which needed the `uuid` crate's
  `v4` feature. MG-1 imports legacy rows by this id.
* **Citations are uuids, never rowids.** SQLite recycles the rowid of a deleted
  max row.

## Diverged from legacy

* **No reflect agent loop.** Legacy's is ~3,900 lines over ten iterations, five
  tools, delta ops and directives. Not ported, with reasons and re-entry
  criteria in `docs/parity-gaps.md`. This is the single largest deliberate gap
  in Phase B and the plan says so.
* **No background refresh task.** Legacy's maintenance loop discovers due
  models and enqueues refreshes. Here `due` is *reported* on every read and
  acted on by nobody: CE-10 has no caller (the four hooks call recall and
  retain only), and a scheduler that spends GPU with no consumer is the
  opposite of what this system is for. The due-ness logic is ported and tested;
  wiring it to a ticker is a few lines the day a caller exists.
* **`trigger` is a cron string, not legacy's JSONB.** Legacy stores
  `{"refresh_after_consolidation": false}` in `trigger` and keeps the cron in a
  separate routine; the plan's DDL says cron expression, and one column that
  means one thing beats two that disagree.
* **`since` is filtered in Rust, not SQL.** `_get_supporting_facts` passes
  `since=last_refreshed_at` into its query; `recall` here is the whole hybrid
  pipeline and has no `since` axis, so the recalled rows are filtered on
  `occurred_start ?? mentioned_at` afterwards. A memory with neither timestamp
  is not "new since" anything and is excluded.
* **The refresh and reflect prompts are ours.** Legacy's 822 lines of reflect
  prompt describe tools this PR does not ship.
* **PATCH sets, never clears.** One COALESCE'd UPDATE, one statement shape;
  clearing a nullable column would need dynamic SQL or a sentinel, and nothing
  needs it.
* **Six dead columns and `mental_model_versions` are not ported**, per the
  plan. `structured_content` *is* in the DDL and is always NULL — the
  structured-document port is Phase C+, and adding the column to a populated
  table later is a migration for nothing.

## Known limits

* No embedding backlog for mental models: a model created while the embedder
  loads stays out of KNN until its next write. Tens of rows, human-triggered —
  a backlog table would be more machinery than the problem.
* The cron subset is minute/hour/dom/month/dow with `*`, `*/n`, ranges and
  lists. `@daily`, `L`, `#`, `?` and month/day *names* are rejected at write
  time rather than silently mis-scheduled.
* Reflect with an unavailable embedder still answers, from recalled memories
  alone; mental models simply do not join the payload.
* KNN over `vec_mental_models` is brute force, like `vec_nodes`. Measured
  201 µs p50 at **1,000** models, which is ~20× any realistic bank.

## Verification

`cargo test --workspace`: **401 passed, 13 ignored** (master: 367 / 11), fmt
and clippy `-D warnings` clean.

Tests that pin the ported behaviour, by name:

* `composite_primary_key_scopes_ids_to_a_bank`,
  `crud_round_trip_is_scoped_to_the_owning_bank` — the composite PK.
* `embedded_text_is_name_space_content` — `"{name} {content}"`.
* `knn_is_bank_partitioned_and_upserts_on_update`,
  `a_model_without_an_embedding_is_absent_from_knn_only`,
  `bank_delete_cascades_into_the_vector_table`.
* `refresh_with_zero_supporting_facts_makes_no_llm_call` — asserted against a
  stub's call counter, which is the only honest way to prove a *negative*.
* `refresh_producing_empty_content_preserves_the_old_content_and_errors` —
  502, content byte-identical, watermark **not** advanced, audit written.
* `is_due_compares_prev_fire_with_the_watermark`,
  `due_reflects_the_trigger_against_the_last_refresh`.
* `hallucinated_citation_ids_are_dropped`,
  `reflect_filters_hallucinated_citation_ids` — a stub returning a fabricated
  id.
* `prompt_and_reply_fit_the_requested_window`, `bounds_fit_the_requested_window`,
  `an_over_budget_refresh_sheds_whole_inputs_in_order`,
  `an_over_budget_reflect_sheds_whole_items`, `reply_cap_clamps_a_caller_supplied_budget`,
  `memory_text_cannot_escape_the_payload` — the bounds, the shed order, and the
  no-call floor.
* `fresh_database_has_the_0006_mental_model_schema`,
  `migrate_upgrades_a_v5_database_in_place`.

**Measured** (Ryzen 7 9800X3D, 16 threads, release):

* Reflect end to end against the real Ollama (`qwen3-14b-nothink`) and the real
  embedder, 3 memories in the payload: **6.21 s** cold (model load included),
  **1.70 s / 1.71 s** warm — `live_reflect`, `#[ignore]`d.
* Mental-model KNN, 1,000 models, k=3: **p50 201 µs, p95 206 µs, max 381 µs**
  — `mental_model_knn_bench`, `#[ignore]`d.
* AC-2 interleaved-paired recall check — two release binaries (master
  `42bff5a` vs this branch), alternated, **8 pairs**,
  `MEMGARDEN_BENCH_CONTROL=1 MEMGARDEN_BENCH_LOAD=1
  MEMGARDEN_BENCH_NODES=3000 MEMGARDEN_BENCH_REQUESTS=2000`:

  | | mean paired delta | median | per-pair spread |
  |---|---|---|---|
  | p50 | **+0.09 ms** | +0.08 ms | −0.57 … +1.32 ms |
  | p95 | **+0.32 ms** | +0.43 ms | −2.13 … +3.73 ms |

  Honest note on the data: pairs 1-7 are fully interleaved; pair 8's base arm
  produced no output line (harness capture, not a failed run) and was
  re-measured on its own immediately after, so it is a reconstructed pair.
  Dropping it entirely gives p95 +0.24 ms / p50 +0.11 ms — the same answer.

  Levels for context: base p95 43.2 ms, lever p95 43.6 ms. Absolute levels
  drift on this machine (re-benching an identical commit moved +1.5 ms), so the
  paired **delta** is the number, not the level — and the delta is well inside
  that drift, with three of the eight pairs negative. Expected: CE-10 adds no
  code to the recall path, only an empty table and an index the recall queries
  never touch.
