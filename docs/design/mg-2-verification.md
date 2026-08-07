# MG-2 — the instrument AC-3 is read from

`crates/memgardend/src/migrate/verify.rs`. Third and last of Phase D's PRs.
D1 froze legacy, D2 imported it; this reads both back and says whether
anything was lost — in a way that distinguishes *equal*, *recomputed* and *not
applicable*, because a single boolean would either lie or fail forever.

---

## Why this is a separate program from `import`

Counts printed by the program that wrote the rows are not evidence that the
rows are right. `import` reports what it believes it did. `verify` reads three
oracles, and the importer's opinion is not among them:

| oracle | answers | provenance |
|---|---|---|
| `snapshot/stats.json` | *"how many did legacy have?"* | `GET /stats`, frozen at snapshot time |
| `snapshot/<bank>/…json` | *"what did legacy say?"* | `document-transfer`, frozen at snapshot time |
| the SQLite file | *"what do we have?"* | read through `memgarden_store` |

A disagreement between the first two is a **snapshot integrity** failure and is
reported on its own line: the first means the legacy bank moved under the read,
the second means we lost something. Conflating them would send the operator
looking for a migration bug that is not there.

---

## The exit-code contract

```
0  PASS    every Tier-1 equality holds, no content difference, Tier 2 in band
1  FAIL    a Tier-1 mismatch, a content difference, or a snapshot integrity failure
2  REVIEW  Tier 1 clean, a Tier-2 metric outside its band — a human should look
3  usage
```

**3 for usage, not 2.** `snapshot` and `import` use 2 for "you called it
wrong"; `verify` needs 2 for the review stop, and a script that reads "bad
arguments" as "go look at the adjacency numbers" is worse than one that reads
it as neither. The three subcommands now differ on this, which is stated here
because it is exactly the kind of thing a reader assumes is uniform.

`Verdict::exit_code` lives next to the enum so the contract and the code cannot
drift.

---

## Tier 1 — equality, and every one of them fails the run

Sixteen checks. Each carries **where its expected value came from**, because
three different things produce one and a column headed "legacy" for a check
legacy has no opinion about would be the dishonest version of this table.

| check | expected from | note |
|---|---|---|
| banks, documents, nodes, `nodes.{world,experience,observation}`, `caused_by` | legacy's frozen `/stats` | the counts that *can* be equal |
| `node_sources` | the archive, **distinct** `(document_id, fact_index)` pairs | 2,200 raw collapse to **2,114**; `link_sources_tx` is `INSERT OR IGNORE` against the `(observation_id, source_id)` PK (`consolidate.rs:638-650`) |
| `node_tags` | the archive | distinct per node, matching `INSERT OR IGNORE` |
| `entities`, `node_entities` | the archive, normalized | **exact, and the plan did not expect it** — see below |
| import marker | our own rule | `state = done` **and** `snapshot` = this snapshot's hash, so a bank imported from a different snapshot is caught rather than counted |
| consolidation watermark | our own rule | one `done` run per bank at `watermark = MAX(memory_nodes.id)`; without it the daemon re-consolidates the whole migrated corpus within one poll of restart (`consolidate.rs:314-330`) |
| embedding coverage | our own rule | 0 rows with a NULL or foreign `embedding_model`; non-zero after `--defer-embeddings` until the daemon has drained |
| orphan facts | our own rule | 0 non-observations without a document |
| semantic edges from observations | our own rule | 0 — **parity, not a divergence**; see below |
| temporal self-consistency | our own rule, re-run | the check that catches a broken import; see below |

### Entities became a Tier-1 equality, which the plan did not expect

The plan has no entity row in Tier 1, because when it was written the importer
ran `entities::resolve_fact` and the counts could only be approximately right.
MG-1b's correction — normalize and stop — made them exact: **3,917 distinct
normalized archive names to 3,917 rows, 10,379 mentions to 10,379
`node_entities` edges**. An exact count is a gate; an approximate one is a
paragraph. `docs/design/mg-1-migration.md` §1 has the measurement.

