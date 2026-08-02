# AX-1 — `embedding_model` tagging (vector-space versioning)

Branch `feat/ax-1-embedding-model-tag`. Migration `0005_embedding_model.sql`,
`LATEST_VERSION` → 5.

Nothing on disk recorded *how* a vector was made: `memory_nodes.embedding` is a
bare 1536-byte BLOB, `vec_nodes` a bare `FLOAT[384]`. MG-1 (Phase D) plans to
import the legacy Python bank **without re-embedding** on the premise that both
sides are `BAAI/bge-small-en-v1.5`. If that premise is wrong, AC-3 (count
parity) still passes and only AC-1 (quality) drops, with no way to attribute
it. The column is cheap now and needs a full re-embed later.

Origin: jcode analysis §2-1 (`jcode-memory-findings.md`), plan appendix AX-1.

## What this adds

* `memgarden_core::EMBEDDING_MODEL_ID` — the active producer id, a `const`.
* `0005`: `ALTER TABLE memory_nodes ADD COLUMN embedding_model TEXT`, plus a
  backfill.
* Every vector-producing write stamps it: `nodes::set_embedding`,
  `nodes::set_embeddings_batch` (the backlog worker and CE-9b's re-embed both
  route through these), and `consolidate::insert_observation_tx`, which embeds
  inline and so stamps it itself.
