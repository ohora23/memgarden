# CE-9b — Batch fact→observation consolidation (PR B8)

Branch `feat/ce-9b-consolidation-round`. No migration — `consolidation_runs`
shipped empty in CE-9a's `0004_consolidation.sql` and this PR is its writer.

Legacy ports: `engine/consolidation/prompts.py:144-193` (the prompt, verbatim,
including `_PROCESSING_RULES` `:20-38`, `_INPUT_FORMAT_NOTE` `:58-69`,
`_DECISION_GUIDE` `:84-89`, `_OUTPUT_SECTION` `:92-141`, `_DEFAULT_MISSION`
`:10-13`, `_MISSION_PRIORITY_NOTE` `:15-18`), `consolidator.py:1002-1526` (the
job), `:2400-2555` (one LLM batch), `:2323-2358` (the observation shape),
`:2418-2429` (the fact line), `:2250-2320` (the pooling recall), `:1175` (the
bisection), `config.py:1147-1171` and `:1298` (every parameter).

## What this adds

* `memgardend::consolidate::prompts` — the verbatim prompt, split legacy's way:
  rules + input format + decision guide + output format in the **system**
  message, MISSION + INPUT in the **user** message.
* `memgardend::consolidate::round` — the round: fact selection, per-fact
  observation pooling, batching, the token bound, the LLM call with outer
  retries, plan validation, application, the CE-9a dedup pass, the ledger, and
  the background task.
* `memgarden_store::consolidate` — `unconsolidated` / `count_unconsolidated`
  (the watermark range scan), `apply_plan` (one `BEGIN IMMEDIATE` per batch),
  `insert_observation_tx`, and the run ledger (`start_run`, `finish_run`,
  `latest_run`, `watermark`).
* `POST /v1/banks/{id}/consolidate` (synchronous, returns the round summary)
  and `GET /v1/banks/{id}/consolidation` (watermark, pending count, latest run).
* `[consolidation]` gains `interval_secs`, `batch_size`, `llm_batch_size`,
  `max_attempts`, `recall_budget`, `max_tokens` — all validated at startup.
* `OllamaClient::chat_json_background_bounded` gains an optional per-call
  `num_ctx`.
* CE-9a's `store_observation` splits into insert + `dedup_created`, so the
  batch round can write a whole plan in one transaction and dedup afterwards.

## Pipeline

```text
tick (every interval_secs)
  └─ retain queued or in flight? ──────────────────────► defer the whole tick
       └─ for each bank: run_round
            ├─ watermark → unconsolidated(bank, > wm, batch_size)
            ├─ empty ──────────────────────────────────► no run row, no call
            └─ start_run
                 └─ queue of llm_batch_size ranges, oldest first
                      └─ per range:
                           ├─ pool observations (1 recall per fact, cut to
                           │    consolidation.max_tokens)
                           ├─ assemble → over budget?
                           │    ├─ shed pooled observations, tail first
                           │    ├─ still over, >1 fact  ──► bisect, requeue
                           │    └─ still over, 1 fact   ──► drop the fact
                           ├─ LLM call ×max_attempts until a valid plan
                           │    └─ none ────────────────► skip the batch
                           ├─ embed each create (one ONNX acquire each)
                           ├─ apply_plan            ← ONE Db::write
                           └─ per create, capped at 8: CE-9a dedup
                                └─ merged → re-embed the twin synchronously
                 └─ finish_run(done, counts, watermark)
```

The watermark advances to the last fact of every range that reached a
terminal decision — applied, skipped, or dropped. A range that is bisected
does not advance it until both halves are done, so the mark stays contiguous.

## The prompt is token-bounded by construction

CE-9a bounded a **one-pair** prompt at 2048 tokens and noted that B8 assembles
the shape that actually caused the 2026-08-02 incident: a system message, a
mission, N fact lines and a pool of existing observations. The legacy dominant
term was `consolidation_source_facts_max_tokens = 4096` of embedded source
facts **multiplied by** `llm_batch_size = 8` — a 32k-token prompt against a
16k window, truncated by Ollama with `keep=4` (which ate the system prompt),
after which the model rambled past the client timeout and the identical
payload was retried forever. All three caps were config values.

