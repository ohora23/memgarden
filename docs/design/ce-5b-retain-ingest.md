# CE-5b — Retain ingest, both caps, `file:` tags, ledger (PR B3)

PRD: CE-5 (second half) + the MX-1 `retain_cap_saving` deferral. Plan:
`phase-b-impl.md` §PR B3 + Critic Revisions R11/R14/R15/NIT-16. Legacy
references: the fork's `hindsight-integrations/claude-code/scripts/`
(`lib/content.py`, `lib/config.py`, `retain.py`) and
`hindsight-api-slim/hindsight_api/engine/retain/`.

## What this adds

- **Migration `0002_retain_jobs.sql`** — durable `retain_jobs` (uuid v7 key,
  `pending|running|done|failed`, chunk/fact counters, JSON `detail`), plus
  R14's `chunks_failed`. Durable rather than an in-memory map because the
  Phase C hook renders retention progress and memdash reads it.
- **`memgardend/src/retain/transcript.rs`** — the two fork caps, ported with
  their constants: `TOOL_INPUT_FIELD_MAX = 300` (per string field, suffix
  `"... (+N chars)"`), `TOOL_INPUT_TOTAL_MAX = 1500` (above it only the 8
  priority keys survive plus `_truncated_fields`), `TOOL_RESULT_MAX = 2000`.
  Also `strip_memory_tags` for all three tag families
  (`memgarden_`/`hindsight_`/`relevant_memories` — the middle one matters
  during the parallel-run transition), the `[role: x]…[x:end]` text format,
  assistant-only `tool_use`, MemGarden's own MCP tools skipped, and
  `extract_touched_files` → `file:<relpath>` tags (cap 20, first-touch
  order, cwd-relativized, outside-cwd absolute).
- **Backfill cap**, server-side: last 300 messages, first retain only, `0`
  disables. `MEMGARDEN_RETAIN_MAX_INITIAL_MESSAGES`.
- **`retain/chunk.rs`** — `chunk_text` with whole-turn packing for JSON
  arrays, JSONL line packing, and a recursive separator ladder for plain
  text. `chunk_text(chunk_text(x)) == chunk_text(x)` is a required, tested
  property (legacy issue #2301).
- **`retain/mod.rs`** — `plan_ingest` (sync: normalize twice, count cl100k
  tokens, hash, `file:` tags) and `run_worker` (async: chunk → one Ollama
  call per chunk → `nodes::insert_batch`).
- **`retain_cap_saving` ledger row** on every ingest that saved anything,
  detail `{raw_tokens, capped_tokens, saved, ratio, session_id}`. This
  closes the MX-1 deferral (AC-6).
- **`POST /v1/banks/{id}/retain`** → 202 `{status:"accepted", job_id,
  document_id, raw_tokens, capped_tokens, saved_tokens, saving_ratio}`;
  200 `skipped` / `duplicate`; 400 empty or missing `is_initial`; 404 unknown
  bank; 429 full queue (job slot **or** byte budget).
  **`GET /v1/retain/{job_id}`** → the job row.
- **Two changes outside the strict §PR B3 file list**, declared here because
  they are behavior, not plumbing:
  1. `POST /v1/banks` now inherits `[profile] bank_mission` when the request
     omits a mission — the fork's `ensure_bank_mission`, and without it the
     ported `coding` preset's bank mission would be dead config. An explicit
     mission in the request always wins.
  2. `main.rs` calls `retain_jobs::fail_stale` at startup. The queue is in
     memory, so anything left `pending`/`running` by a crash or restart will
     never be picked up; leaving those rows would wedge the Phase C hook's
     progress view forever.
- **`[retain]` and `[profile]` config**, incl. the `coding` preset verbatim
  from `lib/config.py:74-99` with legacy's precedence (defaults → TOML → env
  → preset fills only unset keys).
- **Metrics**: `retain_chunks_failed`, `retain_cap_savings` added;
  `retain_requests/errors/latency`, `retain_tokens_raw/capped` and
  `nodes_written` now actually move.

## Key decisions