### "Observations have no semantic edges" is parity, not a divergence

§Binding decisions #5e presents this as a MemGarden gap to file in
`parity-gaps.md`. It is not one. Decomposing `GET /graph` for `bank-a`
by each endpoint's `fact_type`, after removing the 1,199 duplicate edge ids the
projection carries:

| link type | no observation endpoint | `/stats` | touching an observation |
|---|---|---|---|
| `temporal` | **3,269** | **3,269** | 6,732 |
| `semantic` | **4,603** | **4,603** | 9,080 |
| `caused_by` | **4** | **4** | 8 |

Exact, all three. `/stats` counts stored rows, and every observation-touching
edge in `/graph` is a visualization copy — `memory_engine.py:7723-7724` says so
outright. **Legacy stores none either.** So this is a post-condition worth
asserting and not a gap worth filing.

### Temporal self-consistency, and the two wrong scopes it had first

Tier 1 cannot assert our temporal edge set against legacy's — the rules differ
(§Tier 2). What it *can* assert is that the stored set is exactly what our own
rule produces over the migrated nodes' own `(fact_type, event_date)`. That
catches a broken import, a lost `event_date` and a mis-ordered batch, which is
everything a Tier-1 temporal gate was wanted for.

It reads the dates from the **database**, not the archive, and that is
load-bearing: MG-1b stamps observation dates that
`consolidate::insert_observation` does not write, and if it stopped, the
import's own temporal pass would still emit the edges (it reads the archive)
while this check would collapse on the observation side.

**Scope took three attempts, and the manual verification found both wrong
ones.**

1. *Unscoped.* Failed on the live database, 2,281 stored against 2,460
   expected. That failure is **the daemon working correctly**: retain builds
   the graph incrementally, `links::temporal_links(&chunk, &window)` per chunk
   against a rolling window (`retain/mod.rs:626`), so a bank retained into
   since the import is not a fixed point of the whole-corpus rule and never
   will be.
2. *`id <= MAX(consolidation_runs.watermark)`.* Looks like the import's
   boundary and is not — **the daemon writes `consolidation_runs` rows too**.
   On the live database the scope drifted onto a bank that was never imported
   and the check failed on 1 of 4.
3. *`metadata.$.legacy`* — the key MG-1b stamps on every node it writes and
   nothing else ever writes. Exact rather than approximate: every edge a later
   retain adds has a **new** node as its `from`, so edges with both endpoints
   inside the migrated set are precisely the import's.

The check reports how many banks it covered, so a database with nothing
migrated prints `over 0 of 4 banks` instead of a green row over an empty set.

---

## Tier 2 — recomputed, and banded only where a band means something

| metric | legacy | ours (first run) | band |
|---|---|---|---|
| `temporal`, fact to fact | 43,657 | **70,212** | **[1.45, 1.75]** — observed **1.608** |
| `temporal`, observation to observation | — | **34,804** | **none, and no ratio** |
| `semantic` | 65,127 | **6,890** | **none** — observed 0.106 |
| `proof_count` disagreements | 1,747 observations | **93** | reported |

Every metric prints its **per-node out-degree** as well as its count, because
that is what actually shows a rule change: a cap that stopped firing reads as a
count drift and is unmistakable as a histogram. In the first run temporal sits
at p50/p90/max = 20/20/20 — hard against `MAX_TEMPORAL_LINKS_PER_NODE` — and
semantic at mean 2.54, max 7, which is the shape of the defect below.

### The band is fact-to-fact, and folding the observation class in would break it

Legacy's `/stats` temporal count is fact-to-fact (the table above). Our
observation-to-observation edges are therefore a **new class with no legacy
counterpart**, and they get no ratio. A band on the *total* — which is what an
earlier draft of `mg-1-migration.md` proposed at ~2.4 — would pass a run in
which the fact-edge rule silently broke, as long as observation edges made up
the difference.

