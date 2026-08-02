# Parity gaps

Legacy behaviour that MemGarden knowingly does **not** implement. Created in
PR B9 (Critic Revision NIT 26) so the Phase F cutover gate has one list to read
instead of eight design notes.

Each entry names what is missing, why, and what would have to be true to close
it. "No caller" is a real reason on this system: the four Claude Code hooks
call `recall` and `retain` and nothing else.

| Gap | Legacy | Why not ported | Re-entry criterion |
|---|---|---|---|
| **Reflect agentic tool loop** — 10 iterations, 5 tool schemas, forced tool sequence, `expand`, delta ops, directives | `engine/reflect/agent.py` (1,555 lines) + `reflect/prompts.py` (822) | No caller; qwen3-14b over Ollama has no reliable multi-turn tool-calling contract (legacy's own loop carries synthetic tool-error recovery paths to stay on the rails); it cannot fit one honest PR | A client that needs multi-hop retrieval **and** a model whose tool calling holds for 10 turns without recovery scaffolding |
| **Mental-model `tags`** — the column, its `@>`/`&&`/`=` list filtering, and tag-scoped refresh | read at `memory_engine.py:12699` (inside `_row_to_mental_model`), selected at `:11073`, filtered at `:11060-11070`, and load-bearing in refresh via `_resolve_refresh_tag_filtering(mental_model.get("tags"), trigger)` at `:11470`/`:12670` | **Not dead in legacy** — dropped here because the plan's DDL omits it and no caller sets mental-model tags. Recorded because the refresh subsystem CE-10 *did* port is the one that uses them | A caller that wants tag-scoped mental models, or a refresh whose recall must be narrowed by tag |
| **Structured mental-model documents** — `StructuredDocument`, `parse_markdown`, `render_document`, delta operations | `memory_engine.py:11620-11710`, `reflect/structured/*` | CE-10 refreshes the whole document in one call; delta ops exist to avoid re-reading a large doc, which is not yet a cost anyone pays | A mental model large enough that full re-synthesis is the bottleneck. `mental_models.structured_content` is already in the schema for it |
| **Background mental-model refresh** — the maintenance loop that discovers due models and enqueues refreshes | `maintenance.py:417-425` and the op queue behind it | The due-ness rule *is* ported (`mental::cron::is_due`, reported as `due` on every read); only the ticker is missing, and a scheduler that spends GPU with no consumer is the wrong default | Any caller that reads mental models. Wiring `is_due` to a tick is a few lines |
| **Chinese temporal rules** | `engine/search/chinese_temporal_periods.py` (~1,800 lines) | No Chinese content; rule ordering is load-bearing and untestable without a corpus | A Chinese-language bank plus a labelled query set |
| **Reranker on by default** | `engine/cross_encoder.py` | **Criterion evaluated in CE-11 (PR B10): met on quality, declined on latency and ingest throughput.** Measured, interleaved-paired at the shipped `top_k = 10`: **+13.73 ms p50 / +31.83 ms p95** on the whole recall (not the pre-CE-11 37-107 ms estimate this row used to carry). Both arms pass AC-2 idle (on: p50 20.4 ms, p95 40.7 ms), but the margin drops from 28/51 ms to 15/19 ms, and the background ingest loop falls to **89.9 % of offered load** with the cross-encoder running. The live legacy daemon also runs `HINDSIGHT_API_RERANKER_PROVIDER=rrf`, so off remains parity. **The reranker is implemented and supported as an opt-in** — this row is now about the *default*, not about capability | **Superseded — the old criterion fired.** AX-2 showed RRF-passthrough ranking *is* a limiting factor: MRR 0.482 → 0.739, nDCG@10 0.302 → 0.379 over 13 queries (`docs/design/ce-11-reranker.md`). The remaining bar is a latency one: a caller whose budget absorbs +14 ms p50 / +32 ms p95, **or** a rerank path that does not starve the ingest loop below 90 % of offered load |
| **Korean absolute dates in query temporal extraction** — `8월 2일`, `3월 15일` | `query_analyzer.py:182-246` runs `dateparser.search.search_dates` with language detection; `temporal_periods.py:156-159` deliberately declines exact dates *because* dateparser already handles them | **A parity gap, not a coverage gap** — and it was mis-recorded as the latter. `temporal::query::fallback_date` accepts only ISO-extended tokens (`len >= 10` and containing `-`), so `8월 2일` yields no constraint, the temporal arm never fires, and `scores.temporal` stays `NEUTRAL`. Verified against legacy's own dateparser: `search_dates('8월 2일')` → `datetime(2026, 8, 2)`, with and without `languages=['ko']`. Falsifies `ce-8-temporal.md`'s "the ISO fallback covers the explicit-date case these banks actually produce" — AX-2's q17 is a counterexample from our own gold set. **Not fixed in B10 on purpose**: the fix would invalidate the temporal numbers CE-11 just recorded | A CE-8 follow-up that adds one `N월 N일` rule (or a Korean date parser) **and re-baselines AX-2**, since it moves the temporal stratum. Closing it is also the precondition for the temporal stratum meaning anything: today neither of AX-2's two temporal queries exercises the arm |
| **Mental-model `subtype`, `description`, `entity_id`, `observations`, `links`, `last_updated`; `mental_model_versions`** | `memory_engine.py` mental-model DDL | Legacy reads none of them — `_row_to_mental_model:12688-12734` is the whole read surface and none of these six appear in it; `mental_model_versions` was dropped upstream at `o0j1k2l3m4n5_migrate_mental_models_data.py:83`. (`tags` is *not* in this row — see above; it is read at `:12699` and was cut for a different reason) | Nothing — these are dead in legacy too |
| **Knowledge-base folder/page tree over mental models** | `http.py:5257-5582` | A UI surface with no v1 requirement | A UI |
| **Session/turn-state tables** | legacy session tables | Routed to Phase C by Phase A (HK-1); `retain_jobs` covers retention progress | The Phase C hooks binary |

## What AC-1 does and does not cover for reflect

AC-1 is a **recall** comparison. Both `POST /reflect` and mental-model refresh
retrieve through `crate::recall::recall` with CE-6's ported scoring, and CE-10
adds no code to that pipeline, so retrieval parity is intact and unaffected.

**Reflect answer quality is not a parity claim in Phase B.** Two independent
reasons, either sufficient: the prompt text here is ours rather than legacy's
822 ported lines, and legacy's answer is the product of up to ten tool-calling
iterations with `expand`, so even a byte-identical prompt would diverge on any
question needing a second hop. `POST /reflect` exists in both systems and they
have **not** been compared. It is a new capability, to be measured on its own
terms if and when it has a caller.

One smaller scope note in the same area: `keep_known` ports two of legacy's
three citation filters (`agent.py:1312-1314`) — memories and mental models.
Legacy's third, `observation_ids`, has no counterpart because reflect's payload
has no observation channel; CE-9's observations reach reflect only as ordinary
recall results. Nothing is broken, but the reflect surface does not yet see
consolidation's output as a distinct thing.

Related: `docs/design/*.md` each carry a `## Diverged from legacy` section for
the smaller, per-PR differences (prompt wording, id shapes, filter placement).
This file is only for whole subsystems.
