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
  `insert_observation_tx`, `observation_text_in_bank`, `fail_stale_runs`, and
  the run ledger (`start_run`, `finish_run`, `latest_run`, `watermark`).
  `merge_observation` and `ObservationVector` become uuid-keyed.
* `POST /v1/banks/{id}/consolidate` (synchronous, returns the round summary)
  and `GET /v1/banks/{id}/consolidation` (watermark, pending count, latest run).
* `[consolidation]` gains `interval_secs`, `batch_size`, `llm_batch_size`,
  `max_attempts`, `recall_budget`, `max_tokens` — all validated at startup.
* `OllamaClient::chat_json_background_bounded` gains an optional per-call
  `num_ctx`.
* CE-9a's `store_observation` splits into insert + `dedup_created`, so the
  batch round can write a whole plan in one transaction and dedup afterwards.
  `dedup_created` now also reports whether it actually called Ollama.
* `AppState.consolidating` — the per-bank single-flight set.
* The AC-2 bench gains `MEMGARDEN_BENCH_CONSOLIDATE=1`, a **rate-paced** ingest
  loop, and an assertion that the offered load was actually met.

## Pipeline

```text
tick (every interval_secs, MissedTickBehavior::Delay)
  ├─ retain queued or in flight? ──────────────────────► defer the whole tick
  ├─ embedder not loaded? ─────────────────────────────► defer the whole tick
  └─ for each bank: timeout(interval_secs x 2, run_round)
       ├─ bank already consolidating? ─────────────────► Conflict (409)
       ├─ watermark → unconsolidated(bank, > wm, batch_size)
       ├─ empty ───────────────────────────────────────► no run row, no call
       └─ start_run
            └─ queue of llm_batch_size ranges, oldest first
                 └─ per range:
                      ├─ retain arrived? ──────────────► stop, mark deferred
                      ├─ pool observations (1 recall per fact, cut to
                      │    consolidation.max_tokens; + source facts, capped
                      │    per observation and per pool)
                      ├─ assemble → over budget?
                      │    ├─ shed pooled observations, tail first
                      │    ├─ still over, >1 fact ─────► bisect, requeue
                      │    └─ still over, 1 fact ─────► drop the fact
                      ├─ LLM call x max_attempts until a valid plan
                      │    ├─ all refused, >1 fact ────► bisect, requeue
                      │    ├─ all refused, 1 fact ─────► abandon the fact
                      │    └─ transport failed ────────► skip the batch
                      ├─ embed each create (one ONNX acquire each)
                      ├─ apply_plan               ← ONE Db::write
                      ├─ re-embed each update synchronously (best effort)
                      └─ per create then per update, capped at 8 *calls*:
                         CE-9a dedup
                           └─ merged → re-embed the twin synchronously
            └─ finish_run(done, counts, watermark,
                          error = abandoned/dropped/deferred if any)
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
— which is a **rejection**, so the batch bisects and at most one fact is
abandoned (see the bisection rules below; an earlier draft of this note said
"no fact is lost", which was wrong before the bisection fix and is merely
imprecise after it).

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

**Deletes are capped at 2 per plan.** Rule 7's "be very conservative with
deletes" is prompt text, and the observation pool is chosen by recall over the
attacker's own fact text — so one landed fact can both pull a target
observation into the pool and argue for its deletion, and the watermark has
already passed that fact by the time anyone notices. `MAX_DELETES_PER_PLAN` is
the structural backstop: a plan asking to delete three observations at once has
stopped following rule 7 whatever its `reason` fields say. Counted on the raw
list, before per-entry drops, so a plan that asked for six and would have had
four dropped is still refused.

**An entry citing no source facts is dropped.** `resolve` returns `None` for an
empty list, not `Some(vec![])`. An observation with no provenance has
`proof_count = 0`, gives an operator nothing to audit, and is
indistinguishable from a hallucination — the same reason an *unknown* uuid
drops its entry.

**The adjudication budget is charged only for real LLM calls.** `dedup_created`
returns `(outcome, adjudicated)`, and `adjudicated` is false on both of its
early returns — dedup disabled, and (far more often) nothing clearing the 0.97
threshold, which is the normal case on a bank of distinct observations.
Charging for those was a review HIGH: a fresh-bank backfill would exhaust its
whole cap without once reaching the GPU, and every later write in the round
would silently skip the probe. That skip is *permanent* — dedup only ever runs
on a write, never as a sweep — so the cap has to ration the thing it claims to
ration.

**A `done` round that abandoned work says so.** `skipped_batches`,
`dropped_facts` and `deferred` are written into the existing `error` column
when non-zero. Without it, an Ollama outage across a whole round writes
`status='done', facts_seen=50, created=0, updated=0, deleted=0, watermark=50`
— byte-identical to "the model read 50 facts and correctly found nothing
durable" — with the mark past all fifty. No migration; the column and its
`GET /consolidation` field already existed.

**Updates and deletes are keyed by uuid, not rowid.** The LLM names a uuid and
the rowid it maps to was read seconds earlier. SQLite reuses the rowid of a
deleted max row, so an update aimed at an observation deleted in the meantime
can land on a brand-new unrelated one and silently rewrite its text. This is
not hypothetical: `apply_plan_skips_an_update_whose_target_vanished` hit
exactly that reuse while being written, and the test now asserts the
non-recycling explicitly. `memory_nodes.uuid` is `NOT NULL UNIQUE`.

**And so is the dedup merge**, which is the same bug on a longer fuse and was
missed in the first cut. `select_twin` picks the survivor, an adjudication runs
for seconds — up to the client's whole deadline — and only then does
`merge_observation` mutate. Its guard checked *existence*, which a recycled
rowid passes while naming a different observation: one stranger's text
rewritten, another's row deleted. CE-9a shipped that path as create-time only;
B8 calls it up to `MAX_ADJUDICATIONS_PER_ROUND` times per round from a
scheduled task, so the exposure went from incidental to continuous.
`ObservationVector` now carries the uuid (free — one more column in a SELECT
the probe already runs), `select_twin` returns both uuids, resolved before the
call, and `merge_observation` resolves them to rowids *inside* its transaction.
`reembed_merged_twin` got the same treatment from the other end: it read the
twin back with a bare `nodes::get` and then wrote a vector with an explicit
`bank_id`, so a recycled rowid would have stamped another bank's node into this
bank's `vec_nodes` partition. It now reads through a bank- and type-scoped
`observation_text_in_bank`.

**Fact uuids are the deliberate exception**, resolved to rowids before the LLM
call rather than inside the write. That is safe only because nothing in the
daemon deletes a non-observation node: `apply_plan`'s deletes are
`fact_type = 'observation'` only, and the sole other remover is a bank or
document cascade, which takes the observations with it. There is a comment on
the line saying so, because a future fact-deletion endpoint would make it
wrong and nothing downstream would catch it.

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

**One round per bank at a time.** `run_round`'s watermark read, fact selection
and `start_run` are three separate transactions. Two overlapping rounds — two
POSTs, or one POST landing on the tick — read the same watermark, select the
same facts and both apply plans, and the advanced watermark then guarantees
those duplicates are never revisited. The guard is an in-memory
`Mutex<HashSet<String>>` on `AppState`, taken inside `run_round` rather than at
the route so the background tick is covered by the same code path; the loser
gets 409. It is also what makes the uuid-keying above sufficient rather than
merely necessary. `fail_stale_runs` at startup closes `running` rows a crashed
process left behind — harmless before the guard existed, load-bearing now.

**The scheduler cannot compound.** Three things, all of which only matter when
Ollama is slow, which is when they matter most:
`MissedTickBehavior::Delay` (tokio's default is Burst, which fires every missed
tick immediately and makes rounds run back-to-back exactly when they are
overrunning); a per-round `tokio::time::timeout` of `interval_secs x 2`
(one bounded call can burn the client's 600 s total deadline, times
`max_attempts`, times the batch count — hours per round against a hung server);
and the retain-in-flight check rechecked **between batches**, not just at tick
entry, so a retain arriving mid-round waits one interval rather than a full
Ollama deadline. Abandoning a round is safe by construction: its `running`
ledger row keeps a NULL watermark and `store::watermark` is a `MAX` over rows
that recorded one, so the facts are simply re-selected next tick.

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

* **`source_memories` is sent, under two `const` caps instead of two config
  values.** The first cut of this PR omitted the field entirely and argued
  that deleting the 2026-08-02 incident's multiplier beat capping it. Review
  demolished that with this note's own numbers: legacy already shipped a
  256-token *per-observation* cap (`config.py:1171`), and six pooled
  observations at 256 is 1,536 on top of the measured 3,075 — 4,611 against a
  6,144 budget that `assemble` would shed against anyway. Capping was
  affordable; the argument was wrong.

  It also showed the cost in this PR's own live output. `obs 51`, at
  `proof_count` 10 and built by successive UPDATEs, reads *"Multiple
  components commit one chunk per BEGIN IMMEDIATE transaction to manage data
  processing"* — ten source facts naming five distinct subjects dissolved into
  "Multiple components" plus a vacuous clause. Source facts are the anchor
  that keeps a summary tied to what it summarises, and an UPDATE that cannot
  see them is rewriting blind. That is exactly the failure the field exists to
  prevent.

  What ships: `SOURCE_FACTS_MAX_TOKENS_PER_OBSERVATION = 256` (legacy's own
  number) and a whole-pool `SOURCE_FACTS_MAX_TOKENS = 1536` (legacy's 4096,
  rescaled to this module's 8192 window), both `const`, both behind the
  whole-prompt `const` that still sheds observations. Three layers where
  legacy had one, and none of them an env var — which was the real lesson of
  the incident, not "never send source facts".
* **A duplicate `observation_id` rejects the batch; legacy collapses it.**
  `_dedupe_updates` (`consolidator.py:2362-2397`) keeps the last text, unions
  the source ids and warns. A model confused enough to emit two plans for one
  observation is not a model whose merge of those plans should be trusted, and
  the round has `max_attempts` fresh tries before the batch is skipped.
* **Bisection is triggered by prompt size *and* by a rejected reply; a
  transport failure is not bisected.** Legacy bisects only on a failed call
  (`consolidator.py:1175`). Here an over-budget assembly reaches the same
  mechanism, and so does a reply that every attempt refused — that class is
  content-dependent and therefore deterministic under retry, so halving is the
  only thing that can help. A reply we never received is the opposite case: a
  down or hung Ollama fails the halves too, and 15 sub-batches x
  `max_attempts` is 45 pointless calls and 45 client deadlines. Worst case for
  the rejected class is ~15 sub-batches x 3 attempts on an 8-fact batch,
  bounded further by the per-round deadline.

  **This was a HIGH found in review, and the first draft of this heading
  asserted both triggers while implementing only one.** Before the fix, a
  batch whose reply was rejected on all `max_attempts` returned `Skipped`,
  which fell through to `watermark = facts[end - 1].id` — putting all 8 facts
  under a committed mark that `unconsolidated`'s `id > ?` would never return
  again. Legacy loses exactly one row to a `consolidation_failed_at` stamp
  because it tracks per row; a monotone rowid watermark turned every skip into
  an irreversible eight. The trigger was ordinary, not exotic: `max_attempts`
  retries the *identical* prompt at temperature 0.1, so a model that emits a
  duplicate `observation_id` once very likely emits it three times.
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
* **`exclude_id` on the UPDATE path.** Stated precisely, because an earlier
  draft of this note and the first commit message both claimed this in a way
  that was false: `exclude_id` and its only caller `select_twin` are
  **byte-identical to CE-9a** and were already used there. What this PR
  changes is *who calls them*. `dedup_created` originally ran only over
  `plan.creates`, so an observation whose text the LLM rewrote got no dedup
  probe at all — even though a rewrite is exactly when an observation becomes
  a near-duplicate of a sibling. It now runs over `plan.updates` too, which is
  the update path handoff #11 named, and the anchor exclusion is what stops
  the rewritten row matching itself at similarity 1.0.

  Two things had to land first for that to be possible. `update_text_tx` nulls
  the embedding (R4), so an updated observation has no vector to probe with
  and is invisible to `observation_vectors` until the backlog catches up —
  and rule 1 makes UPDATE the *common* path, so within a single batch an
  observation this plan updated was invisible to a create in the same plan.
  The round now re-embeds updated observations synchronously, immediately
  after the write, which both closes that window and supplies the vector.
  Best-effort, like the merged-twin re-embed: the update is already committed,
  so a failure costs exactly what CE-9a shipped (a backlog tick and a skipped
  probe) rather than abandoning the round.

  **The cap gets tighter, and that is fine.** The live round produced 9
  creates and 14 updates — 23 candidates for 8 slots. Creates are probed
  first, because a create has never been probed at all while an update was
  probed when it was created. More importantly the budget is now charged
  **only when an LLM call actually happens** (below), and most candidates
  clear no twin at 0.97 and so cost nothing, so 8 slots buy far more than 8
  candidates in practice.

## Known limits

* **A single fact can still be abandoned.** Two ways: one whose own line
  exceeds the prompt budget (`dropped_facts` — needs a ~2.5k-token fact, which
  extraction does not produce), and one that bisection narrowed down to and
  whose reply was still rejected on every attempt (`skipped_batches`). Both
  are counted, both are written to the ledger's `error` column, and both are
  bounded at one fact — which is legacy's own worst case. What is *not*
  recoverable in either case is the watermark: it advances past them, and
  nothing revisits. The repair is a manual `DELETE FROM consolidation_runs`
  for the bank, which replays from 0.
* **A transport-failed batch abandons its whole range**, deliberately — see
  the bisection divergence. If Ollama is down for a full round, all 50 facts
  pass the mark with nothing to show. This is the case the ledger `error`
  column exists for; a per-fact failure column (legacy's
  `consolidation_failed_at`) is the real fix and is a schema change, so it is
  B9's if anyone wants it.
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
  Legacy does the same per-fact recall. It also re-hydrates ids that `recall`
  already hydrated, because `RecallItem` does not carry `proof_count` or the
  uuid; both are free deletions listed under Phase F.
* **The single-flight guard is in-memory and single-process.** Two daemons on
  one database would race exactly as before. Nothing in this deployment does
  that, and a DB-level claim (a `CAS` on the ledger row) is the upgrade path.
* **`GET /consolidation` reports the latest run, not a history.** The table
  keeps every row; nothing prunes it. At one row per bank per 300 s that is
  ~105k rows a year per active bank — small, but unbounded, and a `DELETE`
  sweep is the obvious follow-up.
* **The dedup cap is a count, not a wall-clock budget.** 8 slow adjudications
  is still ~16 s of GPU.

## Verification

`cargo test --workspace`: **362 passed, 0 failed, 10 ignored** — up from 326 at CE-9a. `cargo clippy
--workspace --all-targets -- -D warnings` clean; `cargo fmt --all -- --check`
clean.

Prompt coverage: the system prompt carrying every ported section **in order**
with `.format()`'s braces unescaped and the MISSION absent from the cached
prefix; the user prompt with the mission present and absent (absent falling
back to `_DEFAULT_MISSION`); fact lines with all, some and no temporal fields;
a forged fact line and section header surviving as escaped text on one line;
observation entries omitting absent temporal fields; `source_memories`
rendered with per-source temporal fields and omitted when empty; an empty pool
rendering as `[]`.

The plan contract: a well-formed plan resolving every uuid; **two updates to
one observation rejecting the whole batch** (including a perfectly good create
in the same reply); an unknown `source_fact_ids` uuid dropping only that
entry; **an entry citing no source facts at all dropped**; **a plan asking for
three deletes refused whole, counted on the raw list**; unpooled targets,
blank reasons and blank texts each dropping their entry; a delete of an
observation the same plan updates dropped; a delete of a pooled observation
surviving; missing arrays and junk bodies (array, string, null) not panicking.

The bound: the system prompt fitting the budget, a full 8-fact batch fitting
with >1000 tokens of headroom, prompt + reply fitting `num_ctx`, an
over-budget batch refusing to render so the caller bisects, a lone over-budget
fact still refused, its short neighbour still accepted, the pool shed from the
tail with the nearest twin last to go, and pooled source facts reaching the
assembled prompt.

Storage: `unconsolidated` reading world + experience above the mark,
ascending, never observations and never another bank, honouring the limit;
`count_unconsolidated` at three marks; `apply_plan` creating, updating and
deleting in one transaction with provenance unioned and `proof_count`
recounted, the created observation embedded and the updated one re-queued;
`apply_plan` refusing to delete facts, other banks' rows or unknown uuids;
skipping an update whose target vanished **without following its recycled
rowid**; leaving an unchanged text embedded; the uuid-keyed merge unioning,
recounting and returning the survivor's rowid; a merge into a vanished twin
failing `NotFound`; the ledger recording a run, an open run contributing no
watermark, a partway failure still advancing it, and a no-progress failure not.

CE-9a's dedup, re-verified after the signature changes: the 0.97 boundary,
`>= 1.0` disabling, nine malformed replies, the token bound, the shed order,
the injection guards — and new here, **`dedup_created` reporting
`adjudicated = false` on both of its no-call paths and `true` on a real one**,
which is what makes the per-round cap ration GPU rather than candidates.

End to end: a round applying an update and a delete, moving the provenance and
the mark, and writing a `done` ledger row visible through
`GET /consolidation`; a second round being a total no-op (no row, no call) and
a third moving again after one new fact; **a duplicate `observation_id`
bisecting rather than taking its whole fact range down — 9 calls, 2 skipped
single-fact batches, nothing written**; an unknown source uuid dropping one
entry in a single call while the delete lands; an unparseable reply skipping
the batch without failing the round; 20 facts producing exactly 3 batches at
`llm_batch_size`; **a second concurrent round on one bank refused with
`Conflict` while the first is provably mid-LLM-call, exactly one ledger row
written, and the slot released afterwards**; **a bodyless POST refused 415 with
no LLM call and no round**; both endpoints 404ing on an unknown bank; status
before the first run.

Ignored (run by hand, output below): the embed-backlog regeneration, CE-9a's
live merge, and the live round.

### Measured — AC-2 baseline before writing any B8 code

CE-9a's handoff #1 asked for an n >= 5 controlled baseline first, because two
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

**Median-of-p95 = 50.37 ms, spread 49.43-52.55 ms (3.12 ms).**

`453755b` is the *exact commit* CE-9a measured at 48.85 / 49.66. Same bits,
+1.5 ms. That number is the most important thing in this section and it is
discussed under Phase F.

### Measured — the two-hop-merge lever was spent, and it is empty

Handoff #3: median >= 49 ms means spend the reserve lever first. 50.37 >= 49, so
it was spent. The change folded the graph arm's `graph::arm` + follow-up
`search::hydrate` — plus pass-1 filtering and fusion, all pure CPU — into the
main blocking hop, removing one `spawn_blocking` round trip per recall.

Five runs in a block gave 53.10 / 50.43 / 50.57 / 54.16 / 51.48, worse than the
baseline block — but by then both arms were drifting upward, so a block
comparison could not answer the question. Rebuilt as two binaries and run
**interleaved**, four paired runs:

```
pair   baseline p95   two-hop p95   difference
  1      51718us        51664us       -0.054ms
  2      52117us        51668us       -0.449ms
  3      53301us        53582us       +0.281ms
  4      52514us        52323us       -0.191ms
                            mean       -0.10ms