| Decision | Why |
|---|---|
| Caps run server-side, not in the hook | The ledger row is a store concern and the PRD budgets the hook at <10ms (plan decision #4). The hook posts a raw transcript. |
| Token accounting + ledger in the **handler**, extraction in the **worker** | The 202's `saving_ratio` must be a real number, and normalize+tokenize is ms-scale. Only the seconds-scale LLM work is deferred. |
| Bounded `mpsc` (32) with `try_reserve` before any DB write | A flooded queue is a clean 429 with no orphaned document/job rows, never unbounded RAM. |
| `128MB` body limit on the retain route only | The backfill cap runs *after* parsing, so the body has to fit a real first retain; the 102MB-transcript incident is exactly this shape. |
| Content dedup = exact SHA-256 in `documents.metadata`, no new column | Legacy dedups on SHA-256 and nothing else (brief gotcha #5); `0002` is scoped to `retain_jobs`, and `json_extract` reads the key with no JSON dep in the store crate. |
| The hash is stamped by the **worker**, only on a clean run | Review HIGH 1. Written at upsert time, a partially failed job made every re-POST of that transcript a permanent "duplicate" — silent data loss. The hash means "fully ingested", so only the worker can know it. A job that failed or lost a chunk leaves the document hash-less and is retryable. |
| Two-sided queue admission: 32 jobs **and** 256MB of queued transcript | The job count says nothing about RAM (32 × 32MB = 1GB). Body limit lowered 128MB → 32MB to match. |
| `is_initial` is a required field | A `#[serde(default)]` means a caller that forgets it silently takes the *uncapped* branch — precisely the 102MB case the cap exists for. Absent ⇒ 400. |
| Embeddings left `NULL` | **Divergence from legacy** (`orchestrator.py:579,617` embeds inline). Keeps the retain transaction short and reuses B1's backlog worker; R2's semantic-link hook already lives there. |
| Per-chunk failure ≠ job failure (R14) | One flaky LLM call must not discard a whole session. Only an all-chunks failure fails the job; the reason is recorded either way. |
| Wall timeout 7200s per job (R11) | Live-daemon parity. Partial progress stays committed and the job is marked `failed` with the reason. |
| `is_initial` defaults to `false` | Capping a delta retain would silently drop messages; an uncapped initial retain is still bounded by the wall timeout. |
| Unknown `profile.name` is a startup error | Legacy only warns. Running with the wrong missions silently is worse than a refused boot; matches the existing `[ollama]` validation style. |
| Stub-Ollama integration tests, no mock trait | The whole path (HTTP, retries, chunking, R14) is exercised without a trait/impl pair that exists only for tests. |

## Carried-over obligations from the CE-5a review

- **Permit not held across chunks, and the worker no longer times out
  waiting for it.** `chat_json` drops the permit at the end of each call, so
  the worker holds nothing between chunks — but that alone was not enough
  (review HIGH 2): the worker was reusing the *interactive* acquire path, so
  a retain chunk unlucky enough to arrive behind a 20s `/dry-run-extract`
  would give up with `Busy` after 15s and be written off. `ollama.rs` now
  has two entry points: `chat_json` (15s acquire timeout, HTTP handlers) and
  `chat_json_background` (untimed acquire, the retain worker — its bound is
  the job's own wall clock). `Busy`/`Deadline` additionally get one retry per
  chunk. Proven by
  `ollama::tests::background_acquire_waits_out_a_holder_that_defeats_the_interactive_timeout`,
  which is the suite's one slow test (~16s: the threshold under test is 15s,
  so it cannot be proven faster).
- **Disconnect vs. permit — still open.** `reqwest` gives no
  client-disconnect signal, so a caller who hangs up mid-generation still
  burns the permit until Ollama answers. The per-call deadline in
  `ollama.rs` (`TOTAL_DEADLINE_CAP`, 600s) remains the only bound.
  Documented at the call site in `retain/mod.rs`; revisit if a real
  starvation case appears.
- **Degenerate/empty chunks never call Ollama.** `run_job` gates every chunk
  on `extract::parse::is_degenerate_text`, and the chunker never emits a
  blank piece.

## Deliberate non-ports / divergences

- `_is_channel_message_tool` (Telegram/Slack `tool_use` → text) — no channel
  plugin exists here; such blocks are retained as ordinary capped tool calls.
- The last-turn slicing branch of `prepare_retention_transcript` — which
  messages are new is the hook's delta bookkeeping (Phase C).
- `serde_json` compact separators vs Python's `", "` / `": "` make the 1500
  serialized-total boundary marginally more permissive; tier 1 does the
  heavy lifting. `serde_json::Map` also sorts keys, so `_truncated_fields`
  comes out alphabetical rather than in insertion order.
- `split_oversized_unit` is a hand-rolled separator ladder, not a byte-exact
  LangChain `RecursiveCharacterTextSplitter`. It guarantees what the pipeline
  depends on: every piece ≤ budget, earlier separators preferred, idempotent.
- The legacy `<10 character` transcript floor is ported as written; the
  `[role: …]` markers make it unreachable in the text path.
- Two chunker off-by-parity fixes against legacy, both required for the
  idempotence invariant: a turn is kept whole only when `unit_size + 2` fits
  (the enclosing `[]`, review LOW 11), and the lone-JSON-object branch drops
  legacy's unreachable "keep whole" arm (review LOW 12).

## Input bounds (security review)

Everything below crosses a trust boundary — caller-supplied JSON, or LLM
output steered by caller-supplied JSON — and none of it had a natural bound:

| Input | Bound |
|---|---|
| request body | 32MB (retain route only; every other route keeps axum's 2MB) |
| queued transcripts | 256MB total across the queue, else 429 |
| `tags[]` | ≤32 tags, ≤128 chars each, empty and control-char tags dropped |
| `file_path` / `notebook_path` | ≤512 chars, no control characters |
| one fact's combined text | ≤2000 chars, else the fact is dropped |
| a fact's `entities[]` | ≤32 entries, ≤256 chars each |
| `doc_key` in logs | logged as `Debug`, truncated to 128 chars |

## Phase C hook obligations

The daemon owns the caps; the hook still owns the bookkeeping that tells the
daemon *what* to send. Two contracts Phase C must honour:

1. **`is_initial` is required and must be computed**, not hardcoded — it is
   `true` exactly for a session's first retain (the fork derives it as
   `retention_progress.start_index == 0`). Sending `true` on every retain
   truncates deltas; sending `false` on the first one skips the backfill cap.
2. **Send deltas, not the growing window.** A re-POST of a transcript that
   merely grew has a different content hash, so it is not a duplicate and
   the whole window is re-extracted. The fork's `plan_retention` /
   `commit_retention` state is what prevents that, and it stays hook-side.

## Verification

`cargo test --workspace`: 189 passed, 3 ignored (2 pre-existing live tests +
`live_retain`). `cargo clippy --workspace --all-targets -- -D warnings`
clean. Coverage: both caps at their exact boundaries, all three memory tag
families, backfill cap first-retain-only semantics with `0` disabling,
`file:` tag relativization/dedup/`mcp__x__Edit`/cap-20/oversize+control-char
rejection, tag sanitization, chunker idempotence (conversation, JSONL, plain
text, Korean, hard split), `+10ms` document-absolute offsets, ledger row
written with the right ratio and *not* written when nothing was saved,
duplicate detection *only after* a clean ingest, a partially failed job
staying retryable, 429 on a full job queue, the byte budget rolling back a
refused reservation, body-limit scoping, R14 partial-failure and
all-failure paths, wall timeout, background-vs-interactive permit acquire,
fact-text and entity bounds, and the `0002` migration applying to both a
fresh DB and one created at schema v1.

Manual: `cargo test -p memgardend --test retain_api live_retain -- --ignored
--nocapture`, against the real Ollama (`qwen3-14b-nothink:latest`):

```
live_retain: raw=6912 capped=1707 saved=5205 ratio=0.753
live_retain: status=done chunks=2/3 failed=1 facts=2 wall=564.5s
live_retain: ledger retain_cap_saving
  {"raw_tokens":6912,"capped_tokens":1707,"saved":5205,
   "ratio":0.7530381944444444,"session_id":"live-session"}
```

**−75.3%**, inside `docs/measurement.md`'s −55% / −87% band, and the
`benefit_ledger` row is written automatically.

Two things that run reads as expected but are worth stating plainly:

- One of three chunks failed and the job still completed with the other two
  written — R14's partial-failure policy exercised for real, not just
  against the stub.
- 564s wall for three chunks is far above the plan's 1.5–4s/chunk estimate.
  Two causes, neither a defect in this PR: the GPU was shared with the live
  legacy hindsight daemon during the run, and the fixture transcript is
  deliberately pathological (a 120×-repeated edit) so `num_predict = 8192`
  gets a lot of room. It is well inside the 7200s wall timeout. Re-measure
  on an idle GPU before quoting a retain-throughput number anywhere.