The rules differ, and by rule rather than by ordering:
`fetch_temporal_neighbors` (`ops_postgresql.py:562-593`) takes the 20 nearest
by `event_date` in each direction and applies no 24-hour predicate, where
`links.rs:69` does. Measured three ways over the archive — whole-corpus 70,192,
by `chunk_index` 68,781, by `created_at` batch 69,771 — ordering moves it by
2 %. Legacy's own side carries the proof: **72 stored fact-to-fact temporal
edges in `bank-a` sit at weight exactly 0.3**, the `max(0.3, 1 − h/24)`
floor, reachable only at `h ≥ 24`.

### `semantic` gets no band, and not for the reason the plan gives

The plan says semantic has "no prior at all". It has one — legacy's 65,127 —
and we are at 6,890 for a reason that is **not** a property of the migration:

* all 6,890 edges connect nodes whose rowids differ by at most **7**, and
  `embedding.batch_size` is **8**. Not one crosses a batch boundary;
* `embed_task.rs:178-179` builds `node_types` from the just-embedded batch's
  ids only, and `links::semantic_links` drops every neighbour missing from that
  map (`links.rs:143`). The KNN correctly returns the best 100 in the bank; all
  but the handful in the same batch of 8 are discarded;
* over the same migrated vectors a whole-corpus pass would emit **68,537** —
  97 % of every node's top-20 neighbours clear the 0.7 threshold — which is
  within 5 % of legacy's number.

So the threshold is right and the embedding space is fine. **This is a CE-7
defect affecting every MemGarden database**, not just migrated ones, and a
one-line fix moves the number by 10×. Banding it now would be banding a bug.
`book/src/roadmap.md` carries it; D3 sets no band until CE-7 decides.

### `--accept-tier2`, and why its hash is not a hash of the report

A phase that always exits 2 trains the reader to ignore exit 1 within two runs,
so the review stop needs a re-entry criterion of its own:
`verify --accept-tier2 <hash>` records an explicit acknowledgement of one
specific Tier-2 result and downgrades **that** exit 2 to 0.

The hash covers **the snapshot hash and the Tier-2 counts, and nothing else**.
The first version hashed the whole report minus the verdict, and it did not
survive a save-and-reload: one out-degree `mean` came back as
`19.55813953488372` where it went out as `19.558139534883722`, so the hash an
operator read from a saved report did not match the next run's. A hash nobody
can paste is not a re-entry criterion. Every field in the material is an
integer or a string; the ratios are `ours / legacy` and add nothing.

Two properties the tests pin: acknowledging a Tier-2 result **cannot** launder
a Tier-1 failure (the downgrade only applies to `REVIEW`, and the hash is
computed with the verdict excluded), and one result's hash does not accept a
different one.

---

## Tier 3 — not applicable

`entity` links, printed with `counts.py:47-49` next to the number so nobody
re-litigates it from `/stats` output. Legacy reports 4,124 and stores zero —
the figure is derived at read time from `unit_entities`. We store zero on
purpose (`links.rs:6-8`). Exact parity, and the only tier where "not
applicable" is the honest answer.

---

## The 50-sample content diff

Deterministic from `(seed, snapshot)`: records are sorted into a stable order
first — facts by `(document uuid, fact_index)`, observations by
`(text, mentioned_at)` — and then drawn with a five-line `splitmix64`. No new
dependency; `ponytail:` if a future check needs a real distribution, that is
when a crate earns its keep.

**Stratified by bank in proportion to node count**, so `bank-b`
contributes ~30 of 50 and `bank-a` ~5. A uniform draw over the pooled
corpus could miss a bank entirely.

| field | comparison |
|---|---|
| `text` | byte-equal |
| `fact_type` | equal |
| `context` | equal, with `""` ≡ `NULL` — legacy emits empty strings and `NewNode.context` filters them |
| `occurred_start`, `occurred_end`, `mentioned_at` | equal as epoch ms |
| `event_date` | equals `occurred_start.or(mentioned_at)` — `writes.py:80` parity, asserted rather than assumed |
| `tags` | equal as sets |
| `entities` | the archive's names, normalized, equal as a set to the node's canonical names |

`proof_count` is **not** compared: it cannot be equal by construction and is
Tier 2.

### The join that matched nothing