```

**Mean paired difference -0.10 ms against a 3 ms run-to-run spread: zero.**

The lever was **reverted**. Its premise — that a second `spawn_blocking` costs
real time on the loaded path — does not hold: tokio's blocking pool is 512
threads, so the second hop is a scheduler round trip of tens of microseconds,
not a queue wait.

### Measured — controlled loaded p95 with consolidation ENABLED

Handoff #2's budget. `MEMGARDEN_BENCH_CONSOLIDATE=1` seeds a second bank with
3000 facts and drives a real `run_round` against it **once a second** for the
whole loop — 300x the production tick rate — against a stub Ollama returning
one CREATE per batch, so every round genuinely embeds (ONNX mutex), writes
(`apply_plan`), and adjudicates (CE-9a dedup). A second bank rather than `b1`
because consolidation contends on the process-wide mutex and the write lock
whichever bank it runs on, but pointing it at `b1` would also hand its dedup
probe a full scan of the 36k *observations* this harness's background ingest
writes, which no real bank has.

**The first attempt at this measurement was invalid and is worth recording as
a method failure.** The harness's ingest loop was an unpaced busy loop, so
consolidation *displaced* ingest rather than adding to it: the consolidating
arm wrote ~33k background nodes against the baseline's ~36k. Background node
count and p95 correlate at r ~= 0.86 across the baseline block with a slope of
~2 us/node, so a size-matched non-consolidating run predicts ~44 ms — meaning
the observed 49.64 implied consolidation *cost* ~+5 ms rather than saving
0.7 ms. The original claim ("budget met: 49.64 <= 50") had the sign of the
effect probably inverted. It is **withdrawn**.

The fix is in the harness, not the prose: the ingest loop is now **rate-paced**
at one 8-fact batch every 12 ms, and the bench **asserts** it achieved >= 90% of
that offered load, so a run whose load generator fell behind fails instead of
publishing an incomparable number. Both arms then offer identical load by
construction. Re-measured as interleaved pairs on the paced harness:

```
pair   consolidation OFF        consolidation ON         delta
        p95      bg nodes        p95      bg nodes
  1   46118us     28,808      46764us     30,352       +0.646ms
  2   47269us     29,064      48090us     30,504       +0.821ms
  3   46673us     28,952      46565us     30,264       -0.108ms
  4   46690us     29,456      48289us     30,632       +1.599ms
                                            mean       +0.740ms
