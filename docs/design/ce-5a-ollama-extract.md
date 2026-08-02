# CE-5a — Ollama client + extraction prompts (PR B2)

PRD: CE-5 (first half). Plan: `phase-b-impl.md` §PR B2. Legacy reference:
`hindsight-api-slim/hindsight_api/engine/retain/fact_extraction.py`.

## What this adds

- `[ollama]` config section (`memgarden-core/src/config.rs`): base_url/model/
  temperature/num_predict/request_timeout_secs/max_retries/keep_alive/
  max_concurrent, env overrides `MEMGARDEN_OLLAMA_URL`/`_MODEL`, startup
  validation (URL shape, non-zero timeout/concurrency).
- `memgardend/src/ollama.rs`: `OllamaClient` — reqwest against `/api/chat`,
  `stream:false`, `format:<schema>`, semaphore `max_concurrent` (default 1;
  the GPU holds one 14B model), retry with 1s→10s backoff, 15s permit acquire
  timeout (R11), 600s total-deadline cap per call (security M2), fast-fail on
  permanent 4xx. Background `/api/version` prober feeds `/healthz`
  (`ollama: ready|unreachable`, DEGRADED when unreachable) — never probed
  per-request (the gc.collect lesson).
- `memgardend/src/extract/prompts.rs`: the four legacy prompt constants,
  byte-for-byte verbatim (validated in review against `fact_extraction.py`).
  Mission stays out of the system prompt (stable prefix → Ollama KV cache);
  it rides in the user message. The literal JSON output shape is appended to
  the system prompt because `/api/chat` does NOT enforce `format` schemas
  (verified fact, plan §Verified Environment Facts).
- `memgardend/src/extract/parse.rs`: lenient parsing ported from
  `fact_extraction.py:1447-1636` — absent-value semantics, what→factual_core→
  text fallback, `" | "` combined text (no `where`), both response shapes,
  degenerate-text rejection (14-member junk set), causal bounds `0 <= t < i`
  max 2, and survivor-ordinal remap of `target_index` after drops (legacy
  `_remap_causal_relations`; review HIGH finding).
- `POST /v1/banks/{id}/dry-run-extract`: debug/verification endpoint, no
  writes; text ≤32KB, mission ≤4KB (security M1). Errors: 404 unknown bank,
  503 transient (busy/transport/5xx), 502 permanent upstream (4xx/garbage).

## Key decisions

| Decision | Why |
|---|---|
| `num_predict = 8192` (legacy 64000) | 65 tok/s × 64K = 16-min worst case; 8K covers a 3000-char chunk's facts |
| Backoff cap 10s (legacy 60s) | R14: one permit sleeping 60s starves every caller behind it |
| No reranker/sanitize port | Rust String can't hold lone surrogates; serde escapes control chars |
| Zero facts from parse ≠ error; unparseable response after retries = hard error | legacy issue #1833: never silently commit nothing |
| `RawFactsResponse::Wrapped.facts` has no `#[serde(default)]` | with it, ANY object deserializes to 0 facts with no retry (review M-2) |

## Verification

`cargo test --workspace` (110 tests + 2 ignored live), clippy `-D warnings`
clean, live `dry-run-extract` measured in the PR body (GPU shared with the
legacy hindsight daemon during measurement — see PR Notes).