Facts join on `(document_id, fact_index)` out of `memory_nodes.metadata`;
observations join on `(text, mentioned_at)`, measured 0 duplicates over all
1,747 where `text` alone collides 3 times.

The first version bound `mentioned_at` as **text** and compared it to
`coalesce(mentioned_at, '')`. `coalesce` strips the column's INTEGER affinity,
so SQLite never converted the operand, and **every observation in the sample
came back "no matching node"** — 18 false content differences and an exit 1 on
a database that was correct. A join key that silently matches nothing is worse
than no join key, because it reads as a migration failure.
`every_observation_in_the_sample_finds_its_node` is the test.

---

## On "read-only"

There is **no read-only path in the store**: `Db::open` runs `migrate::migrate`
(`lib.rs:52-58`), eight `BEGIN IMMEDIATE`s. The plan's first draft claimed
`verify` opens the database read-only and then specified a test asserting the
opposite.

The honest guarantee is narrower and is what the test asserts: **`verify`
issues no `INSERT`, `UPDATE` or `DELETE`** — every query goes through
`Db::read` — and it is safe against a live database because migrations are
already applied and each re-checks `user_version` inside its own transaction
before doing anything (`migrate.rs:44-48`).
`verify_changes_nothing_in_the_database` takes a census of every table in
`sqlite_master` before and after and compares it, rather than asserting the
absence of a statement nobody can grep for at runtime.

---

## `--dump-only`

The runbook's step 3a, and the only thing that preserves the shadow run's
`sessions` before `import --replace` deletes them. No comparison, no snapshot
requirement beyond its checksums: it emits what the database holds. It exists
because §Binding decisions #5c makes `--replace` delete AC-2/AC-6 measurement
data and nothing in the binary can enforce a runbook step.

---

## Manual verification — 2026-08-07

```
$ mg_migrate verify --snapshot <scratch>/snapshot-d2 --db <scratch>/rehearsal.db \
                    --out ac-3.json --sample 50 --seed 1
```

Every Tier-1 check green, including **temporal self-consistency 105,016 ==
105,016 with 0 stored edges the rule would not emit and 0 it emits that are not
stored, over 4 of 4 banks**. Tier 2: fact-to-fact **1.608×**, in band;
observation-to-observation 34,804 with no ratio; semantic 0.106× unbanded;
93 `proof_count` disagreements. Sample: 50 drawn, stratified 30/9/6/5, **no
content differences**. `VERDICT Pass (exit 0)`.

Then twice against the **live** `~/.local/share/memgarden/memgarden.db` with
`memgardend` up on :9100:

* `--dump-only` — exit 0, preserving 4 sessions, 7 retain jobs, 2 benefit-ledger
  rows and 3,674 metric snapshots;
* a full run — exit 1, which is correct: that database has not been migrated.
  Temporal self-consistency reports `over 0 of 4 banks` rather than a green row.

`user_version` 8 before and after, node and session counts unchanged, and
:9077 / :9090 / :9100 all still listening on the same pids.

---

## Diverged from legacy

| divergence | why |
|---|---|
| **A three-tier verdict instead of a single pass/fail** | AC-3's "카운트 일치" is only meaningful for the counts that *can* be equal. A single boolean would either lie or fail forever |
| **Exit 2 as a distinct review state, with an acknowledgement hash** | A recomputed-adjacency drift is a signal for a human, not a migration failure. Collapsing the two teaches people to ignore the failure; leaving it permanent teaches the same thing faster |
| **The temporal Tier-1 gate is against *our own* rule, not legacy's** | Legacy's rule applies no 24-hour window to its neighbour query. An equality against it is a gate that can never pass; an equality against ours catches every failure such a gate was wanted for |
| **`semantic` is reported with no band at all** | The number is 10× off for a CE-7 reason (`embed_task.rs:178-179`), not a migration reason. A band derived from it would be a band on a bug |
| **The sample's PRNG is five lines of `splitmix64`, not a crate** | Determinism from `(seed, snapshot)` is the whole requirement, and Phase D adds no dependency |