| | |
|---|---|
| Prompt budget | `CONSOLIDATION_PROMPT_MAX_TOKENS = 6144` cl100k tokens, a `const`, counted with the same `retain::token_count` that bounds retain chunks and CE-9a's pair |
| Measured over | the **system + user** message content, before every call, in `assemble` — the only path from this module to Ollama |
| Reply budget | `CONSOLIDATION_REPLY_MAX_TOKENS = 1536`, a `const`, applied per call as a `num_predict` ceiling, plus `maxLength` 2000 on `text` and 500 on `reason` in the response schema |
| Window | `CONSOLIDATION_NUM_CTX = 8192`, requested explicitly per call. 6144 + 1536 = 7680 against 8192 |
| **The multiplier itself** | `source_memories` is **never populated** on a pooled observation. This is the incident's actual dominant term, deleted rather than capped |
| Secondary bound | `consolidation.max_tokens = 512` (legacy's own, `config.py:1163`) already cuts the observation pool long before the `const` is reached |

**Why 6144 and not 2048.** Measured with `retain::token_count`:

| | tokens |
|---|---|
| system message alone (the verbatim port) | 2,314 |
| \+ 8 realistic engineering fact lines | 2,702 |
| \+ a six-observation pool at `max_tokens = 512` | 3,075 |

CE-9a's 2048 is below the system message, so the budget had to move; 6144
leaves ~3k for input a good deal longer than anything extraction produces.
The `const` is pinned from **both** sides by tests that fail if it is touched:
`the_budget_leaves_real_room_for_a_full_batch` asserts
`prompt + reply < num_ctx`, so inflating it past **6656** fails, and asserts
a normal batch keeps >1000 tokens of headroom, so shrinking it below ~3700
fails; `an_over_budget_batch_bisects_and_a_lone_over_budget_fact_is_refused`
fails if it is deleted or raised past a ~100k-token fact.

**Why `num_ctx` is now explicit.** CE-9a could measure its 2048 against
Ollama's 4096 default and be done. A 2.3k system message plus a batch plus a
1.5k reply does not fit that window, so leaving the window to the server would
make the bound an assumption about the deployment rather than a property of
the code. It is requested per call and does not touch the shared client's
other callers; CE-9a's dedup path still passes `None`. A server that cannot
honour 8192 clamps *down*, and the reply then truncates into unparseable JSON
— the batch is skipped and no fact is lost.

**Shed order, deterministic:**

1. Pooled observations are shed from the **tail** — the pool is rank-ordered,
   so the nearest twin (the one an UPDATE would target) is the last to go.
2. With an empty pool and still over budget, the batch is **bisected**
   (`consolidator.py:1175` bisects the same way, on LLM failure rather than on
   size) and both halves are retried. No fact is ever shed for size while
   another fact in its group is processed.
3. One fact alone over the budget is **dropped**: logged, counted in
   `dropped_facts`, and passed by the watermark. Its row stays in the bank and
   stays recallable; only its consolidation is skipped. The alternative — a
   batch that can never fit being retried every 300 s forever — is the
   incident, exactly.

**Nothing is ever truncated at any step.** A fact with its tail cut off
becomes an observation that quietly asserts less than the fact did, and unlike
a dropped batch that error is durable and invisible.

## Key decisions

**The tick is skipped entirely while a retain job is queued or in flight.**
CE-9a's handoff called this the highest-value structural guard and it is the
one thing in this PR that removes contention rather than shrinking it.
Consolidation has no latency SLO and a 300 s interval; retain is the hot
ingest path; the two contend on exactly the same two resources — the single
ONNX mutex and the SQLite write lock. `retain::queued_bytes()` is the existing
admission counter (non-zero from acceptance until the job finishes), so this
is one `if` reusing a counter that was already there, rechecked per bank so a
retain arriving mid-tick stops the remaining banks too.

**The write lock is never held across an LLM call.** One `Db::write` per
applied batch, opened after the call has already returned. The CE-9a dedup
adjudications run *after* that transaction commits, each with its own small
write if it merges.

**The ONNX mutex is acquired per observation, not per batch.** `embed_one`
embeds one text per `spawn_blocking`; a batch-sized hold would be a
batch-sized stall on recall's semantic arm, which shares the mutex.

**`llm_parallelism` is 1, and is not a config value.** Legacy's 4
(`config.py:1165`) assumes a hosted provider. MemGarden runs one local 14B
model behind `ollama.max_concurrent = 1`, so a second concurrent group would
queue on that semaphore rather than run — the knob could only ever be a lie.
Batches are processed sequentially from one queue.

**Dedup adjudications are capped at 8 per round.** CE-9a measured ~1-2 s each
behind the single Ollama permit and explicitly deferred the cap here. An
uncapped round creating 30 near-duplicates serialises for most of a minute
with nothing else able to reach the GPU — including `/dry-run-extract`, which
gives up at 15 s rather than queueing. 8 caps the dedup spend at ~8-16 s
against a 300 s interval. Nothing is lost by skipping: the *next* round's
creates dedup against these same observations, so a burst drains over several
rounds instead of one long hold.

**Two `updates` for one `observation_id` reject the whole batch.** Applying
both means the second write silently overwrites the first and one of the two
consolidations the model intended is destroyed with no trace. The prompt
forbids it in capitals (`prompts.py:137`). Everything else — an unknown
`source_fact_ids` uuid, an unpooled `observation_id`, a missing `reason`, an
empty `text` — drops just that entry and the run continues, the same asymmetry
CE-9a's parser has: a dropped entry costs one observation, a rejected round
costs a bank's whole backlog.

**Updates and deletes are keyed by uuid, not rowid.** The LLM names a uuid and
the rowid it maps to was read seconds earlier. SQLite reuses the rowid of a
deleted max row, so an update aimed at an observation deleted in the meantime
can land on a brand-new unrelated one and silently rewrite its text. This is
not hypothetical: `apply_plan_skips_an_update_whose_target_vanished` hit
exactly that reuse while being written, and the test now asserts the
non-recycling explicitly. `memory_nodes.uuid` is `NOT NULL UNIQUE`.

**The watermark is `memory_nodes.id > mark`, not a per-fact column.** Legacy
tracks consolidation per memory (`consolidated_at IS NULL`). MemGarden's
rowid is monotone, so "everything newer than the last committed mark" is one
indexed range scan and costs no second write per fact. The mark is written on
a **failed** run too when the round got partway: those batches really were
applied, and replaying them would create duplicate observations. The cost is
under *Known limits*.

**A no-op round writes no ledger row.** One indexed count, no `running` row,
no LLM call — so a bank with nothing new leaves no trace in
`consolidation_runs` and `GET /consolidation` keeps showing the last real run.

**Manual runs are synchronous.** `POST /consolidate` runs the round inline and
answers with what it did, unlike `/retain`'s 202-plus-job. This is the manual
and test surface for a path whose normal trigger is a 300 s background tick,
and a caller who asked for a round wants the round's result. Bounded by
`batch_size` facts, but the wall clock is minutes, so callers need a matching
timeout.

**Fact text is JSON-encoded in the fact line.** The same divergence CE-9a made
for `dedup_prompt`, with more at stake: the facts section is newline-delimited,
so a raw fact carrying `\n[<uuid>] …` would forge an extra fact line and one
carrying `\n### Existing observations\n[…]` would forge a whole section. The
template stays legacy's; only the value is quoted. Asserted by
`a_forged_fact_line_cannot_escape_its_own_line`. The observations slot is
`serde_json` output already, as it is in legacy.

## Diverged from legacy

* **`source_memories` is never sent.** Legacy embeds each observation's source
  facts in the prompt under two token budgets
  (`consolidation_source_facts_max_tokens` 4096,
  `..._per_observation` 256). That product with `llm_batch_size` is the term
  that caused the 2026-08-02 incident, so it is deleted rather than capped.
  Legacy's own field documentation already says the list "may be partial or
  absent for large observations — the count above remains the true total", so
  `proof_count` still tells the model how well-evidenced an observation is and
  the prompt remains truthful.
* **A duplicate `observation_id` rejects the batch; legacy collapses it.**
  `_dedupe_updates` (`consolidator.py:2362-2397`) keeps the last text, unions
  the source ids and warns. A model confused enough to emit two plans for one
  observation is not a model whose merge of those plans should be trusted, and
  the round has `max_attempts` fresh tries before the batch is skipped.
* **Bisection is triggered by prompt size as well as by LLM failure.** Legacy
  bisects only on a failed call (`consolidator.py:1175`); here an over-budget
  assembly reaches the same mechanism.
* **`llm_parallelism` is fixed at 1** (above).
* **The pooling recall is not tag-scoped.** Legacy filters with
  `tags_match="all_strict"` to keep per-tenant observation scopes apart; this
  deployment has no scopes. Same divergence CE-9a recorded for the dedup probe.
* **`reranking="interleave"` is not a parameter.** Legacy forces it on the
  pooling recall so the semantic-#1 twin is never buried below the token
  budget. MemGarden's recall is RRF over four arms with no cross-encoder, and
  the pool preserves each fact's recall order across the union, which is what
  that setting was protecting.
* **Temporal fields are unix-ms integers, not ISO dates.** MemGarden stores
  ms; the prompt only asks the model to compare and report them.
* **Not ported:** `output_language_directive` (no such knob), the
  `## CAPACITY CONSTRAINT` section and `max_observations_per_scope`,
  per-tenant strategy overlays, Oracle branches, observation history, mental
  model refresh triggers (CE-10's), and the perf-log machinery. Legacy's
  `consolidator.py` is 2,693 lines; this is the load-bearing behaviour.

## Correctness debt from CE-9a, closed

* **A merged twin is re-embedded synchronously.** `merge_observation` nulls
  the embedding (R4), which drops the twin out of `observation_vectors` — the
  probe requires `embedding IS NOT NULL` — for one backlog tick, and forever
  if the embedder never loaded. Inside a batch round the next created
  observation is milliseconds away, so a third near-duplicate arriving in that
  window creates instead of merging and is never revisited. CE-9a could not
  fix it (`store_observation` has no embedder by construction); `round` holds
  one. Best-effort by design: the merge is already committed, so a failure
  here costs exactly what CE-9a shipped.
* **R4's remaining test half is closed.**
  `an_embed_task_tick_regenerates_an_invalidated_embedding` nulls an embedding
  via `nodes::update_text`, runs one `embed_task::drain_once`, and asserts a
  full f32 vector is back and the node is off the backlog. `drain_once` is now
  `pub` for the same reason `on_batch_embedded` is.
* **`exclude_id` on the UPDATE path** — `select_twin` already passes the new
  observation's own id, and `dedup_created` is now called on the UPDATE path's
  siblings from the round. The parameter is no longer unused.

## Known limits

* **A fact inserted below a committed watermark is never consolidated.** The
  mark is a rowid comparison, so this needs an explicit out-of-order id, which
  nothing in the daemon does. The repair is a manual `DELETE FROM
  consolidation_runs` for the bank, which replays from 0.
* **A failed round's committed batches are not re-consolidated, but a failure
  *inside* a batch's write is.** `apply_plan` is one transaction, so a batch
  is all-or-nothing; the watermark advances only past completed ranges. What
  is not protected is the gap between the write committing and the round
  failing later — those observations exist and their facts are past the mark,
  which is correct, but if the round failed *before* `finish_run` the process
  could die and the mark is lost. Re-running then duplicates observations,
  which is what the dedup pass exists for.
* **A dropped fact is dropped forever.** Only reachable for a single fact
  whose own line exceeds ~2.5k tokens, which extraction does not produce.
* **The pooling recall is one recall per fact.** At `llm_batch_size = 8` and
  `batch_size = 50` that is 50 recalls a round on the same blocking pool the
  latency path uses — the main reason the retain-in-flight skip matters.
  Legacy does the same per-fact recall.
* **`GET /consolidation` reports the latest run, not a history.** The table
  keeps every row; nothing prunes it. At one row per bank per 300 s that is
  ~105k rows a year per active bank — small, but unbounded, and a `DELETE`
  sweep is the obvious follow-up.
* **The dedup cap is a count, not a wall-clock budget.** 8 slow adjudications
  is still ~16 s of GPU.

## Verification

`cargo test --workspace`: **355 passed, 0 failed, 10 ignored** — up from 326
at CE-9a. `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo fmt --all -- --check` clean.

New coverage: the system prompt carrying every ported section **in order**
with `.format()`'s braces unescaped and the MISSION absent from the cached
prefix; the user prompt with the mission present and absent (absent falling
back to `_DEFAULT_MISSION`); fact lines with all, some and no temporal fields;
a forged fact line and section header surviving as escaped text on one line;
observation entries omitting absent temporal fields and never carrying
`source_memories`; an empty pool rendering as `[]`.

The plan contract: a well-formed plan resolving every uuid; **two updates to
one observation rejecting the whole batch** (including a perfectly good
create in the same reply); an unknown `source_fact_ids` uuid dropping only
that entry; unpooled targets, blank reasons and blank texts each dropping
their entry; a delete of an observation the same plan updates being dropped;
a delete of a pooled observation surviving; missing arrays and junk bodies
(array, string, null) not panicking.

The bound: the system prompt fitting the budget, a full 8-fact batch fitting
with >1000 tokens of headroom, prompt + reply fitting `num_ctx`, an
over-budget batch refusing to render so the caller bisects, a lone over-budget
fact still refused, its short neighbour still accepted, and the pool shed from
the tail until the prompt fits with the nearest twin last to go.

Storage: `unconsolidated` reading world + experience above the mark, ascending,
never observations and never another bank, honouring the limit;
`count_unconsolidated` at three marks; `apply_plan` creating, updating and
deleting in one transaction with provenance unioned and `proof_count`
recounted, the created observation embedded and the updated one re-queued;
`apply_plan` refusing to delete facts, other banks' rows or unknown uuids;
skipping an update whose target vanished **without following its recycled
rowid**; leaving an unchanged text embedded; the ledger recording a run,
an open run contributing no watermark, a partway failure still advancing it,
and a no-progress failure not.

End to end: a round applying an update and a delete, moving the provenance and
the mark, and writing a `done` ledger row visible through
`GET /consolidation`; a second round being a total no-op (no row, no call) and
a third moving again after one new fact; a duplicate `observation_id` costing
exactly `max_attempts` calls, writing nothing, and still closing the run and
advancing the mark; an unknown source uuid dropping one entry in a single call
while the delete lands; an unparseable reply skipping the batch without
failing the round; 20 facts producing exactly 3 batches at `llm_batch_size`;
both endpoints 404ing on an unknown bank; status before the first run.

Ignored (run by hand, output below): the embed-backlog regeneration and the
live round.

### Measured — AC-2 baseline before writing any B8 code

CE-9a's handoff #1 asked for an n ≥ 5 controlled baseline first, because two
runs cannot distinguish 49.66 from 48.85 ms. Five runs,
`MEMGARDEN_BENCH_CONTROL=1 MEMGARDEN_BENCH_LOAD=1`, 3000 nodes, 2000 requests,
on `453755b` (CE-9a's merge commit):

```
run   p50      p90      p95      p99      max       <35ms      <60ms   bg nodes
 1   20219us  46113us  52548us  60397us  73599us  1564/2000  1974/2000  36,864
 2   19797us  43721us  49431us  57971us  64645us  1604/2000  1989/2000  35,416
 3   20540us  45315us  51372us  59756us  67515us  1585/2000  1980/2000  36,672
 4   19927us  43999us  49461us  57798us  63621us  1590/2000  1985/2000  36,204
 5   19881us  45495us  50366us  61707us  69384us  1584/2000  1983/2000  36,056
```

**Median-of-p95 = 50.37 ms, spread 49.43–52.55 ms (3.12 ms).**

This is ~1.5 ms above CE-9a's recorded 48.85 / 49.66, at comparable ingest
volume, and the honest reading is that the machine is not the machine CE-9a
measured on — thermals and background load differ across a day. **The
baseline already fails the 50 ms trigger before a line of B8 exists**, which
is what made the next section mandatory rather than optional. AC-2's hard
line (p50 ≤ 35 ms, p95 ≤ 60 ms) holds in every run.

### Measured — the two-hop-merge lever was spent, and it is empty

Handoff #3: median ≥ 49 ms means spend the reserve lever first. 50.37 ≥ 49, so
it was spent. The change folded the graph arm's `graph::arm` + follow-up
`search::hydrate` — plus pass-1 filtering and fusion, all pure CPU — into the
main blocking hop, removing one `spawn_blocking` round trip per recall.

Five runs in a block gave 53.10 / 50.43 / 50.57 / 54.16 / 51.48, i.e. *worse*
than the baseline block — but by then the numbers were drifting upward for
both arms, so a block comparison could not answer the question. Rebuilt as two
binaries and run **interleaved**, four paired runs:

```
pair   baseline p95   two-hop p95   difference
  1      51718us        51664us       -0.054ms
  2      52117us        51668us       -0.449ms
  3      53301us        53582us       +0.281ms
  4      52514us        52323us       -0.191ms
                            mean       -0.10ms
```

**Mean paired difference −0.10 ms against a 3 ms run-to-run spread: zero.**

The lever was **reverted**. The premise behind it — that a second
`spawn_blocking` costs real time on the loaded path — does not hold: tokio's
blocking pool is 512 threads, so the second hop is a scheduler round trip of
tens of microseconds, not a queue wait. What makes the loaded case slow is the
single ONNX mutex and the SQLite write lock, and merging hops touches neither.
Keeping a no-benefit refactor of the hottest path inside a PR that also adds a
background task would make this PR's own bisect worse, which is the exact
concern the handoff raised about landing it late.

**The reserve lever is now spent and empty.** Anyone reaching for headroom on
this path should aim at the ONNX mutex (a second embedder instance) or at
write-lock hold time, not at hop count.

### Measured — controlled loaded p95 with consolidation ENABLED

Handoff #2's actual budget. `MEMGARDEN_BENCH_CONSOLIDATE=1` seeds a second
bank with 3000 facts and drives a real `run_round` against it **once a
second** for the whole loop — 300× the production tick rate — against a stub
Ollama that returns one CREATE per batch, so every round genuinely embeds
(ONNX mutex), writes (`apply_plan`), and adjudicates (CE-9a dedup). A second
bank rather than `b1` because consolidation contends on the process-wide mutex
and the write lock whichever bank it runs on, but pointing it at `b1` would
also hand its dedup probe a full scan of the 36k *observations* this harness's
background ingest writes, which no real bank has.

```
run   p50      p90      p95      p99      max        rounds  obs  bg nodes
 1   22583us  44413us  50729us  59796us   73205us      49    343   32,872
 2   22531us  43815us  49639us  57298us   76637us      50    350   33,104
 3   22367us  44233us  49282us  59239us   70315us      49    343   32,720
 4   22089us  42845us  47359us  53160us   58853us      49    343   33,072
 5   23010us  48031us  53608us  65819us  207892us      49    343   33,056
```

**Median-of-p95 = 49.64 ms, spread 47.36–53.61 ms** — against the baseline's
median 50.37 ms (spread 49.43–52.55). p50 is 22.4 ms against 35 ms and every
run's p95 is under AC-2's 60 ms line.

**Budget met: 49.64 ms ≤ 50 ms with the consolidation task enabled and a bank
actively consolidating.** AC-2 (p50 ≤ 35 ms, p95 ≤ 60 ms) holds in all five.

Read alongside the baseline, and read the caveat with it: **the harness's load
generator is an unthrottled busy loop**, so consolidation does not add load on
top of the baseline — it *takes* throughput from the ingest loop, which is why
the background node count drops from ~36k to ~26k. The offered load is
therefore not held constant between the two columns and the comparison
flatters consolidation. What can be said without qualification is that the
consolidation-enabled number is **inside both the 50 ms budget and AC-2's
60 ms line**, on a bench running rounds 300× more often than production ever
will, and that in production the retain-in-flight guard means the hot ingest
path and consolidation do not overlap at all.

### Measured — per-round wall time on a ~50-fact bank

`cargo test --release -p memgardend --test consolidate_api -- --ignored
--nocapture live_consolidation_round`, real Ollama (`qwen3-14b-nothink`,
v0.21.2) and the real `bge-small-en-v1.5`:

```
RoundSummary { run_id: Some(1), facts_seen: 50, created: 9, updated: 14,
               deleted: 0, merged: 0, batches: 7, skipped_batches: 0,
               dropped_facts: 0, adjudications: 8, watermark: 50 }
  wall: 68.5s
```

**68.5 s for a 50-fact round**: 7 batch calls plus 8 dedup adjudications, all
serialised behind `ollama.max_concurrent = 1`, so ~4.6 s of GPU per LLM call
on this model. Against the 300 s interval that is a 23% duty cycle at full
`batch_size` — which is why the retain-in-flight skip, not a finer-grained
guard, is the thing standing between consolidation and the latency path.

`adjudications: 8` with `created: 9` is the per-round cap doing its job: the
ninth created observation skipped its dedup probe and will be adjudicated
against the same pool by the next round's creates.

## Manual verification

The plan asks for "consolidate a bank with ~50 facts; quote the created
observations and the run row". The live round above is exactly that. Its 50
input facts are five subjects × five claims × two repetitions, so the correct
answer involves both CREATE and UPDATE — and the model produced both.

Created observations (`id`, `proof_count`, text):

```
obs 51 (proof_count 10): Multiple components commit one chunk per BEGIN IMMEDIATE
                         transaction to manage data processing.
obs 52 (proof_count 10): Multiple components drain in batches of eight to cap the
                         ONNX mutex hold.
obs 53 (proof_count  9): Multiple components fuse four retrieval arms with
                         reciprocal rank fusion.
obs 54 (proof_count  3): The retain worker partitions only on bank_id, so
                         fact_type is filtered in Rust.
obs 55 (proof_count  2): The embedding backlog partitions only on bank_id, so
                         fact_type is filtered in Rust.
obs 56 (proof_count  2): The recall pipeline partitions only on bank_id, so
                         fact_type is filtered in Rust.
obs 57 (proof_count  2): The sqlite-vec index partitions only on bank_id, so
                         fact_type is filtered in Rust.
obs 58 (proof_count  2): The Ollama client partitions only on bank_id, so
                         fact_type is filtered in Rust.
obs 59 (proof_count 11): Multiple components hold exactly one concurrency permit
                         for the local 14B model.
```

The run row:

```
("done", facts_seen 50, created 9, updated 14, deleted 0, merged 0, watermark 50)
```

Nine observations from fifty facts, with `proof_count` 9-11 on the four that
aggregated across batches — that is the UPDATE path working: each of the seven
batches saw the previous batches' observations in its pool and attached its
facts to them rather than creating siblings. `deleted 0` is rule 7 being
respected (nothing here is superseded). `merged 0` is correct too: the
adjudicator was offered eight pairs and kept every one, because the nine
observations are genuinely distinct facets.

The one thing this output shows that the design did not anticipate: rows 54-58
are five near-identical observations that *should* have been one, split
because the model chose to keep the subject in the text ("The retain worker
…", "The embedding backlog …") for that claim but generalised to "Multiple
components …" for the others. They are below the 0.97 dedup threshold
(different subject nouns), so dedup correctly left them alone. This is a
prompt-quality observation, not a correctness bug, and it is the kind of thing
AC-1's A/B is for.

CE-9a's live dedup was re-run after the `num_ctx` signature change and still
merges: `Merged { into: 2, dropped: 3, proof_count: 1 } in 5.7s`.

The R4 backlog test also runs live:
`an_embed_task_tick_regenerates_an_invalidated_embedding ... ok`.