```

**Consolidation costs a mean paired +0.74 ms of recall p95**, on a bench
running rounds 300x more often than production ever will. Median p95 is
46.68 ms off and 47.43 ms on; every one of the eight runs is far inside AC-2's
60 ms line and p50 is ~22 ms against 35 ms.

The claim withdrawn above is replaced by this one, which the data supports:
consolidation has a **small, real, positive cost** in the expected direction,
an order of magnitude below the +5 ms the biased comparison implied and well
inside the +1.5 ms paired-delta gate proposed for Phase F.

One residual asymmetry, disclosed rather than corrected: the ON arm
accumulates ~5% more background nodes (30.4k vs 29.1k) because the pacer fixes
the offered *rate*, and the ON arm's recall loop takes slightly longer wall
clock, so it receives more batches at that rate. Matching the rate is what
makes the arms comparable; matching the totals would require ending the ingest
at a fixed node count instead, which is the obvious next refinement.

**What this measurement does and does not say.** The recall side is a
sequential closed loop with no think time, so these are *service-time* p95s at
concurrency 1, not response times under offered load. That is the forgiving
direction and it is acceptable for a gate, but it must be written down,
because it means AC-2 as measured says nothing about concurrent recall.

### Measured — per-round wall time on a ~50-fact bank

`cargo test --release -p memgardend --test consolidate_api -- --ignored
--nocapture live_consolidation_round`, real Ollama (`qwen3-14b-nothink`,
v0.21.2) and the real `bge-small-en-v1.5`:

```
RoundSummary { run_id: Some(1), facts_seen: 50, created: 20, updated: 17,
               deleted: 0, merged: 0, batches: 8, skipped_batches: 0,
               dropped_facts: 0, adjudications: 0, deferred: false,
               watermark: 50 }
  wall: 151.1s
