# CE-9a — Consolidation storage, proof_count, and 0.97 semantic dedup (PR B7)

Branch `feat/ce-9a-consolidation-dedup`. Migration `0004_consolidation.sql`
(schema v4): `memory_nodes.proof_count`, `node_sources`, `consolidation_runs`.

Legacy ports: `engine/consolidation/consolidator.py:116` (top-K),
`:119-147` (the parse-failure default), `:150-171` (the prompt, verbatim),
`:180-182` (the disable rule), `:222-296` (probe → adjudicate → merge),
`config.py:1157` (0.97), `engine/search/reranking.py:173-176` (`proof_norm`).

## What this adds

* `memgarden_store::consolidate` — `insert_observation` (node + embedding +
  `node_sources` + `proof_count`, one `BEGIN IMMEDIATE`), `merge_observation`
  (union sources, rewrite text, recount, delete the candidate — also one),
  `observation_vectors` (the probe's candidate source), `sources_of`,
  `proof_count`.
* `memgarden_store::nodes::update_text` / `update_text_tx` — Critic Revision
  R4's shared text-update path: text **and** `embedding = NULL` **and**
  `DELETE FROM vec_nodes`, so the node goes back on the embed backlog instead
  of answering semantic queries with a vector for text it no longer holds.
  B8's `updates` and B9's refresh use the same function.
* `memgardend::consolidate` — the dedup path: prompt, decision parsing,
  cosine ranking, the token guard, `store_observation`.
* `recall::scoring::proof_norm` — closes B4's neutral-0.5 stub. Wired through
  `search::hydrate` (`CandidateRow.proof_count`) into `scores.proof`.
* `[consolidation] dedup_threshold = 0.97`.

## Pipeline

```text
store_observation(text, embedding, source_ids)
  └─ insert_observation                        ← embedding written HERE (R3)
  ├─ threshold >= 1.0 ──────────────────────────────────────► Created
  └─ observation_vectors(bank, exclude=new_id)  ← cosine scan, not vec0 KNN
       └─ rank: sim >= threshold, desc, top 5
            ├─ empty ─────────────────────────────────────► Created
            └─ first candidate whose prompt fits the budget
                 ├─ none fits ────────────────────────────► Created  (keep)
                 └─ chat_json_background(_DEDUP_PROMPT)
                      ├─ merge → merge_observation ───────► Merged
                      └─ keep / malformed / call failed ──► Created
```

Every arrow that is not "merge" ends in the observation surviving as its own
node. That is the whole safety argument: the only way to lose text is an
explicit, well-formed `merge`.

## The prompt is token-bounded by construction

CE-9's carried-over obligation, and the reason it exists: on 2026-08-02 the
legacy consolidation pinned a GPU for over an hour. A per-fact source-facts
budget (4096 tokens) times an LLM batch size of 8 pushed the assembled prompt
past Ollama's `num_ctx` (16,384); the runner truncated the input with
`keep=4`, which ate the system prompt; the model then rambled past the client
timeout, the call aborted, and the *identical* payload was retried forever.
It was fixed by lowering three config values — i.e. the bound lived in
configuration, one edit away from being gone.

Here it is code:

| | |
|---|---|
| Prompt budget | `DEDUP_PROMPT_MAX_TOKENS = 2048` cl100k tokens, a `const`, counted with the same `retain::token_count` that bounds retain chunks |
| Reply budget | `DEDUP_REPLY_MAX_TOKENS = 256`, also a `const`, applied per call via `OllamaClient::chat_json_background_bounded` |
| Measured against | the **system + user message content**. Not counted: the ~20-30 tokens of chat-template role markers and BOS the server renders around them (negligible against 2048 with 2× headroom, but stated rather than glossed). The `format` schema correctly does *not* enter the context. |
| Margin | Ollama's default `num_ctx` is **4096** and memgardend never overrides it → prompt + reply is 2,304 against 4,096, so the window cannot be exhausted from either end. Against the live fork daemon's 16,384 it is 7×. |
| Template overhead | ~200 tokens, leaving ~1,850 for the two observation texts — a pair of paragraphs, where real observations are a sentence or two. Asserted by `prompt_template_overhead_leaves_room_for_two_observations`. |
| Enforcement | `select_twin` for the prompt, `adjudicate` for the reply — together the only path from this module to Ollama |

**The reply is the incident's second stage, and it needed its own bound.** A
prompt that fits still lets the model start generating, ramble, and exhaust
the window mid-generation — at which point Ollama context-shifts, which is the
same truncation mechanism reached from the far side. `ollama.num_predict`
defaults to **8192**, larger than the whole default context, so the shared
default bounded nothing here; the only real limit was the client's total
deadline, i.e. ~10 GPU-minutes per adjudication instead of the incident's hour.
Two caps close it, both local to this call and neither touching the shared
client's defaults: `num_predict` 256 (a *ceiling* — a config asking for less
keeps its number) and `maxLength: 500` on the schema's `reason`, so the one
free-text field cannot eat the budget `text` needs. A reply that overruns is
cut off mid-JSON → unparseable → `keep`, the safe direction. Verified live
against `qwen3-14b-nothink`: the grammar accepts `maxLength` and the
adjudication got *faster* (2.1 s → 1.0 s).

