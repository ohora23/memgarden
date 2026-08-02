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
| **Structured mental-model documents** — `StructuredDocument`, `parse_markdown`, `render_document`, delta operations | `memory_engine.py:11620-11710`, `reflect/structured/*` | CE-10 refreshes the whole document in one call; delta ops exist to avoid re-reading a large doc, which is not yet a cost anyone pays | A mental model large enough that full re-synthesis is the bottleneck. `mental_models.structured_content` is already in the schema for it |
| **Background mental-model refresh** — the maintenance loop that discovers due models and enqueues refreshes | `maintenance.py:417-425` and the op queue behind it | The due-ness rule *is* ported (`mental::cron::is_due`, reported as `due` on every read); only the ticker is missing, and a scheduler that spends GPU with no consumer is the wrong default | Any caller that reads mental models. Wiring `is_due` to a tick is a few lines |
| **Chinese temporal rules** | `engine/search/chinese_temporal_periods.py` (~1,800 lines) | No Chinese content; rule ordering is load-bearing and untestable without a corpus | A Chinese-language bank plus a labelled query set |
| **Reranker on by default** | `engine/cross_encoder.py` | Measured 37-107 ms against AC-2's 35 ms p50 budget for the *whole* recall — and the live legacy daemon itself runs `HINDSIGHT_API_RERANKER_PROVIDER=rrf`, so off *is* parity | AX-2's quality harness showing RRF-passthrough ranking is the limiting factor |
| **Mental-model tags, `subtype`, `description`, `entity_id`, `observations`, `links`, `last_updated`; `mental_model_versions`** | `memory_engine.py` mental-model DDL | Legacy reads none of them (`_row_to_mental_model:12688-12734` is the whole surface); `mental_model_versions` was dropped upstream at `o0j1k2l3m4n5_migrate_mental_models_data.py:83` | Nothing — these are dead in legacy too |
| **Knowledge-base folder/page tree over mental models** | `http.py:5257-5582` | A UI surface with no v1 requirement | A UI |
| **Session/turn-state tables** | legacy session tables | Routed to Phase C by Phase A (HK-1); `retain_jobs` covers retention progress | The Phase C hooks binary |

Related: `docs/design/*.md` each carry a `## Diverged from legacy` section for
the smaller, per-PR differences (prompt wording, id shapes, filter placement).
This file is only for whole subsystems.