```

**151 s for a 50-fact round** — up from 68.5 s before source facts were added
to the pooled observations, which roughly doubled prompt size and, with it,
prefill time. Against the 300 s interval that is a **50% duty cycle at full
`batch_size`**, and it should be read as the reason the retain-in-flight skip
is the guard that matters rather than as a comfortable number. A deployment
that consolidates continuously at this rate should raise `interval_secs`.

`batches: 8` over 50 facts at `llm_batch_size = 8` means seven groups plus one
bisection — the larger prompts pushed one group over the budget, and the
bisection did exactly what it exists for.

**`adjudications: 0` is the HIGH-2 fix demonstrated live.** Twenty
observations were created and not one of them cleared 0.97 against an existing
twin, because they are genuinely distinct facets — so no LLM call was made and
nothing was charged. Under the original accounting all eight slots would have
been consumed by the first eight creates *without a single call to Ollama*,
and creates 9 through 20 would have silently skipped the dedup probe
permanently. The cap now rations GPU, which is what it claims to ration.

## Manual verification

The plan asks for "consolidate a bank with ~50 facts; quote the created
observations and the run row". The live round above is exactly that. Its 50
input facts are five subjects x five claims x two repetitions, so the correct
answer involves both CREATE and UPDATE — and the model produced both.

Created observations (`id`, `proof_count`, text):

```
obs 51 (proof_count 1): The retain worker commits one chunk per BEGIN IMMEDIATE transaction.
obs 52 (proof_count 1): The embedding backlog commits one chunk per BEGIN IMMEDIATE transaction.
obs 53 (proof_count 1): The recall pipeline commits one chunk per BEGIN IMMEDIATE transaction.
obs 54 (proof_count 1): The sqlite-vec index commits one chunk per BEGIN IMMEDIATE transaction.
obs 55 (proof_count 3): The embedding backlog drains in batches of eight to cap the ONNX mutex hold.
obs 56 (proof_count 3): The retain worker drains in batches of eight to cap the ONNX mutex hold.
obs 57 (proof_count 4): The embedding backlog partitions only on bank_id, so fact_type is filtered in Rust.
obs 58 (proof_count 5): The recall pipeline partitions only on bank_id, so fact_type is filtered in Rust.
obs 59 (proof_count 3): The Ollama client partitions only on bank_id, so fact_type is filtered in Rust.
obs 60 (proof_count 3): The sqlite-vec index partitions only on bank_id, so fact_type is filtered in Rust.
obs 61 (proof_count 4): The retain worker partitions only on bank_id, so fact_type is filtered in Rust.
obs 62 (proof_count 1): The sqlite-vec index drains in batches of eight to cap the ONNX mutex hold.
obs 63 (proof_count 1): The Ollama client drains in batches of eight to cap the ONNX mutex hold.
obs 64 (proof_count 1): The retain worker fuses four retrieval arms with reciprocal rank fusion.
obs 65 (proof_count 1): The embedding backlog fuses four retrieval arms with reciprocal rank fusion.
obs 66 (proof_count 1): The recall pipeline fuses four retrieval arms with reciprocal rank fusion.
obs 67 (proof_count 1): The sqlite-vec index fuses four retrieval arms with reciprocal rank fusion.
obs 68 (proof_count 1): The Ollama client fuses four retrieval arms with reciprocal rank fusion.
obs 69 (proof_count 1): The sqlite-vec index holds exactly one concurrency permit for the local 14B model.
obs 70 (proof_count 1): The Ollama client holds exactly one concurrency permit for the local 14B model.
```

The run row:

```
("done", facts_seen 50, created 20, updated 17, deleted 0, merged 0, watermark 50)
```

`deleted 0` is rule 7 respected — nothing here is superseded. `merged 0` is
correct: nothing cleared 0.97, because every observation names a different
subject.

### The same bank, before source facts were sent — the quality regression, quantified

This is worth keeping side by side, because it is the clearest evidence in the
PR and it was found by review rather than by the implementation. The identical
50 facts, run before `source_memories` was populated, produced **nine**
observations:

```
obs 51 (proof_count 10): Multiple components commit one chunk per BEGIN IMMEDIATE
                         transaction to manage data processing.