**Shed order, deterministic:** candidates are considered nearest-first; a
candidate whose assembled prompt would exceed the budget is skipped *whole*
and the next-nearest is tried; if none fits, no call is made and the outcome
is `keep`.

**Nothing is ever truncated.** The prompt asks for a merge that "preserves
EVERY detail from both". Hand the model a text with its tail cut off and it
will cheerfully synthesise a merged observation missing that tail, then the
merge deletes the original — silent data loss, and precisely what the
default-to-`keep` rule exists to prevent. Shedding a candidate costs one
missed deduplication.

Proven by `an_over_budget_pair_is_never_sent` (a ~100k-token twin: zero calls,
twin unmodified), `an_over_budget_candidate_is_shed_in_favour_of_the_next_
nearest` (the shed order, plus a token assertion on the prompt the stub
actually received), and `the_budget_straddles_realistic_pair_sizes_and_is_
monotone`.

Two secondary reasons the incident's shape cannot recur here: this path sends
**one pair per call**, so there is no batch-size multiplier to blow the
budget; and the system message is empty (legacy sends the dedup prompt as a
lone `user` message), so "truncation ate the system prompt" has nothing to
eat.

## Key decisions

**The probe is a cosine scan over observations, not a `vec_nodes` KNN.** vec0
partitions on `bank_id` only, so `MATCH … AND k = 5` returns the five nearest
nodes of *any* fact type. Observations are a small minority of a bank, so the
top-5 would almost always be all facts and dedup would never fire. Legacy gets
the type filter for free by retrieving grouped by fact type
(`consolidator.py:222-227`, `types=["observation"]`); scanning observations
directly reproduces that exactly, with an exact top-K instead of an
over-fetch heuristic. Cost is measured below and the upgrade path is on the
function.