* Every cosine/KNN read filters on it: `search::knn` and
  `consolidate::observation_vectors` (CE-9a's 0.97 dedup probe).

## The id format: `<runtime>:<model>` → `fastembed:BAAI/bge-small-en-v1.5`

Weights alone do not determine a vector, and weights alone are the one thing
we already know both sides share. `sentence-transformers` and `fastembed` each
serve `BAAI/bge-small-en-v1.5`; whether they agree on pooling and
normalization is exactly the open question. Tagging only the model name would
make a legacy import and a fastembed vector indistinguishable and the check
impossible after the fact — which is the failure this whole item exists to
prevent. So the runtime is in the id, and MG-1's legacy rows will read
`sentence-transformers:BAAI/bge-small-en-v1.5`.

The colon separates the two halves because the model half already contains a
slash (`org/name`, HF's own form, kept verbatim so the id is greppable against
either project's config).

**The runtime *version* is deliberately absent.** `fastembed = "=5.17.4"` is
pinned in the workspace; folding that in would invalidate every stored vector
on a routine patch bump — a full re-embed as the price of a dependency update.
Bump the const only when the *output* changes: different weights, a different
runtime, or a fastembed release whose notes touch pooling or normalization.

**A const, not config, not env.** It has to describe what the code actually
ran. A value an operator can set is a value that can lie about the bytes on
disk, and a wrong tag is worse than no tag — it silently readmits an
incomparable vector to a cosine comparison.

It lives in `memgarden-core` rather than `memgardend::embed` for one
mechanical reason: `memgarden-store` writes the column and cannot depend on the
daemon crate. Same reason `EMBEDDING_DIM` is there. `embed.rs` remains what the
string describes.

## Backfill: yes, tagged as ours

`UPDATE memory_nodes SET embedding_model = '<id>' WHERE embedding IS NOT NULL`.

Justification: there is no import path yet. MG-1 is Phase D and unwritten, so
"has a vector" and "was embedded by this codebase's fastembed path" are the
same set in every database that exists today. Leaving them NULL would drop
every existing row out of the dense arm the moment the daemon upgrades — a
recall regression dressed as caution. Rows still on the backlog
(`embedding IS NULL`) stay NULL: no vector, no producer.

SQL cannot read a Rust const, so the literal is duplicated in the migration.
`backfill_literal_matches_the_active_model_id` pins the two together.

## Mismatch is a dense-arm exclusion, not a deletion

A foreign or NULL tag removes a row from `knn` and from the dedup probe. It
removes it from **nothing else** — FTS/BM25, the graph arm, `hydrate` and the
temporal arm are all untouched, by design and by test. This is jcode's core
insight and we get it free: hybrid search *is* the migration strategy. A mixed
bank degrades to keyword recall for its foreign rows instead of ranking them by
a cosine distance that means nothing.

`NULL` reads as "producer unknown" and is excluded too — the same convention as
jcode's `LEGACY_EMBEDDING_MODEL`. SQL's `= ?` is already false for NULL, so
this needs no extra predicate.

## Diverged from legacy

* **`update_text` does not null `embedding_model` alongside `embedding`.** The
  obvious symmetry is wrong here. That function deliberately leaves the stale
  `vec_nodes` row in place (its documented R4 deviation: stale-but-present
  beats invisible for the whole backlog window). The tag describes the producer
  of the vector *in the dense index*, and that vector is still there and still
  ours — nulling the tag would filter the node out of `knn` and re-create
  exactly the invisibility the deviation exists to avoid. Two existing tests
  caught this; the tag is overwritten on re-embed.
* **`rebuild_vec_index` does not filter by model.** It mirrors
  `memory_nodes.embedding` into `vec_nodes` faithfully, foreign rows included;
  the read-side filter is what enforces the space, so the index stays a plain
  mirror and a changed const takes effect without a rebuild.
* **No re-embed-on-mismatch.** `pending_embeddings` still keys on
  `embedding IS NULL` only. Deciding *whether* to re-embed a foreign bank is
  MG-1's job, and it needs the cosine number below to decide.

## Cost

`knn` gains a join from `vec_nodes.rowid` to `memory_nodes.id` — that table's
`INTEGER PRIMARY KEY`, so one B-tree lookup and one string compare per
candidate, on the k rows the vec0 scan already selected.

The filter runs **after** vec0 picks its top `k`, so a bank holding foreign
vectors returns fewer than `k` dense candidates rather than reaching deeper.
Acceptable while every row is ours (0005 backfills them all). Ceiling and
upgrade path are in the `ponytail:` comment on `knn`: sqlite-vec's rowid-IN
prefilter, or `embedding_model` as a second vec0 partition key.

Measured, `MEMGARDEN_BENCH_CONTROL=1 MEMGARDEN_BENCH_LOAD=1`, 3000 nodes, 2000
requests, two binaries interleaved, 4 pairs — see *Measured* below.

## MG-1's reference vector

`embed::mg1_reference_vector` (`#[ignore]`d, needs the 133MB model) prints the
active embedder's output for one fixed ASCII sentence: model id, dim, L2 norm,
first 8 dimensions. It asserts nothing about the values on purpose — there is
no committed expectation to compare against yet, and inventing one would only
assert that fastembed equals itself.

Run on this branch:

```
mg1: model_id  = fastembed:BAAI/bge-small-en-v1.5
mg1: text      = "the database migration completed successfully last night"
mg1: dim       = 384
mg1: L2 norm   = 0.9999998
mg1: dims[0..8]= [-0.0379496, -0.0279658, 0.0030362, -0.0563243,
                  -0.0080432, -0.0053930, -0.0526074,  0.0286812]
```

The sentence is passed **raw**: no `augment_for_embedding`, no BGE
query/passage prefix. Neither side applies one — verified 2026-08-03, legacy's
live provider is `LocalSTEmbeddings` whose `encode()` is bare, and the
`query_prefix`/`passage_prefix` pair exists only in the unused
`OnnxEmbeddings` class — so there is no query/document asymmetry in our recall
today, and feeding byte-identical input leaves pooling and normalization as the
only variables. MG-1 embeds the same sentence on the legacy stack, takes the
cosine (both vectors are unit, so a dot product), and records the number in
its PR. ~1.0 confirms import-without-re-embedding; anything else means
re-embed, and the tag is what makes the mixed intermediate state safe either
way.

## Tests

5 new, 367 total.

* `fresh_database_has_the_0005_embedding_model_column` — v5, and all three
  write paths stamp the id.
* `migrate_upgrades_a_v4_database_in_place` — a populated v4 DB with one
  embedded and one pending row: rows survive, the embedded one is backfilled,
  the pending one stays NULL, and the backfilled row is still a KNN hit.
* `backfill_literal_matches_the_active_model_id` — the const/SQL drift pin.
* `a_foreign_model_vector_is_absent_from_knn_but_present_in_fts` — three
  identical vectors, tags `ours` / another producer / NULL: KNN returns only
  ours, BM25 and `hydrate` return all three.
* `the_dedup_probe_skips_foreign_model_observations` — the other cosine
  consumer, which reads the BLOB column rather than `vec_nodes`.

## Measured

Interleaved paired runs, `MEMGARDEN_BENCH_CONTROL=1 MEMGARDEN_BENCH_LOAD=1`,
3000 nodes, 2000 requests, release, two prebuilt binaries alternated:

```
pair   base p95   AX-1 p95   difference   base bg    AX-1 bg
  1     42278us    41639us     -0.639ms    27,272     27,512
  2     44380us    41174us     -3.206ms    28,480     27,136
  3     43293us    42331us     -0.962ms    27,736     27,376
  4     44319us    41318us     -3.001ms    28,096     27,008
                       mean     -1.952ms
```

**Mean paired p95 difference -1.95 ms. It does not exceed +1.5 ms; it does not
exceed zero.** Per-pair spread of the differences: -0.64 to -3.21 ms (range
2.57 ms), against a baseline-arm p95 spread of 2.10 ms — so the raw number is
inside the noise, even though AX-1 won all four pairs.

The sign consistency is not a speedup, and the last two columns say why. The
load generator wrote ~640 more background nodes into `b1` on the baseline arm
per pair (mean 27,896 vs 27,258), and CE-9b measured p95 correlating with node
count at ~2us/node. Charging each pair its own node delta at that rate:

```
pair   raw diff   node delta (AX-1 - base)   charge     corrected
  1     -0.639ms          +240                +0.48ms    -1.12ms
  2     -3.206ms         -1344                -2.69ms    -0.52ms
  3     -0.962ms          -360                -0.72ms    -0.24ms
  4     -3.001ms         -1088                -2.18ms    -0.82ms
                                                 mean     -0.67ms
```

The correction collapses the spread from 2.57 ms to 0.88 ms, which is the
result you expect if offered-load drift really was most of the raw signal.
**Corrected mean -0.67 ms: no measurable change in either direction.** Both
arms passed the harness's own 90%-of-offered-load gate, so neither run is
disqualified — the residual drift is the pacer's, not a fallen-behind loop.

Consistent with the mechanism: the added predicate is one primary-key lookup
and one string compare on the k rows vec0 already selected, next to a
brute-force cosine scan of a 3000-vector partition.
