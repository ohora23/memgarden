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
| **Reranker on by default** | `engine/cross_encoder.py` | Measured 37-107 ms against AC-2's 35 ms p50 budget for the *whole* recall — and the live legacy daemon itself runs `HINDSIGHT_API_RERANKER_PROVIDER=rrf`, so off *is* parity | AX-2's quality harness showing RRF-passthrough ranking is the limiting factor |
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