**Create first, then maybe delete.** Legacy adjudicates *before* the insert
and skips the CREATE on a merge; the plan specifies the other order ("and
**delete** the new candidate"), and that is what shipped.

It is worth being precise about *why*, because the obvious explanation is
wrong: this is **not** forced by R3's read-your-own-write. `observation_vectors`
excludes the new id and `rank_candidates` compares against the `embedding`
function parameter, so the new row is never read back. Either order would work.

The real argument is **crash durability**. The candidate's text is the output
of an upstream LLM call; adjudicate-first holds it only in memory across a
second LLM call, so a crash in that window loses work that cost GPU time.
Create-first persists it before spending anything, and the worst a crash can
do is leave a duplicate — which the next round's dedup is exactly the
mechanism for.

**The cost, stated:** between the insert and the merge there is a window —
**~1.0 s measured** against the real Ollama, and up to the client's total
deadline in the worst case — during which the bank contains both observations
and a concurrent recall can return both. Adjudicate-first would not have that
window. It is the right trade for a background path with no read consistency
requirement, but B8 should know it exists before it runs 50 of these per round.

**R3's actual justification.** Synchronous embedding is still genuinely
required, for a different reason than "the probe reads its own write": an
observation must be embedded before the **next** observation can dedup against
it, and in a batch round the next one is milliseconds away — far inside
`embedding.backlog_poll_secs`. Both call sites now say so; the previous wording
was false, and a reader who checked would have been entitled to delete the
exception.

**`proof_count` is always derived, never incremented.** Every write recomputes
it as `count(node_sources)` inside the same transaction, so it cannot drift
from the join table. Legacy does the same
(`count(DISTINCT e)` over the unioned array, `consolidator.py:290`).

**Both substituted slots are JSON-encoded, and substitution is single-pass.**
Observation text is attacker-influenced — it is LLM output over user-supplied
transcripts. Raw interpolation let a text carrying a newline plus a forged
`[EXISTING] …` marker, or a literal `{"action": "merge", "text": "…"}`, open a
second frame inside the prompt and steer the adjudicator into a merge that
rewrites the survivor. The structural controls hold independently (the model
never names its own target; `twin_id` comes from the ranked candidates, never
from the response; source facts are never deleted), which is why this was a
MED and not a HIGH — but that is defence in depth, not a reason to hand a
model a forgeable frame. `serde_json::to_string` on each slot escapes every
newline and quote, so neither can leave its line or close its field. The
**template** is still legacy's verbatim; only the two values are quoted.
Single-pass substitution (`split_once` twice, one `format!`) closes the
mirrored bug: the old two-`replace` chain substituted `{new}` first, so a
`{existing}` planted inside the NEW text was rewritten with the victim's text
on the second pass. `prompt_tokens` routes through `dedup_prompt`, so the
token bound measures the encoded form for free.

**`dedup_threshold` is validated at startup.** The knob deciding whether the
14B model is called at all was config and unchecked; at 0.0 or negative,
`sim >= threshold` holds for every candidate, so every created observation
fires an adjudication against its nearest neighbour however unrelated — at
~1 s each behind `ollama.max_concurrent = 1`, a B8 round serialised for a
minute. `validate` now rejects anything outside `0.5..=1.0`, upper end
inclusive because `>= 1.0` is the documented disable. Same argument this PR
makes about the prompt bound, turned on its own config surface.

**`proof_count` is recounted by a trigger, not only by Rust.** The Rust write
paths recount, but a source fact deleted anywhere — directly, or by a
bank/document FK cascade — removes a `node_sources` row that no Rust code
sees, and the count would stay high forever. `node_sources_ad` fires on the
cascade too, which is why it is a trigger rather than a line in
`nodes::delete` (the same reasoning as `memory_nodes_vec_ad`,
`0001_init.sql:91`).

**The merge checks its target exists, first.** `keep_id` is chosen *before* an
LLM call that takes seconds. If it were deleted meanwhile and the candidate
happened to carry no sources, every statement in the merge would silently
no-op except the `DELETE` — destroying the new observation and merging
nothing. An explicit `SELECT` at the top of the transaction turns that into a
`NotFound` that rolls the whole thing back. It previously "worked" only
incidentally, via `recount_proof_tx`'s trailing diagnostic query hitting
`QueryReturnedNoRows` *after* the delete had already run; correctness must not
depend on a diagnostic.

**One source is exactly neutral.** `proof_norm(1) = 0.5 + ln(1)/10 = 0.5`,
which makes the boost exactly 1.0 — an observation backed by a single fact
must not out-rank a plain fact. `proof_count = 0` (every world/experience
node, by DDL default) is neutral too, matching legacy's `is not None and >= 1`
guard. The clamp bites at `e^5 ≈ 148.4`, capping the signal at the documented
+5%.

## Diverged from legacy

* **The merged observation is re-embedded, not left with the twin's old
  vector.** Legacy keeps it (`consolidator.py:283-285`: "the merged text is
  >= threshold similar, so it stays representative and avoids a re-embed").
  Critic Revision R4 overrides that: `update_text_tx` nulls `embedding`, so
  the embed backlog regenerates it. The legacy argument holds for *this* merge
  but not for B8's `updates` or B9's refresh, which share the function and can
  rewrite text arbitrarily — one path, one rule. Exception, added on review: a
  merge whose text is byte-identical to what the twin already says (the LLM's
  empty-`text` fallback) skips the invalidation entirely, because re-embedding
  an unchanged string is pure loss.
* **R4 said to delete the `vec_nodes` row too; it is deliberately kept.**
  Review evidence: immediately after a merge with the row deleted,
  `search::knn` returned `[]` for the merged twin. A node that *was* indexed
  becoming invisible is worse than an ordinary backlog row that was never
  there, and the window is `embedding.backlog_poll_secs` (5 s) — **unbounded**
  if the embedder failed to load or `embedding.enabled = false`. Leaving the
  row means the twin answers the vector arm with a vector for text it is ≥0.97
  similar to, for at most one backlog tick; `set_embedding` and
  `set_embeddings_batch` both delete-then-insert, so it is replaced, never
  duplicated. One place still sees the gap: `rebuild_vec_index` reinserts only
  from non-NULL embeddings and so drops a row in this state — a manual repair
  op, and the backlog puts it straight back.
* **Threshold-then-truncate, where legacy truncates-then-filters.**
  `rank_candidates` filters to `sim >= threshold` and *then* keeps the top 5;
  legacy takes the global top-5 from retrieval and filters inside that
  (`consolidator.py:229-235`). The resulting set is a superset of legacy's and
  the *nearest* candidate is identical either way, so the adjudicated pair is
  the same in every case except one: when the nearest candidate is shed for
  the token budget, this version can fall through to a 6th-nearest twin that
  legacy would never have seen. That only ever adds a deduplication
  opportunity, never changes a verdict.
* **Observation embedding is synchronous** — the deliberate exception to
  CE-4's async-backlog rule (R3), since dedup depends on reading its own
  write. `insert_observation` takes the vector as a parameter, so there is no
  way to call it without one.
* **The candidate list is exact, not a hosted-search `retrieve_semantic_bm25_
  combined` call.** Legacy's probe goes through the full grouped retrieval
  (semantic + BM25, tag filtering, `tags_match` "all_strict"/"any"). Here it
  is one cosine scan: the threshold is 0.97, where BM25 rank contributes
  nothing a cosine does not already say, and MemGarden's consolidation is not
  tag-scoped (legacy's tag scoping serves per-tenant observation scopes,
  which this deployment does not have).
* **Oracle branches are not ported** — `_dedup_active`'s backend check
  (`consolidator.py:176-183`) exists because the merge uses Postgres-only SQL.
  Only the `>= 1.0` half of that function is behaviour here.
* **No `exclude_id` for an UPDATE path.** Legacy's `_dedup_adjudicate` also
  serves the update path, which must exclude the anchor row to avoid a 1.0
  self-match. B7 only creates; the parameter exists (`observation_vectors`
  takes `exclude_id`) and B8 will use it.
* **`consolidation_runs` is created but not written.** The plan puts the DDL
  in this migration and the writer in B8; splitting a migration across two PRs
  would be worse.

## Known limits

* **A merged twin is invisible to the next dedup probe until it is
  re-embedded.** Keeping the `vec_nodes` row fixes the *recall* half of the
  null-embedding window but not this half: `observation_vectors` requires
  `embedding IS NOT NULL`, so for one backlog tick the merged twin is not a
  dedup candidate. A third near-duplicate arriving inside that window creates
  instead of merging — the very thing R3's synchronous embedding exists to
  prevent — and the duplicate then persists, because dedup is create-time only
  and never revisits existing observations. **Unbounded** if embeddings are
  disabled or the model failed to load. Not fixed here on purpose: the fix is
  to re-embed the merged text synchronously, and this path has no embedder by
  construction (R3's parameter design), while B8 — which owns the caller and
  holds the embedder — can do it in the same round. Handed over below.
* **The probe is a full scan of a bank's observations.** Linear at ~0.8 µs per
  observation (below), and it runs **once per created observation**: B8's
  `batch_size = 50` means 50 scans a round, ~0.4 s at the 10k ceiling rather
  than the 8 ms the per-call number suggests. Memory moves with it — each call
  materialises n × (1536-byte vector + text) at once, ~3 MB at 2k and ~15 MB at
  10k, churned 50 times a round. That is where the
  vec0-with-`(bank_id, fact_type)`-partition upgrade starts paying, and it
  removes the allocation as well as the scan.
* **Cosine is recomputed in Rust from the stored BLOBs**, not by sqlite-vec.
  Same reason as above; the decode is most of the cost.
* **A merge is one LLM call per near-duplicate pair.** With `max_concurrent =
  1` on a 14B model, a consolidation round that produces many near-duplicates
  serialises behind them (~2 s each, measured). B8's round is the place to cap
  that, not here.
* **Only the nearest fitting candidate is adjudicated**, as in legacy — a new
  observation that duplicates *two* existing ones merges into one of them and
  leaves the other. Legacy has the same behaviour; a transitive pass would
  need a fixpoint loop nobody has asked for.
* **`num_ctx` is never set on the Ollama request.** The budget is chosen
  against Ollama's 4096 default so the bound holds whatever model this daemon
  is pointed at; setting it explicitly would be a change to the shared client
  affecting extraction, which is out of scope here.

## Verification

`cargo test --workspace`: **326 passed, 0 failed, 8 ignored** — up from 298 at
CE-8. `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo fmt --all -- --check` clean.

New coverage: schema v4 on a fresh DB **and** on a populated v3 DB upgraded in
place (`proof_count` backfilled to the default, rows intact, the run-ledger
`status` CHECK live); `insert_observation` writing sources + embedding +
`proof_count` in one transaction, with unknown and other-bank source ids
dropped; the merge unioning overlapping source sets, recounting, deleting the
candidate, sparing every source fact, and nulling the stale embedding/vec row;
both cascade directions (observation delete clears `node_sources` and leaves
the facts; source-fact delete clears its provenance row and leaves the
observation); `observation_vectors` excluding self, facts, other banks and
unembedded rows; the prompt verbatim (both JSON shapes surviving as literal
braces); the 0.97 boundary at **0.9699 (no probe) / 0.9701 (probes)**;
`>= 1.0` disabling the path even at similarity 1.0; nine malformed LLM
responses plus a non-JSON body, all → `keep`; the merge end to end; nearest-
first ranking capped at `DEDUP_TOP_K`; the three token-bound tests above;
`proof_norm` at 1 (exactly 0.5) / 3 / 148 / 149 / 150 (clamped 1.0) / 0 / negative,
the ±5% boost at the clamp, and `scores.proof` reaching the recall response
for a 3-source observation, a 1-source observation and a plain fact.

Added in the review round: a forged `[EXISTING]` marker plus a literal
decision object, planted in either slot, producing exactly one `[NEW]` and one
`[EXISTING]` line with quotes escaped; a `{existing}` planted in the NEW text
surviving as literal text with the victim's text appearing exactly once;
`dedup_threshold` rejected at 0.0 / -1.0 / 0.49 / 1.01 / NaN and accepted at
0.5 / 0.97 / 1.0 (with 1.0 round-tripping through TOML); a merge into a
vanished twin failing `NotFound` with the candidate intact; a merge whose text
is unchanged keeping its embedding and staying off the backlog;
`proof_count` recounting to 2 after a source fact is deleted; `update_text`
nulling the embedding, re-queueing the node and leaving it KNN-reachable; and
the shed-path token assertion now counting system + user rather than user
alone.

### Measured — dedup probe latency

`cargo test --release -p memgardend dedup_probe_bench -- --ignored --nocapture`
(in-memory, 200 samples, `observation_vectors` + `rank_candidates`):

```
dedup probe @  500 observations: p50  369us p95  389us max  529us
dedup probe @ 2000 observations: p50 1550us p95 1607us max 1997us
```

Linear, ~0.8 µs per observation. This runs on the consolidation background
path, which has no latency SLO; the number exists to date the upgrade path,
not to gate anything.

### Measured — live merge against real Ollama

`cargo test --release -p memgardend live_dedup_merge -- --ignored --nocapture`,
`qwen3-14b-nothink:latest` on the real daemon:

```
live_dedup_merge: Merged { into: 2, dropped: 3, proof_count: 1 } in 2.1s
  merged text: "After moving embedding inference to CPU, recall p95 for
                MemGarden settled at 20 ms."
```

Two phrasings of one fact ("MemGarden's recall p95 is 20ms after forcing
embeddings onto the CPU" / the above) merged, the candidate node was deleted,
and the twin kept its source fact. 2.1 s per adjudication is the cost to keep
in mind for B8's round.

### Measured — AC-2, size-controlled (`MEMGARDEN_BENCH_CONTROL=1`)

3000 nodes, 2000 requests, five queries, real `bge-small-en-v1.5`. CE-8
recorded the trigger to spend the reserve lever as **controlled loaded
p95 > 50 ms or idle p95 > 15 ms**, and asked B7 to re-run it.

```
                        p50       p90       p95       p99       max     <35ms      <60ms   bg nodes
idle,    CONTROL       7121us    7597us    7833us    8698us   31830us   2000/2000  2000/2000      —
loaded,  CONTROL #1   19550us   43463us   48847us   56781us   60838us   1607/2000  1997/2000  35,736
loaded,  CONTROL #2   19527us   44384us   49657us   57709us   74277us   1606/2000  1985/2000  35,464
CE-8 controlled, ref  19628us   43458us   48964us   57744us   63789us   1598/2000  1993/2000  35,712
```

**Neither trigger fires, and the loaded one is close.** Controlled loaded p95
is **48.85 ms** and **49.66 ms** against the 50 ms trigger — 1.15 ms and
**0.34 ms** of margin. Idle p95 is **7.83 ms** against 15 ms, which is not
close. Stating it plainly: run #2 is within a third of a millisecond of the
threshold CE-8 set, and a single unlucky run at this ingest volume could put
it over.

It is not a CE-9a regression. Both runs bracket CE-8's controlled reference
(48.96 ms) at comparable ingest volume (35.5–35.7k background nodes vs
35.7k), and there is no mechanism for one: CE-9a adds one `i64` column to the
hydrate SELECT and one `ln()` per candidate to scoring, and changes nothing
about the pipeline's shape. Run-to-run spread on the loaded bench has been
~1 ms since CE-7 (48.65 / 48.96 / 48.85 / 49.66), so 49.66 is the top of the
existing band, not a new one. The `<60ms` fraction — CE-8's designated watch
metric, far less jumpy than a tail quantile — is 0.15% / 0.75%, against
CE-7's 0.15% and CE-8's controlled 0.35%.

AC-2 (p50 ≤ 35 ms, p95 ≤ 60 ms) holds in every run, with 10 ms of headroom on
p95.

**What this means for B8.** The trigger exists to buy lead time for the
reserve lever (merging the graph arm's expand+hydrate into the main blocking
hop), and 0.34 ms is not lead time. B8 adds a background consolidation task
contending on exactly the write lock and ONNX mutex that make the loaded case
slow — it is the PR most likely to push this over 50 ms. The handoff below
says what to do about it.

## Handoff to B8 (CE-9b)

Adopted from the architect review; recorded here rather than in a PR comment
so it survives.

**Measurement**

1. **Establish an n ≥ 5 controlled baseline BEFORE writing B8 code** —
   median-of-p95 across 5 runs, plus the observed spread. Two runs cannot
   distinguish 49.66 from 48.85, and the bench is cheap (~55 s).
2. **B8's budget: controlled loaded p95 ≤ 50 ms with the consolidation task
   *enabled and a bank actively consolidating*.** Measuring it idle measures
   nothing. AC-2's p95 ≤ 60 ms remains the hard line. Neither number gets
   renegotiated inside B8.
3. **If the median-of-5 baseline is already ≥ 49 ms, spend the two-hop-merge
   lever FIRST**, on a clean baseline, before adding contention. Landing it as
   a panic fix underneath a regression is a much worse bisect.

**Design constraints, in descending value**

4. **Skip the consolidation tick entirely while a retain job is in flight.**
   Consolidation has no latency SLO and a 300 s interval; deferring during hot
   ingest *removes* the contention window rather than shrinking it, and
   largely moots every finer-grained guard below.
5. Never hold the SQLite write lock across an LLM call.
6. Acquire the ONNX mutex per observation, not per batch.
7. Keep `llm_parallelism = 1` (single local 14B model).
8. **Cap dedup adjudications per round.** ~1 s each behind
   `ollama.max_concurrent = 1`, so a round with 30 near-duplicates serialises
   for half a minute of GPU with nothing else able to use it.

**Correctness debt this PR hands over**

9. **Re-embed a merged twin synchronously**, closing the null-embedding window
   under *Known limits*. B8 owns the caller and holds the embedder; this path
   does not.
10. **Close R4's remaining test half**: null an embedding via
    `nodes::update_text`, run an `embed_task` tick, assert the vector is back.
    B7 tests the invalidation; only B8 has a worker running to test the
    regeneration.
11. Use `observation_vectors`' `exclude_id` on the UPDATE path — it exists for
    exactly that and is unused today.

### Manual verification

The live merge above is the manual verification the plan asks for (two
near-identical observations → merge → resulting `proof_count`), run against
the real Ollama rather than a stub. The `proof_count: 1` in that output is
correct: the twin had one source fact and the candidate had none, so the union
is one.
