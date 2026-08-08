# Parity gaps

Legacy behaviour that MemGarden knowingly does **not** implement. Created in
PR B9 (Critic Revision NIT 26) so the Phase F cutover gate has one list to read
instead of eight design notes.

Each entry names what is missing, why, and what would have to be true to close
it. "No caller" is a real reason on this system: the four Claude Code hooks
call `recall` and `retain` and nothing else.

**Rows with a ~~struck~~ title are CLOSED and retained for audit — they are not
open gaps.** The Phase F cutover gate reads the unstruck rows. A closed row
stays because the reasoning that closed it (often a correction to this file's
own earlier claim) is the part worth keeping; deleting it would erase the
correction and invite the gap being "rediscovered" and re-opened wrongly.

| Gap | Legacy | Why not ported | Re-entry criterion |
|---|---|---|---|
| **Reflect agentic tool loop** — 10 iterations, 5 tool schemas, forced tool sequence, `expand`, delta ops, directives | `engine/reflect/agent.py` (1,555 lines) + `reflect/prompts.py` (822) | No caller; qwen3-14b over Ollama has no reliable multi-turn tool-calling contract (legacy's own loop carries synthetic tool-error recovery paths to stay on the rails); it cannot fit one honest PR | A client that needs multi-hop retrieval **and** a model whose tool calling holds for 10 turns without recovery scaffolding |
| **Mental-model `tags`** — the column, its `@>`/`&&`/`=` list filtering, and tag-scoped refresh | read at `memory_engine.py:12699` (inside `_row_to_mental_model`), selected at `:11073`, filtered at `:11060-11070`, and load-bearing in refresh via `_resolve_refresh_tag_filtering(mental_model.get("tags"), trigger)` at `:11470`/`:12670` | **Not dead in legacy** — dropped here because the plan's DDL omits it and no caller sets mental-model tags. Recorded because the refresh subsystem CE-10 *did* port is the one that uses them | A caller that wants tag-scoped mental models, or a refresh whose recall must be narrowed by tag |
| **Structured mental-model documents** — `StructuredDocument`, `parse_markdown`, `render_document`, delta operations | `memory_engine.py:11620-11710`, `reflect/structured/*` | CE-10 refreshes the whole document in one call; delta ops exist to avoid re-reading a large doc, which is not yet a cost anyone pays | A mental model large enough that full re-synthesis is the bottleneck. `mental_models.structured_content` is already in the schema for it |
| **Background mental-model refresh** — the maintenance loop that discovers due models and enqueues refreshes | `maintenance.py:417-425` and the op queue behind it | The due-ness rule *is* ported (`mental::cron::is_due`, reported as `due` on every read); only the ticker is missing, and a scheduler that spends GPU with no consumer is the wrong default | Any caller that reads mental models. Wiring `is_due` to a tick is a few lines |
| **Chinese temporal rules** | `engine/search/chinese_temporal_periods.py` (~1,800 lines) | No Chinese content; rule ordering is load-bearing and untestable without a corpus | A Chinese-language bank plus a labelled query set |
| **Reranker on by default** | `engine/cross_encoder.py` | **Criterion evaluated in CE-11 (PR B10): met on quality, declined on latency and ingest throughput.** Measured, interleaved-paired at the shipped `top_k = 10`: **+13.73 ms p50 / +31.83 ms p95** on the whole recall (not the pre-CE-11 37-107 ms estimate this row used to carry). Both arms pass AC-2 idle (on: p50 20.4 ms, p95 40.7 ms), but the margin drops from 28/51 ms to 15/19 ms, and the background ingest loop falls to **89.9 % of offered load** with the cross-encoder running. The live legacy daemon also runs `HINDSIGHT_API_RERANKER_PROVIDER=rrf`, so off remains parity. **The reranker is implemented and supported as an opt-in** — this row is now about the *default*, not about capability | **Superseded — the old criterion fired, on a narrower margin than first recorded.** AX-2 showed RRF-passthrough ranking *is* a limiting factor: **MRR 0.551 → 0.701** over 13 queries. nDCG@10 is 0.340 → 0.352 and recall@10 is a 0.044 *loss*. (This row previously read `MRR 0.482 → 0.739, nDCG@10 0.302 → 0.379`, measured against the pre-`fix/ce-8-korean-absolute-dates` baseline in which q17's temporal constraint never fired; re-baselined, and `docs/design/ce-11-reranker.md` carries the full tables.) The remaining bar is a latency one: a caller whose budget absorbs +14 ms p50 / +32 ms p95, **or** a rerank path that does not starve the ingest loop below 90 % of offered load |
| ~~**Korean absolute dates in query temporal extraction** — `8월 2일`, `3월 15일`~~ **CLOSED, as a deliberate divergence** | `query_analyzer.py:182-246` runs `dateparser.search.search_dates` with language detection; `temporal_periods.py:156-159` deliberately declines exact dates *because* dateparser already handles them | **Closed by `fix/ce-8-korean-absolute-dates`** — `N월 N일` and `YYYY년 N월 N일` now parse, day-precise, in the fallback. **This row's own verification claim was wrong and is corrected here.** It recorded `search_dates('8월 2일')` → `datetime(2026, 8, 2)`; that is true only when the reference date happens to be the 2nd, which it was the day the check was run. Re-run on dateparser 1.4.1 with an explicit `RELATIVE_BASE = 2026-08-03` (AX-2's pinned now), which is what `query_analyzer.py` supplies: `'8월 2일'` → `2026-08-03`, `'3월 15일'` → `2026-03-03`, `'2024년 3월 15일'` → `2026-03-03`, `'12월 31일'` → `2026-12-03`. dateparser matches **only the `N월` token**; the day and the year come from the reference date, and legacy narrows that to a single day (`:283-287`). Nor does legacy filter them out: `_is_cjk_character` is `U+4E00–U+9FFF`, Han only, so Hangul `월` is not CJK, `is_embedded_cjk_dateparser_match` returns `False` at its first check, and `_date_match_score('8월')` scores 100 on the digit. **So on q17 legacy is worse than the gap was**: it returns a confidently wrong Aug-3 window that filters out the Aug 2 facts the query asks for, where we returned nothing and stayed neutral. We are day-precise on purpose — same category as CE-7 dropping legacy's phantom causal `+1.0`. `docs/design/ce-8-korean-absolute-dates.md` | **Falsifiable, in both directions.** To reopen as a gap, show a query shape legacy resolves *correctly* and we do not — bare `N월` and `8/2` do not qualify, legacy resolves neither. To overturn the divergence, show `search_dates` returning the written day and year under an explicit `RELATIVE_BASE`; `we_do_not_take_the_day_or_the_year_from_the_reference_date` is the assertion that would then have to change |
| **Mental-model `subtype`, `description`, `entity_id`, `observations`, `links`, `last_updated`; `mental_model_versions`** | `memory_engine.py` mental-model DDL | Legacy reads none of them — `_row_to_mental_model:12688-12734` is the whole read surface and none of these six appear in it; `mental_model_versions` was dropped upstream at `o0j1k2l3m4n5_migrate_mental_models_data.py:83`. (`tags` is *not* in this row — see above; it is read at `:12699` and was cut for a different reason) | Nothing — these are dead in legacy too |
| **Knowledge-base folder/page tree over mental models** | `http.py:5257-5582` | A UI surface with no v1 requirement | A UI |
| ~~**Session/turn-state tables**~~ **CLOSED by C1 (HK-1a)** | legacy `turns.json` / `retention_tracking.json` (`state.py:111-193`) | **Closed by `feat/hk-1a-session-state`** — `0007_sessions.sql` adds one row per `(bank_id, session_id)` with `store::sessions` and `POST/GET /v1/banks/{bank_id}/sessions`. The grain split this row used to gesture at is now explicit and enforced: `retain_jobs` is one row per retain *request*, `sessions` is one row per *session*, `sessions.retains` is a count and the detail stays in `retain_jobs` joined on `session_id`. Two cursors rather than one, because the daemon's `202` means *queued*: `byte_offset` is what the hook POSTed, `confirmed_offset` is what a clean job ingested, and the gap is the in-flight-or-lost window. Retention is by age (90 days, on the metrics tick), not legacy's 10,000-entry truncation. `docs/design/c1-session-state.md` | **Not a gap.** Legacy state is **not migrated, and MG-1 did not migrate it** (Phase D, `mg-migrate import`, shipped 2026-08-06 — forecast now fact): legacy tracks a message *index*, `sessions` tracks a *byte offset*, and there is no function from one to the other without re-parsing every historical transcript. Every session starts at offset 0 after cutover — one initial retain each, bounded by `retain.max_initial_messages`. Reopen only if a *table* is missing, not a field |

## Phase C — the hook layer

Added by C5 (HK-2), the last Phase C PR, so the cutover gate reads one list
rather than six hook design notes. Each row was checked against the legacy
source **and** against the live `~/.hindsight/claude-code.json`: "unset in the
live config" means we are matching the system as it actually runs, which is
what AC-1 compares against.

| Gap | Legacy | Why not ported | Re-entry criterion |
|---|---|---|---|
| **Daemon lifecycle management** — start on `SessionStart`, stop on `SessionEnd` | `lib/daemon.py`, `session_start.py:49`, `session_end.py:44` | A 5 s hook must not spawn a process that loads a 133 MB model; that shape *is* the pg0 restart race the rebuild removes. The embedded store also removed the reason auto-start was attractive: there is no second process to keep alive. `memgardend` is a long-lived user service and `hooks status` prints the command to start it | A user who wants `memgardend` on demand — **and** a start path that cannot race a migration |
| **Cross-bank recall** (`recallAdditionalBanks`, `recallAdditionalBankFilters`) | `recall.py:207-237` | Unset in the live config. One bank per call keeps `hook recall` to a single round trip on the per-prompt path, which is the budget that matters | A shared user-profile bank that should be recalled alongside the project bank |
| **Client-side score floors** (`recallMinScores`) | `recall.py:44-72` | `{}` in the live config — already disabled in the system we are matching. AX-2 and both external systems reviewed in Phase B argue against a hard cosine floor: it drops recall to zero on the queries it is meant to sharpen, and precision belongs in the reranker | AX-2 showing **precision**, not recall, is the binding constraint — and then the floor belongs in CE-6, not in a hook |
| **Multi-turn query composition** (`recallContextTurns > 1`) | `recall.py:160-172` | `recallContextTurns: 1` in the live config, so it is off in the system we are matching. Enabling it would also put a transcript read on the **per-prompt** path, which is the one path with a 400 ms budget and a circuit breaker | An AX-2 run showing multi-turn queries beat single-turn on the gold set |
| **Chunked retain mode** (`retainMode: "chunked"`) | `retain.py:117-127` | The live config is `full-session`. The two modes' `document_id` schemes are incompatible, and only one of them is in use | A workload where sliding-window overlap measurably improves extraction |
| **`bankIdPrefix`, channel/user granularity** | `bank.py:85-143` | Openclaw multi-tenant leftovers with no Claude Code caller. `directoryBankMap` **is** ported (one `HashMap` lookup); the rest of the precedence chain has nothing to resolve | A second agent, or a multi-user deployment |
| **`PostCompact` re-injection** | `recall.py:41` names `last_recall.json` as being "for PostCompact re-injection" — but no `PostCompact` hook exists in legacy's `hooks.json`, so it was never wired | **Nothing to port.** We keep the diagnostic file for the same reason legacy does: it is the only record of what the last recall returned | A measured case of context loss across a compaction boundary |

Two Phase C items are **not** gaps and are recorded here so they are not read
as such. The `settings.json` install is a line splice rather than a `Value`
round-trip (C5), and `--mode full` refuses while legacy is wired
(`docs/design/hk-2-cutover-switch.md`) — both are deliberate designs with tests,
not missing features.

## Phase D — the migration

Added by MG-1/MG-2. Everything here is a *deliberate* non-port with a re-entry
criterion; the migration's losses are listed, not implied.

**One thing that is deliberately NOT a row here:** *observations have no
semantic adjacency*. The plan files it as a MemGarden gap; measurement says it
is **parity**. Decomposing `GET /graph` for `bank-a` by endpoint
`fact_type` gives semantic `4,603 == /stats 4,603` and temporal
`3,269 == /stats 3,269` — every observation-touching edge in the projection is
a visualization copy (`memory_engine.py:7723-7724`). **Legacy stores none
either.** MG-2 asserts it as a Tier-1 post-condition instead.

| Gap | Legacy | Why not ported | Re-entry criterion |
|---|---|---|---|
| **The empty banks** — the four named to `--drop-bank` on the measured run, plus `claude-code::memgarden`, which appeared during Phase D | five `banks` rows with a mission and disposition and zero content | Nothing to lose — 0 nodes, 0 documents, 0 links — and `hook session-start`'s `POST /v1/banks` (`session_start.rs:159-166`) recreates any of them on first use. Creating them would put a number in the AC-3 report that overstates what was verified. `banks.json` in every snapshot preserves each mission verbatim, including one bank's hand-written 149-character one, so the string survives the bank not doing so | A user who wants a bank pre-created before its first prompt, or a bank whose mission was hand-tuned away from the default (none is) |
| **Document `original_text` and `document_chunks`** | carried in the transfer archive (`export.py:381`, `_load_chunks`) | No column and no caller (`0001_init.sql:18-27`). The text is read, hashed into the document identity, and discarded; our retain re-derives chunking from the transcript, and the transcript is on disk | A feature that shows the source text behind a fact, or a re-extraction path that must not re-read transcripts |
| **Document `tags`** | `documents.tags`, carried in the archive | `document_tags` exists in our schema and has **zero readers or writers anywhere in the workspace**, and the same tag list is repeated on every fact of the document — measured, 25/25 documents have every document tag on at least one of their facts, so `node_tags` already carries the multiset MG-2 gates on | A caller that filters documents by tag rather than facts |
| **`retain_params.context`** | `"claude-code"` in 25/25 documents | No column, and **our own retain records no equivalent** (`routes/retain.rs:554-568` builds `documents.metadata` without one). Parity, not loss | A second ingest source, at which point "where did this document come from" stops being a constant |
| **Fact `chunk_index`** | every fact carries one | No `chunks` table and no `memory_nodes.chunk_id` column | A UI that shows which transcript chunk a fact came from |
| **`memory_units.state` / the invalidation lifecycle** — `state`, `invalidation_reason`, `invalidated_at`, `edited_at`, `PATCH /memories/{id}`, `/history` | `engine/memories/pg/curation.py` | Measured **0** non-valid facts across all banks. MemGarden has no invalidation surface and no caller for one. `state` selects a *table* rather than adding a predicate (`curation.py:141-143`), so an invalidated fact cannot be exported at all — the exposure is the curation archive being left behind, and **`mg-migrate snapshot` refuses to run if one appears** rather than importing it as valid | Any caller that needs to retract a fact without deleting it |
| **Legacy `memory_units` uuid preservation** | uuid PK, exposed by `/memories/list` | Not in the transfer archive by design (`export.py:171-193`). `(document_id, fact_index)` in `memory_nodes.metadata` is the migration join key instead, and it is the key legacy's own observation provenance uses | An external artifact that keys on legacy uuids — `gold/queries.jsonl` does, which is why AX-2's corpus stays on its own snapshot and is **not** rebuilt from this migration |
| **`documents.created_at` and `memory_nodes.created_at` are the import time** | both carried in the archive | `documents::upsert` and `nodes::insert_batch` write `now_ms()`, and a migration does not get to reshape a store helper retain depends on. Legacy's values are preserved in `metadata.legacy_created_at` and `metadata.legacy.created_at` | A caller that orders by creation and needs the original ordering — at which point `upsert` grows an optional `created_at` |
| **Per-fact `consolidated_at`** | carried in the archive (`export.py:200-206`) | Collapsed into one `consolidation_runs` watermark row per bank: our scheduler reads `id > MAX(watermark)` (`consolidate.rs:314-330`), so one INSERT replaces 3,541 column writes. **The one fact in the corpus with a `consolidation_failed_at` keeps it** in `metadata.legacy.consolidation_failed_at`, because a single watermark rowid cannot say "everything up to here except this one" | A consolidation policy that must distinguish *when* a fact was consolidated, not merely whether |
| **Observation `proof_count`** | stored, with a `or len(source_ids)` fallback (`export.py:457`) | We derive it from `node_sources` unconditionally (`recount_proof_tx`), and `node_sources` collapses duplicate `(document_id, fact_index)` pairs on `INSERT OR IGNORE`. Measured: 93 of 1,747 differ, and 86 duplicate pairs collapse — 2,200 raw to 2,114 distinct. MG-2 reports the 93 and does not gate | A caller that needs legacy's evidence count rather than ours |
| **Semantic and temporal link *values*** (not the types) | 108,744 stored derived edges | Recomputed from the migrated facts by our own rules. Semantic because an edge is a function of the vector space and legacy's vectors are neither exported nor ours. **Temporal because the rules genuinely differ**: legacy's neighbour query applies no 24-hour predicate (`ops_postgresql.py:562-593`) where `links.rs:69` does, so legacy stores edges at the `h ≥ 24` weight floor we would never emit — **72 of them in `bank-a`'s stored fact-to-fact set**. Measured 1.61× fact-to-fact, three replay orders within 2 % of each other | A measured recall regression traceable to the rebuilt graph — which AX-2 is the instrument for |
| **Legacy's `entity` link count** | `/stats` reports 4,124 | Not a port question: legacy **stores zero** and derives the number at read time from `unit_entities` (`counts.py:47-49`). We store zero on purpose (`links.rs:6-8`). Recorded because `/stats` makes it look like content | Nothing — this is exact parity |


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