obs 52 (proof_count 10): Multiple components drain in batches of eight to cap the
                         ONNX mutex hold.
obs 53 (proof_count  9): Multiple components fuse four retrieval arms with
                         reciprocal rank fusion.
obs 54 (proof_count  3): The retain worker partitions only on bank_id, so
                         fact_type is filtered in Rust.
obs 55-58 (proof_count 2 each): the same claim, once per remaining subject
obs 59 (proof_count 11): Multiple components hold exactly one concurrency permit
                         for the local 14B model.
```

Every high-`proof_count` row is a successive-UPDATE pile-up that lost its
subject: ten facts naming five distinct components collapsed into "Multiple
components", and one of them acquired the invented, contentless clause "to
manage data processing". That is summary drift with no anchor — an UPDATE
rewriting text it can no longer see the evidence for. With source facts in the
prompt the same input yields twenty subject-specific observations and not one
"Multiple components".

The first draft of this PR argued that omitting source facts was an incident
guard. It was a quality regression wearing an incident guard's clothes; the
actual guard is the pair of `const` caps, which is what legacy had all along.

CE-9a's live dedup was re-run after the uuid and `num_ctx` changes and still
merges: `Merged { into: 2, dropped: 3, proof_count: 1 } in 5.7s`. The R4
backlog test also runs live:
`an_embed_task_tick_regenerates_an_invalidated_embedding ... ok`.

## Carry into Phase F — measurement method and the remaining levers

Most of this came out of the CE-9b review round rather than the
implementation, and it is the most valuable thing the PR produced.

### The cross-PR latency series is dead. Retire it.

This PR's baseline was taken on `453755b` — the **exact commit** CE-9a
measured at 48.85 / 49.66 ms — and came back **50.37 ms**. Same bits, +1.5 ms
of pure environment.

Put the three magnitudes side by side:

| | |
|---|---|
| Between-PR deltas the series was reading | <= 1 ms |
| Same-commit, between-day delta | 1.5 ms |
| Within-session drift over ~25 minutes of benching | 2 ms+ |

**The series was measuring the machine, not the code.** CE-8's absolute 50 ms
trigger duly fired on unchanged code, which is exactly what a threshold with
no information content does. It should be **retired**, not re-armed, and no
Phase F PR should be gated on an absolute p95 taken on a different day.

### Promote the interleaved-paired design to the standard gate

It was written as an ad-hoc rescue when the block comparison failed; it should
be the default, because it costs nothing and it is the only thing here that
actually resolved a sub-millisecond question:

1. Build two binaries (with and without the change).
2. Alternate them, n >= 4 pairs.
3. Report the **mean paired delta** and the per-pair spread.
4. Gate on the delta — suggested: no PR ships a mean paired p95 delta above
   **+1.5 ms** unjustified.
5. Re-anchor the absolute level per session; never compare it across days.

And pace the load generator. An unpaced busy loop means anything that competes
with it silently reduces offered load, which is how this PR's first
consolidation measurement came out with the sign inverted. The bench now
asserts it met its offered load; keep that assertion.

### Levers, sorted

* **Hop count is closed forever.** The two-hop merge measured -0.10 ms. By the
  same 512-thread argument, so is *any* lever whose thesis is "remove a
  `spawn_blocking` round trip". Write the whole class off.
* **Write-lock hold time is not supported by the code.** An earlier draft of
  this note named it as a cause of the loaded p95. Recall's read path takes no
  write lock — the only `db.write` in `search.rs` is the admin-only
  `rebuild_vec_index` — and WAL does not block readers on the writer. The
  claim is withdrawn; it smelled exactly like the two-hop premise.
* **The ONNX mutex is the one remaining lever with double-digit-ms
  potential.** `Embedder` holds a `Mutex<TextEmbedding>` (`embed.rs:17`) and
  `embed_batch` takes it for the **whole batch** (`:44`); recall's semantic arm
  needs one query embedding through that same mutex on every single request,
  while the ingest loop holds it eight texts at a time. Idle p95 is 7.83 ms
  against a loaded ~50 ms, and 42 ms is the right order of magnitude for
  head-of-line blocking. Note that consolidation is already the polite user
  here — `embed_one` takes one text per acquire, per CE-9a handoff #6 — so the
  contender is the ingest batch, not this PR.

  **Instrument the mutex wait time under load before spending a PR on it.**
  Spending one on a plausible-but-unmeasured premise is precisely what the
  two-hop lever cost.
* **Consolidation-side, if a round ever needs to be cheaper:**
  `pool_observations` runs one full `recall` **per fact** (50 a round) on the
  same blocking pool the latency path uses — batching that is the largest
  consolidation cost after the LLM itself. And it then calls `search::hydrate`
  on ids `recall` already hydrated, purely because `RecallItem` carries
  neither `proof_count` nor the uuid; adding them is a free deletion.

### Testability seam for B9

`creates` cannot be exercised hermetically because `Embedder` is a concrete
struct with no stub point, so every create-path test either loads the 133 MB
model or goes through the `#[ignore]`d live round. A minimal `trait Embed`
with one production implementation is the one abstraction-over-one-implementation
that earns its keep; not taken this round, flagged for B9.
