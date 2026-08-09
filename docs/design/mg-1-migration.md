# MG-1 — migrating four legacy banks

`crates/memgardend/src/migrate/` and `src/bin/mg_migrate.rs`. Two of three
Phase D PRs so far. **D1 (`snapshot`) writes no database row anywhere**: it
reads legacy over HTTP, freezes the archive on disk, and refuses. **D2
(`import`) issues no HTTP at all**: it reads that frozen directory and writes
the banks into a MemGarden database. D3 adds `verify`; this note grows with it.

Jump to D2: [what `import` does](#what-import-does) ·
[five plan errors](#five-things-the-plan-got-wrong-about-our-own-side) ·
[the measured run](#manual-verification--d2-2026-08-06).

---

## The finding the phase rests on

Legacy already ships its own migration format.

> `GET /v1/default/banks/{bank_id}/document-transfer` — *"Export documents
> (extracted facts, entity names, causal links, chunks) from a bank as a
> transfer ZIP archive. **Embeddings and database ids are not included —
> importing re-embeds with the target bank's model and re-resolves entities.**
> Consolidated observations are excluded unless `include_observations=true`."*

The sentence is at `hindsight-api-slim/hindsight_api/api/http.py:6795`, the
implementation at `engine/transfer/export.py:171`, the payload types at
`engine/transfer/schema.py:46-130`.

Legacy's own answer to *"how do you move a bank"* is: **carry the facts, drop
the vectors, drop the ids, drop the derived links, re-derive on arrival.** That
is what MG-1 does, and it is the supported path rather than a compromise we
invented.

---

## What `snapshot` does

Five endpoints, all `GET`, ~2 s wall for the whole corpus:

| endpoint | what it is |
|---|---|
| `/v1/default/banks` | the bank list, written verbatim to `banks.json` |
| `…/{bank}/stats` | **the count oracle** — the numbers AC-3 compares against |
| `…/{bank}/document-transfer?include_observations=true` | **the content oracle** |
| `…/{bank}/documents` | `content_hash`, `memory_unit_count`, and the row's other seven fields verbatim |
| `…/{bank}/memories/list?limit=1[&state=invalidated]` | live facts, and the curation archive beside them |

Output:

```
<out>/banks.json                      GET /v1/default/banks, verbatim
<out>/stats.json                      the frozen oracle, keyed by bank id
<out>/<bank-slug>.zip                 the archive as legacy produced it
<out>/<bank-slug>/manifest.json       … unpacked beside it
<out>/<bank-slug>/documents/*.json
<out>/<bank-slug>/observations.json
<out>/SHA256SUMS                      sha256sum(1) format, every file above
```

### Why a snapshot rather than a live read at import time

The legacy banks are **still being written** — both hook sets are wired in
`~/.claude/settings.json` and `claude-code::bank-b.last_document_at`
moved during the measurements below. AX-2 already paid for this lesson
(`docs/design/ax-2-recall-quality.md:35-54`): *"a re-fetch returns a different
corpus and would silently invalidate every label."* Three consequences, the
third decisive:

1. `import` and `verify` must see the same bytes, or a fact written between
   them reads as a count mismatch that is not a migration defect;
2. the run reproduces from an artifact rather than from a daemon;
3. **AC-3's evidence has to outlive the legacy daemon.** Phase F retires
   :9077, and a verification report whose oracle is a process that no longer
   exists is not evidence.

### Nothing but GET

Cross-PR rule 1 in the plan is *"`mg-migrate` contains no code path that issues
anything but `GET` to :9077."* The structural form of that guarantee:
`snapshot::get` is the only function in the module that constructs a request,
it takes a URL and returns bytes, and every endpoint above goes through it. The
checkable form:

```
$ grep -rnE '\.(post|put|patch|delete)\(' crates/memgardend/src/migrate/
$ grep -rc 'RequestBuilder\|Method::' crates/memgardend/src/migrate/snapshot.rs
0
```

Both empty. `snapshot::get` holds the module's only `reqwest` request, and it is
`client.get(...)`.

---

## The integrity assertions

`snapshot` refuses, non-zero, on each of these — **each with its own error
variant and its own test that breaks exactly it.** A refusal that cannot say
which property stopped holding is a refusal nobody acts on.

| refusal | measured today | why it is a refusal and not a warning |
|---|---|---|
| `manifest.schema_version != 1` | 1 in 4/4 | `schema.py:23` is the archive's version contract; a bump means this parser is no longer entitled to an opinion |
| manifest counts disagree with the files beside them | exact in 4/4 | every count check below reads the manifest; checked first so nothing reconciles against a number already wrong |
| a document's fact count `!=` `/documents.memory_unit_count` | 25/25 | `_load_facts` is `ORDER BY document_id, created_at, id` (`export.py:509`), a total order — so `fact_index` is stable *provided no fact was deleted between snapshots*. This is that proviso |
| `/documents` returned fewer rows than its own `total` | 25 of 25 | the page carries its truncation flag (`api/http.py:1564-1567`) and `limit` defaults to 100; the first version of this PR deserialized only `items` and threw it away |
| `manifest.document_count != /stats.total_documents != /documents.total` | equal in 4/4 | the **reverse** direction: a legacy document with no archive document behind it. Every other document check runs archive → legacy and cannot see one disappear |
| a snapshotted bank whose archive did not load back | 4/4 loaded | `load_dir` recognises a bank archive by its `manifest.json`, so a directory without one is skipped **in silence** — one fewer `ok` line, checksums written and verified, exit 0 |
| `facts + observations != stats.total_nodes` | exact in 4/4 | the coverage identity. A shortfall is most plausibly `_load_observations`' stale-source skip (`export.py:466`), which logs and continues |
| archive causal count `!= stats.caused_by` | exact in 4/4 | `caused_by` is the only authored link type and the only one D2 copies rather than re-derives |
| `state=invalidated` total `!= 0` | 0 in 4/4 | curation **moves** the row into `invalidated_memory_units` (`curation.py:11,141-143`) and `_load_facts` reads `memory_units` (`export.py:489`), so an invalidated fact cannot be exported — the exposure is the curation archive being left behind with nothing saying so |
| a null `original_text` | 25/25 non-null | `schema.py:125` types it `str \| None` — precisely the assumption that rots. `content_hash` cannot be computed without it |
| `sha256(original_text) != content_hash` | 25/25 equal | the same construction our own retain uses (`retain/mod.rs:146`); it is the document identity D2 dedups on |
| an observation with empty `sources` | 0 of 1,747 | no provenance means an empty `node_sources` and a `proof_count` deriving to 0 (`consolidate.rs:658-666`) |
| a non-null `observation_scopes` | null in all 1,747, censused | there is no MemGarden column for it, so a value is a **silent drop** — the one shape `deny_unknown_fields` structurally cannot catch, because the field is known and merely unused |
| a `--drop-bank` bank is no longer empty | 0/0 in all four named on the run these numbers come from | "nothing to lose" is only true while it stays true, and a dropped bank can be a live directory |
| a bank id that slugs to `""`, `.` or `..` | 8/8 fine | all three are made of characters `slug()` passes through, and `out.join("..")` is the snapshot directory's **parent** — which is where `unzip` would then extract |

The drop set is **named by the operator, not derived from emptiness**, and it
is passed per run as repeated `--drop-bank` rather than compiled in. Deriving
it from "is it empty right now" makes the emptiness assertion circular and
unable to fire; naming a bank is a claim that it holds nothing, and the run
fails if it does not.

It **defaults to empty, and that is not a degraded mode**. A bank that is not
named gets snapshotted whether or not it has content, and an empty archive is
then skipped at import — so an operator with no such claim to make loses
nothing by making none. The numbers below come from a run that named four.

Whatever is named is frozen into `stats.json` as each bank's `dropped` flag,
and `verify` re-checks the claim from *that* rather than from a drop set
re-supplied on its own command line hours later, which could disagree.

### `deny_unknown_fields` on every archive struct

Legacy's own importer is permissive — it model-validates and ignores what it
does not know (`engine/transfer/importer.py:122-123`). We are a one-way
consumer of a corpus that exists once, so the trade runs the other way: a field
legacy adds in an upgrade and we silently ignore is exactly the *silent partial
success* this phase is built against. Strictness costs a refusal we can read;
permissiveness costs memory we cannot get back.

It is on **`TransferDocument` too**, and that is deliberate rather than
incidental: it is the struct a legacy upgrade would most plausibly grow, and
the plan's first draft scoped the attribute to facts and observations only.

---

## Two fixtures, and the distinction is the point

`crates/memgardend/tests/fixtures/migrate/`.

**`real/`** — a redacted slice of the live `claude-code::bank-a`
archive, carrying the shapes a generator would not invent: a tag list mixing
`file:` tags with a bare document uuid; `causal_relations` pointing **both
forward and backward** within one document (2 each); `occurred_start` /
`occurred_end` null with `mentioned_at` set (78 of 86 facts). The 180,516-char
transcript in `original_text` is replaced with a stand-in and `content_hash`
recomputed over it; everything else is verbatim. `real/README.md` records every
edit.

**`edge/`** — hand-written and labelled synthetic, for shapes **legal per
`schema.py` but absent from today's corpus**: `context: ""`, `original_text:
null`, a non-null `observation_scopes`, an out-of-range `target_fact_index`.
The first of its three banks must **pass** integrity — legal-but-unusual is not
a refusal.

Conflating them is how the plan's first draft came to specify a fact-level
`document_id` and a `context: ""` that no live fact has. **A fact carries no
`document_id` field at all** — the archive's grouping *is* the document — and
`context` is populated in every one of the 3,540 live facts.

---

## The two checks that were missing, and what they have in common

Code review found both, and they are the same shape: **the checks that were
never written are the ones that would have caught something *disappearing*
rather than something *disagreeing*.**

### `state` selects a table, not a predicate

The first version of this PR asserted `state=valid total == unfiltered total`
and explained it as *"`_load_facts` has no `state` predicate, so an invalidated
fact would import as valid."* Both halves were wrong.

`curation.py:141-143`:

```python
# Invalidated facts live in a separate archive table; pick the source
# accordingly. Default (state is None) lists live facts.
is_archived = state == "invalidated"
source_table = fq_table("invalidated_memory_units") if is_archived else fq_table("memory_units")
```

`state` never enters `query_conditions`. So the two GETs ran the identical
COUNT over the identical table and the assertion compared a number with
itself — true by construction, in every bank, forever. Measured live on
`bank-a`: `total` 536, `state=valid` 536, `state=invalidated` **0**.

And the premise was inverted. Curation *moves* the row out of `memory_units`
(`curation.py:11`), which is the table `_load_facts` reads — so an invalidated
fact **cannot** be exported. The real exposure is the mirror image: the
curation archive is silently left behind and nothing notices. The census that
carries information is `state=invalidated`, and it is now the one that runs.

There is no `memories_valid` field in `stats.json` as a result. A number that
is `memories_total` by construction is worse than absent: D3 could gate on it
and get a free pass.

### Every reconciliation ran in one direction

`run()` iterated the loaded archives and looked each one up in the oracle;
`assert_integrity` required every archive document to appear in `/documents`.
Neither reverse was checked, and `/stats.total_documents` — already fetched,
already in `BankStats` — was compared against nothing.

The failure that hides there is silence. `load_dir` recognises a bank archive
by its `manifest.json`, so a directory that ends up without one — an unexpected
zip layout, a partial write, an `unzip` that exits 0 having extracted nothing —
is not a bank archive and is skipped without a word: one fewer `ok` line,
`SHA256SUMS` written and verified, exit 0. `NoArchives` fires only when *all*
of them vanish.

Three comparisons over data already in hand now close it:

* `assert_every_bank_loaded` — every non-dropped `stats.json` entry must have
  an archive behind it (`ArchiveMissing`);
* `manifest.document_count == stats.total_documents == /documents.total`
  (`DocumentCountMismatch`);
* `/documents` `items.len() == total` (`DocumentListTruncated`) — the response
  carries its own truncation flag (`api/http.py:1564-1567`) and the first
  version deserialized only `items`.

---

## Diverged from legacy

| divergence | why |
|---|---|
| **Snapshot to disk, not a live read** at import and verify time | The legacy banks are still being written; AX-2 already paid for this lesson. AC-3's evidence must outlive :9077 |
| **`deny_unknown_fields`** on every archive struct, where legacy's importer is permissive (`importer.py:122-123`) | A silently ignored new field is exactly the failure mode this phase exists against. A one-way consumer can afford to be strict |
| **We refuse to migrate a bank that has a curation archive**, which legacy's exporter simply does not carry | `state` selects a *table*, not a predicate (`curation.py:141-143`), so `document-transfer` cannot see `invalidated_memory_units` at all. Measured 0 today. Losing a retracted fact silently is still losing it |
| **We refuse a non-null `observation_scopes`; legacy carries it** | We have no column. Refusing is the only way a value that arrives is not silently dropped |
| **`snapshot` unpacks the archive itself; the plan's runbook unpacked it by hand** | Every integrity assertion is about the archive's *contents*, so `snapshot` has to read inside the ZIP. See below |
| **Four of the eight banks are not migrated** | Zero nodes, zero documents, zero links, and `hook session-start`'s `POST /v1/banks` (`session_start.rs:159-166`) recreates any of them on first use. `banks.json` preserves every mission verbatim — including `codex`'s hand-written 149-character one — so the string survives the bank not doing so |
| **D2: `memory_nodes.uuid` is a fresh v7**, not a legacy uuid (contrast `recall_bench.rs:222-238`) | The archive carries no ids by design (`export.py:171-193`). A uuid-shaped value that is not a legacy uuid is worse than an honest v7, and `(document_id, fact_index)` in `metadata` is the join key instead |
| **D2: only `caused_by` is copied; `temporal` and `semantic` are recomputed; `entity` is written by neither system** | A semantic edge is a function of the vector space and legacy's vectors are neither exported nor ours; legacy's temporal neighbour query applies no window where `links.rs:69` does. `entity` is exact parity — `counts.py:47-49` derives legacy's at `/stats` time |
| **D2: entity names are normalized and nothing more** — neither carried verbatim (the bench) nor fuzzy-resolved (retain) | Legacy's canonical names are not normalized in our sense, and `write_entities` upserts on `(bank_id, canonical_name)`; but legacy already merged its own spelling variants, so our fuzzy pass only adds false merges — measured 77 names and 22 mentions lost, 33 of them to unrelated entities. §1 |
| **D2: observations get four date columns, a metadata key and their tags that `insert_observation` has no parameter for** | The store helper is shaped for the daemon, where an observation is created *now*. §2 |
| **D2: `documents.created_at` and `memory_nodes.created_at` are the import time**, with legacy's preserved in `metadata` | `documents::upsert` and `nodes::insert_batch` both write `now_ms()`, and a migration does not reshape a store helper retain depends on |
| **D2: a `disposition.mg_import` marker instead of per-bank atomicity** | Every store helper opens its own `Db::write` (`lib.rs:74-82`) and the `_tx` variants are `pub(crate)`; a migration does not get to make seven of them public |
| **D2: a `consolidation_runs` watermark row per bank, instead of a per-fact `consolidated_at`** | Legacy carries the lifecycle per fact for exactly this purpose (`export.py:200-206`); our equivalent is `id > MAX(watermark)` (`consolidate.rs:314-330`), and one row replaces 3,541 column writes |
| **D2: `--replace` or refuse — one mode, not legacy's `skip\|replace\|new-id`** | We have exactly one consumer and one correct answer; three modes would be three untested paths |

### `ponytail:` no DEFLATE code, so `unzip(1)` does it

The plan's §Workspace decision says *"no ZIP code"* and Phase D adds no
dependency — but it also puts `assert_integrity(&BankArchive, &Stats)` in
`snapshot`, and every assertion above is about bytes inside a DEFLATE-compressed
ZIP (`compress_type 8` on all three members). Those two cannot both hold. The
resolution keeps the dependency rule and moves the runbook's manual unpack step
into the binary:

```rust
Command::new("unzip").args(["-q", "-o", "-d"]).arg(dest).arg(zip)
```

Zero new crates, and the on-disk directory the runbook used to produce by hand
is produced here instead — so `import` and `verify` can never be pointed at a
snapshot nobody unpacked. **Ceiling:** `unzip(1)` must be on PATH; the error
names `python3 -m zipfile -e` as the fallback. **Upgrade path, and it is not as
cheap as it looks:** `flate2` is already in `Cargo.lock` (via `hf-hub`'s
`ureq`), but it is a raw DEFLATE codec and this is a ZIP *container* — so
replacing the shell-out needs a central-directory parser (~100 lines) or the
`zip` crate, which is **not** in the lock.

**Second ceiling, named because the first one is the distracting one:** archive
path safety is `unzip`'s, not ours. Nothing here inspects entry names. The
Info-ZIP 6.00 on this box strips a leading `/` and refuses `..` components, but
that is a property of this implementation rather than of the interface. Our own
side is guarded — a bank id slugging to `""`, `.` or `..` is refused
(`UnusableSlug`), since all three survive `slug()` unchanged and `out.join("..")`
is the snapshot directory's parent.

Two smaller ones, in the same spirit:

* **`Stats` is a superset of `/stats`.** The plan names the parameter after its
  headline member, but three checks (`content_hash`, the per-document fact
  count, the invalidated-fact census) need `/documents` and `/memories/list`,
  and those must be frozen with the same provenance and the same lifetime.
  `BankStats` keeps every unrecognised `/stats` field in a `#[serde(flatten)]`
  map so `stats.json` still outlives the daemon intact.
* **`BankStats` is the one struct that does *not* deny unknown fields.** It is
  a measurement, not the migration source; refusing to snapshot because
  legacy's stats page grew a counter is strictness pointed at the wrong thing.

---

## Manual verification — 2026-08-06, against the live daemon

```
$ ss -ltnp | grep -E '9077|9090'
LISTEN 0 5    127.0.0.1:9090 0.0.0.0:* users:(("python3",pid=2120,fd=3))
LISTEN 0 2048 127.0.0.1:9077 0.0.0.0:* users:(("python",pid=13097,fd=19))

$ mg_migrate snapshot --out <scratch>/snapshot-r2
snapshot -> <scratch>/snapshot-r2
drop claude-code::bank-a: empty, not migrated
drop claude-code::bank-f: empty, not migrated
drop claude-code::bank e: empty, not migrated
drop codex: empty, not migrated
ok   claude-code::bank-b: 2021 facts + 1177 obs == 3198 nodes | causal  64 ==  64 | docs 22 == 22 | live 3198 invalidated 0
ok   claude-code::bank-c:     821 facts +  132 obs ==  953 nodes | causal 113 == 113 | docs  1 ==  1 | live  953 invalidated 0
ok   claude-code::bank-a:        278 facts +  258 obs ==  536 nodes | causal   4 ==   4 | docs  1 ==  1 | live  536 invalidated 0
ok   claude-code::bank-d:  421 facts +  180 obs ==  601 nodes | causal  19 ==  19 | docs  1 ==  1 | live  601 invalidated 0
SHA256SUMS written and verified
wall 2.26 s        exit 0

$ sha256sum -c SHA256SUMS
banks.json: OK
claude-code__bank-b.zip: OK
… 39 lines, 39/39 OK …
stats.json: OK

$ ss -ltnp | grep -E '9077|9090'
LISTEN 0 5    127.0.0.1:9090 0.0.0.0:* users:(("python3",pid=2120,fd=3))
LISTEN 0 2048 127.0.0.1:9077 0.0.0.0:* users:(("python",pid=13097,fd=19))
```

Same pids before and after. The MemGarden daemon on :9100 (pid 1786490, mid
shadow-run) was likewise untouched. The snapshot is 23 MB unpacked / 1.94 MB
zipped and lives in the session scratchpad, **not** in the repo — D2
re-snapshots anyway, because the banks keep growing.

**Totals across the four migrated banks:** 3,541 facts, 1,747 observations, 25
documents, 200 causal relations, 5,288 nodes, **0 invalidated**. All eight
banks now have a `stats.json` entry; the four with `dropped: true` carry the
zeroes that justify not migrating them.

**Wall time.** The plan quotes 0.34 s; that is the four `document-transfer`
GETs alone. The full run is ~2 s and additionally does 8 `/stats`, 4
`/documents` and 8 `/memories/list` GETs, four `unzip` invocations, and
sha256s 23 MB. Legacy is never the bottleneck either way.

### The corpus moved three times during this PR

Not an argument — an observation, from three runs of the same command:

```
02:41  bank-b: 2020 facts + 1177 obs == 3197 nodes | docs 21
03:06  bank-b: 2020 facts + 1177 obs == 3197 nodes | docs 21   (+1 doc mid-run)
03:4x  bank-b: 2021 facts + 1177 obs == 3198 nodes | docs 22
```

One new document and one new fact inside an hour, while this PR was being
written and reviewed. That is §Binding decisions #2 (*"the archive, not the
daemon, is the migration source"*) and the runbook's **"step 3 must
re-snapshot"** demonstrated rather than asserted: importing a rehearsal-era
archive at cutover would lose exactly the facts written in between, and a
`verify` that re-fetched would report a count mismatch that is not a migration
defect.

The integrity assertions held on every run, which is the other half of the
point: a growing corpus is not a broken one, and the checks distinguish the two.

### One assumption D2 depends on, now measured

With `document_metadata` preserved (see `DocumentSummary`), the equality D2's
step 2 plans to rely on is checkable from the frozen artifact rather than from
a running daemon:

```
document_metadata == retain_params.metadata: 25 same, 0 different (of 25 documents)
```

D1 does not gate on it — it is D2's field to carry — but the copy that proves
it now survives :9077.

## What D2 inherits, and what it must add

* `archive::load_dir` reads a snapshot directory; `migrate::load_stats` reads
  the frozen oracle beside it. Neither touches the network.
* `snapshot::verify_sha256sums` is the guard `import` runs before writing a row.
* **The causal `target_fact_index` range check is D2's, not D1's**, per the
  plan's split — `edge::legal-but-absent` carries an out-of-range value and
  D1 accepts it on purpose, so D2's test has a fixture that reaches it.
* **`stats.json`'s shape changed in review** and D2/D3 read it: `Stats` gained
  `documents_total`, `memories_invalidated` and `dropped`, and lost
  `memories_valid`. `DocumentSummary` gained a `#[serde(flatten)] extra` map, so
  `document_metadata` — the one `/documents` field with no `schema.py`
  counterpart, and the equality D2's `metadata` plan quietly depends on — is
  now preserved rather than dropped.
* Dropped banks now have a `stats.json` entry with `dropped: true`. Their
  `/stats` zeroes are the evidence for the decision not to migrate them, and
  they stop existing when Phase F retires :9077.

### Left for D2, deliberately — and what D2 did with it

* **`run()` had no automated coverage. It does now.** `snapshot::run_from`
  takes the base URL, `run` is one line delegating to it with `LEGACY_BASE`,
  and `migrate::snapshot::run_tests` stands up an `axum` stub answering the
  five real endpoints from the committed fixtures. Its routing table is keyed
  on URLs built by `endpoint` *itself*, so a change to how a bank id is
  percent-encoded breaks the test rather than passing it. Three tests: the
  happy path end to end (both fixtures, checksums verified, oracle reloaded),
  the `ArchiveMissing` direction — a ZIP that unpacks cleanly and leaves no
  `manifest.json`, which is the exact shape the one-directional reconciliation
  could not see — and a 404 carrying its `detail` body.

  The stub's transfer ZIPs are built with `zip(1)`, for the same reason
  `unpack` shells out to `unzip(1)`: no crate here speaks the ZIP container,
  and a test is not the place to add one.

* Two assertions D1 does **not** make, both currently 0 across the corpus: a
  non-null `observation_scopes` on a *fact*, and a non-zero
  `manifest.mental_model_count` / `directive_count` / `webhook_count`.
  **Both are now refusals in `assert_importable`,** before any write. The
  second one stopped being theoretical when `--replace` gained the
  `mental_models` delete (§Binding decisions #5d): a legacy bank that grew a
  mental model would have had it dropped on this side *and* not carried from
  that one.
* `manifest.archive_type` and `includes_history` were parsed and asserted
  against nothing. **Both are now refusals.** A `"bank"` archive carries mental
  models, directives and webhooks in files `load_dir` never opens
  (`schema.py:149`), and `includes_history: true` means an edit history
  `schema.py` does not say where to find; either would move the documents and
  leave the rest behind in silence.

  All four of these live in `assert_importable` rather than in D1's
  `assert_integrity`, and the line is principled rather than convenient:
  **`snapshot` asserts that the freeze is complete and reconciles; `import`
  asserts that we can carry what was frozen.** A `"bank"` archive is a
  perfectly complete freeze — it just holds content this importer has no home
  for. `import` re-runs `assert_integrity` first, so both sets fire before any
  row is written either way.
* `collect_files` skipped any file named `SHA256SUMS` at any depth, not only
  the one at the root. **Fixed** — it now excludes exactly `root/SHA256SUMS`.
  Harmless today because legacy emits no such entry, and a hole of precisely
  the shape this module exists against if it ever does: not a mismatch, an
  absence. `a_sha256sums_below_the_root_is_still_checksummed` plants one and
  flips a byte in it.

**D1's deferred list is now empty.**

### The discard ledger — every archive field, and where it went

Censused over the whole live corpus on 2026-08-06 (25 documents, 3,541 facts,
1,747 observations), because "the checks that were never written are the ones
that would have caught something *disappearing*" is this repo's recurring
review finding and the only defence is an exhaustive list.

| field | treatment |
|---|---|
| fact `text`, `fact_type`, `context`, `occurred_start/end`, `mentioned_at`, `tags`, `entities`, `causal_relations`, `metadata` | written to a column or a child table |
| fact `event_date` | **derived, not copied** — `occurred_start.or(mentioned_at)`, `writes.py:80` parity. Legacy's is its NOT NULL fallback (`schema.py:57-58`), and **0 of 3,541 facts have neither `occurred_start` nor `mentioned_at`**, so consuming it would only ever put a value in the column our own rule would not produce |
| fact `created_at` | `metadata.legacy.created_at` — `insert_batch` stamps `now_ms()` and a migration does not reshape a store helper |
| fact `consolidated_at` | **deliberately collapsed** into one `consolidation_runs` watermark row (§Binding decisions #5b): one INSERT instead of 3,541 column writes, and the watermark is what our scheduler actually reads (`consolidate.rs:314-330`) |
| fact `consolidation_failed_at` | `metadata.legacy.consolidation_failed_at`, **when set — and it is set on exactly one fact in the corpus.** A single watermark rowid cannot say "everything up to here is consolidated except this one", and lowering it to reach that fact would re-consolidate every fact after it. Carried where it stays recoverable rather than asserted away |
| fact `chunk_index` | **dropped.** No `chunks` table and no `memory_nodes.chunk_id` column; the plan's out-of-scope table already carries the row and its re-entry criterion (a UI that shows which transcript chunk a fact came from) |
| fact `observation_scopes` | **refused if non-null.** Null in all 3,541 |
| observation `text`, `tags`, `occurred_start/end`, `mentioned_at`, `sources` | written — see §2 above for the three date columns `insert_observation` has no parameter for |
| observation `event_date` | derived by the same rule the facts use |
| observation `proof_count` | **derived, never carried** (`recount_proof_tx`). Legacy's stored value is Tier 2 |
| observation `observation_scopes` | refused if non-null (D1). Null in all 1,747 |
| document `id` | `documents.doc_key` |
| document `original_text` | **read, hashed, discarded.** The hash is the document identity; there is no column for the text (`0001_init.sql:18-27`) |
| document `created_at` | `metadata.legacy_created_at` |
| document `retain_params.metadata` | `documents.metadata`, and **asserted equal to `/documents.document_metadata`** |
| document `retain_params.context` | **dropped.** `retain_params` is exactly `{context, metadata}` in 25/25, and `context` is `"claude-code"` in all of them — the bank's own source, which our own retain does not record either (`routes/retain.rs:554-568` builds `documents.metadata` without one). Parity, not loss |
| document `tags` | **dropped.** `document_tags` exists in the schema and has **zero readers or writers anywhere in the workspace**, and the same tag list is repeated on every fact of the document — measured: in 25/25 documents, every document tag also appears on at least one of its facts, so `node_tags` already carries the multiset MG-2 gates on |
| document `chunks` | parsed and discarded — no table, and retain re-derives chunking from the transcript |
| `manifest.mental_model_count` / `directive_count` / `webhook_count` | **refused if non-zero.** 0 in all five banks |
| `manifest.archive_type` | **refused unless `documents`** |
| `manifest.includes_history`, `bank_rows_json_encoding` | parsed, asserted against nothing. `false` / absent in all five |

---

## What `import` does

`mg_migrate import --snapshot <dir> --db <path> [--replace] [--defer-embeddings]`.

Ten steps per bank, **each its own top-level write**. There is no enclosing
transaction and §Binding decisions #5 of the plan explains why: every store
helper opens its own `BEGIN IMMEDIATE` on its own pooled connection
(`lib.rs:74-82`), the composable `_tx` variants are `pub(crate)`, and making
seven of them public to buy atomicity for a one-time binary is the wrong trade.

| # | step | the call |
|---|---|---|
| 0/1 | bank row, legacy mission and disposition, marker `running` | `banks::create` **or** `banks::update` — `create` is a plain INSERT (`banks.rs:16-21`) and fails against a bank that already exists |
| 2 | documents | `documents::upsert` then `documents::set_content_hash`, the idiom retain follows (`retain/mod.rs:391`) |
| 3 | facts | `nodes::insert_batch` with `NewNodeWithTags` |
| 4 | entities | `entities::resolve_fact` + `graph::write_entities`, per document |
| 5 | causal links | `graph::insert_links`, weight `CAUSAL_LINK_WEIGHT`. **The only link type copied rather than derived** |
| 6 | observations, **before** the temporal pass | `Embedder::embed_batch` then `consolidate::insert_observation`, then one write stamping the four date columns and the tags that call has no parameter for |
| 7 | temporal links, once over every node in the bank | `links::temporal_links` + `graph::insert_links` |
| 8 | fact embeddings, and the semantic links that ride with them | `embed_task::drain_once`, bounded to 3 calls with a backlog-must-shrink check |
| 9 | the consolidation watermark | `consolidate::start_run` + `finish_run` with `watermark = MAX(memory_nodes.id)` |
| 10 | marker `done` | `banks::update` |

Steps 6 and 7 are in that order and not the other one. `temporal_links` pairs
same-`fact_type` only (`links.rs:66-67`), so observations link only to other
observations, and only to the ones already inserted when the pass runs — the
plan's first draft had the two steps reversed and gave 1,747 nodes no temporal
edges at all.

### The marker, because there is no transaction

`banks.disposition` gains `{"mg_import": {"state": …, "at": …, "snapshot": …}}`
before the first node and flips to `"done"` after the last step. A failed bank
therefore leaves rows, and the guarantee is that they are never mistaken for a
finished import: `import` refuses a bank whose marker says `running` without
`--replace`, and `verify` will fail Tier 1 on it. `snapshot` is the sha256 of
the snapshot directory's own `SHA256SUMS` — which pins every other file — so a
bank imported from a *different* snapshot than the one being verified is caught
rather than counted.

The marker is checked **before** the row count, and both halves are needed:
a bank that failed at step 2 has zero nodes and a `running` marker, and a bank
the shadow run wrote into has nodes and no marker at all.

`ponytail:` it records *whether* a run finished, not how far it got. If a
partial bank ever needs resuming rather than redoing, that is when a step
counter earns its keep.

---

## What running it turned up — and the one correction that was itself wrong

The plan's Critic Revisions section records the pattern: *"four of six blockers
were on the MemGarden side of the wire. The legacy side was measured; our side
was read from design notes."*

Writing D2 turned up five more of the same shape, all on our side. It also
turned up **one correction that repeated the pattern in the other direction**:
§4 below started life as *"the plan's temporal band is wrong"*, derived by
measuring our side carefully and legacy's not at all, and code review refuted
it in one query. That section is left in as the refutation rather than deleted,
because the thing it found on the way — that legacy stores **zero** derived
edges on observations — is worth more than the claim it replaced, and because a
document arguing that unmeasured claims are the hazard should show its own.

Scorecard: §1, §2, §3, §5 and §6 are corrections to the plan and stand. §4 is a
correction to *this document's own first draft*. §4b is a defect in production
code that neither the plan nor D1 knew about.

### 1. Legacy's entity names are not normalized — but normalizing is the *whole* of what step 4 needs, and the first version did more

The plan's step 4 cites `recall_bench.rs:241-262` **and** `retain::write_graph`
as if they were the same call. They are not, and neither of them is right.

**The bench's raw names are wrong.** It hands `graph::write_entities` legacy's
names as they come, which is harmless for a corpus nothing ever retains into
again. Measured on the archive, legacy's canonical names are `Agent`, `BM25`,
`Claude`, `CE-9a` — **not lowercased**. `entities::normalize` is trim +
lowercase (`entities.rs:30`) and `write_entities` upserts on
`(bank_id, canonical_name)`, so raw names would put the daemon's next `claude`
beside the migrated `Claude` as a second entity: the graph arm's co-membership
signal split in half, silently, from the first prompt after cutover.

**And retain's path is wrong too, which took a measurement to see.**
`retain::write_graph` calls `entities::resolve_fact`, which is `normalize`
*plus* a fuzzy pass against the bank's existing entities. The first version of
this module did the same, on the reasoning that matching the production path
must be right. The corpus disagreed: over the four banks the fuzzy pass
dissolved **77 of 3,917 distinct normalized names** into other entities, taking
22 mentions with them, and **33 of the 77 have no plausible variant to have
merged into**:

```
ce-4       → ce-1          phase e   → phase a       prd      → pr
ci.yml     → cli.mjs       shell     → schedule      degraded → derived
mindvault  → invaliddata   idx_links_to → links      linkedin_obsidian → obsidian
```

The mechanism is in the scoring. `resolution_score` is
`ratio*0.5 + overlap*0.3 + temporal*0.2` (`entities.rs:160-176`), so two names
sharing a fact on the same day already hold **0.5 of the 0.6 gate before their
names are compared at all** — the effective name-similarity bar is about
**0.2** whenever co-occurrence and recency are both satisfied, which in a bulk
import they always are: every fact's names co-occur densely and every date is
clustered. That is how `ci.yml` becomes `cli.mjs`.

**The part the first version had backwards is that the fuzzy pass buys the
migration nothing.** Its job is to merge spelling variants — and legacy already
merged those, in legacy's own space, before exporting. Normalization is the
load-bearing half; the fuzzy half is pure downside here.

**Two sentences an earlier draft of this section had, which review refuted, and
which are worth keeping as corrections rather than deleting:**

* *"in ordinary retain the candidates are a handful of entities from one
  chunk."* They are not. `load_resolution_context` is
  `WHERE bank_id = ?1 ORDER BY last_seen DESC LIMIT 5000` (`graph.rs:54-72`) —
  **bank-wide**. The per-chunk quantity is `nearby` (`entities.rs:224-228`).
  So the dense regime that produced the 77 merges is not unique to the import,
  and every retain after cutover runs against the 3,917-entity bank this
  migration creates.
* *"a later `CE-4` hits the same row whether or not `resolve_fact` scores it
  over 0.6."* It does not necessarily. `retain::write_graph` calls
  `resolve_fact` **before** `graph::write_entities` (`retain/mod.rs:600-618`),
  so the upsert key sees the *resolved* name, and `resolve_fact` takes the
  argmax with no exact-match short-circuit. An exact match holds `1.0*0.5` plus
  a temporal term that is **0** once the migrated entity's `last_seen` — its
  legacy date — is months old, so a fresher, co-occurring candidate can
  outscore it.

The second is a standing CE-7 property, not something this PR introduces or is
entitled to change; `book/src/roadmap.md` records it with the shape of a fix
(an exact-match short-circuit is one line) and the measurement that would
justify one. What this PR fixes is the migration's own use of it.

So step 4 normalizes, dedups within the fact, and stops — through
`entities::normalized_mentions`, which `resolve_fact` now calls for its own
first half, so "these two agree" is held by the compiler rather than by a doc
comment. Two things it deliberately does not do that retain's path does: the
fuzzy resolution above, and `parse::MAX_ENTITY_CHARS`' 256-character cap, so a
long legacy name becomes a `canonical_name` our own extraction could not have
produced. Longest name in the corpus is under 64 characters.

The result is exact rather than approximate:

| | archive | database |
|---|---|---|
| distinct normalized entity names | 3,917 | **3,917** |
| entity mentions | 10,379 | **10,379** |

Which is worth more than the correctness: **it turns entities from something
MG-2 could only report into something it can gate on.** And it cut the import
from 207 s to 167 s, because the resolver was O(mentions × candidates) over a
candidate list that grew all the way to 2,491.

### 2. `insert_observation` writes six things fewer than the archive carries, and the plan never mentions any of them

`insert_observation_tx` inserts exactly `uuid, bank_id, fact_type, text,
embedding, embedding_model, mentioned_at, created_at, updated_at`
(`consolidate.rs:139-155`). `mentioned_at` is `now`. **`event_date`,
`occurred_start`, `occurred_end`, `metadata` and the tags are never written at
all.**

That is right for the daemon, where an observation is created *now* out of
facts it has in hand. For a migration it drops:

* **four date columns on a third of the corpus.** All 1,747 live observations
  carry an `event_date` and a `mentioned_at` (241 carry `occurred_*` too),
  censused. And `mentioned_at` is worse than absent — MG-2's 50-sample diff
  joins observations on `(text, mentioned_at)`, so the wall-clock substitute is
  a *fabricated* value that reads as real. The stamp is therefore
  unconditional, not a `coalesce` over `insert_observation`'s `now`: a
  `coalesce` cannot write a NULL, which would have made the post-condition
  *"no observation has a NULL `mentioned_at`"* true however badly the line was
  broken;
* **§Binding decisions #4's observation identity key**,
  `{"legacy":{"observation_of":[…]}}` — the archive's `sources` array verbatim.
  A binding row of the plan's identity table, and the first version of this
  module wrote no observation `metadata` at all. The provenance survives in
  `node_sources` either way, but the *join* MG-2 was specified to use would
  not — and the duplicates are kept in the metadata where `node_sources`
  collapses them, so **the database now holds both 2,200 and 2,114** and MG-2
  can explain the difference once the archive is gone;
* the tags MG-2's Tier-1 multiset gate counts.

`import` writes all six in one transaction after the inserts, rather than
paying 1,747 more `BEGIN IMMEDIATE`s for one `INSERT OR IGNORE` each through
`nodes::add_tags`.

**What the stamp does *not* do is create the temporal edges, and an earlier
draft of this section said it did.** Step 7 builds its `TimedNode`s from the
archive, not from the database, so deleting the whole second write leaves all
34,804 observation temporal edges exactly where they were. Code review caught
it; the counterfactual was invented and the test named after it would have
passed with the function deleted.

The real temporal consequence is one PR later and is stronger: **D3's Tier-1
self-consistency check re-runs `links.rs:62-92` over the *migrated nodes'* own
`(fact_type, event_date)`.** Observations with a NULL `event_date` produce zero
edges in that reference run against 34,804 stored, so the check fails — and
that check is the plan's replacement for the equality against legacy that can
never hold. The stamp is what makes the stored graph reproducible from the
database rather than only from an archive Phase F deletes.

### 3. The port guard as specified refuses the run the same PR asks for

§Binding decisions #8 says *"`import` refuses to run if anything is listening
on the configured port — one `TcpStream::connect`"*, full stop. D2's own manual
verification, four sections later, is *"full import of the real four-bank
snapshot into a scratch database **with the daemon left running on :9100
untouched**"*, and the runbook's step 2 is a zero-downtime rehearsal against
`--db /tmp/mg-rehearsal.db`. The guard as written refuses both.

The property worth guarding is not "a daemon exists", it is "a second writer is
about to open the file a daemon already has". `assert_daemon_not_holding`
requires **both**: something listening on `cfg.bind` **and** `--db` resolving
to the same file as `cfg.db_path`. The cutover run still refuses while the
daemon is up; the rehearsal proceeds.

`ponytail:` still TOCTOU by construction, and still blind to a daemon on
another port holding the same file. It is a footgun guard for the operator who
forgot, not a mutual-exclusion primitive; the real protection is SQLite's write
lock plus the fact that the cutover is a two-line manual runbook. Upgrade path
if it is ever automated: an advisory `File::lock()` on a sidecar, the primitive
Phase C already uses.

### 4. Legacy stores **zero** temporal and semantic edges on observations, so ours are a new edge class and not a bigger number

This started as a claim that the plan's Tier-2 temporal band was wrong. **The
claim was wrong**, review refuted it, and the refutation is worth more than the
claim was.

The draft compared our 105,016 temporal edges against legacy's 43,657 and got
2.41×, concluded the plan's `[1.45, 1.75]` was facts-only and told D3 to throw
it away. The missing question is the one this whole document is about: **is
legacy's number facts-only too?** It is, and it is checkable without reading a
line of legacy source. Decomposing `GET /graph` for `bank-a` by each
endpoint's `fact_type`, after removing the 1,199 duplicate edge ids the
projection carries:

| link type | edges with no observation endpoint | `/stats` | edges touching an observation |
|---|---|---|---|
| `temporal` | **3,269** | **3,269** | 6,732 |
| `semantic` | **4,603** | **4,603** | 9,080 |
| `caused_by` | **4** | **4** | 8 |

Exact, all three. `/stats` counts stored rows; every observation-touching edge
in `/graph` is a *visualization copy*, which `memory_engine.py:7723-7724` says
in as many words — *"Observations inherit links from their source memories via
`source_memory_ids`"*. **Legacy never stores a temporal or semantic edge on an
observation**, and its source agrees: the only two temporal insert sites are
the retain path over newly-retained facts (`link_utils.py:378,403-404`) and the
relink path, whose victim query is `WHERE … fact_type IN ('experience','world')`
(`engine/memories/pg/graph.py:606`). Consolidation, the only thing that creates
observations, makes no link call at all.

So the apples-to-apples comparison is **fact-to-fact against fact-to-fact**:

| | edges | vs legacy |
|---|---|---|
| ours, from a fact | **70,212** | 43,657 → **1.61×** — *inside* `[1.45, 1.75]` |
| ours, from an observation | **34,804** | legacy has **no counterpart**, so no ratio exists |
| ours, total | 105,016 | not a meaningful ratio against anything |

**The plan's band stands, and D3 should gate on it — over fact-to-fact edges
only.** A band around 2.4 on the total would be the worse failure: it would
pass a run in which the fact-edge rule silently broke, as long as the
observation edges made up the difference. Observation temporal edges get the
framing §5e already uses for observation *semantic* edges — reported, unbanded,
because legacy has no prior.

Two smaller corrections fall out of the same decomposition:

* **The plan's "213 edges at weight exactly 0.3" is a `/graph` number.** Only
  **72** of those 213 are stored fact-to-fact edges; 141 are projection copies.
  The conclusion survives on 72 and independently on the source, but `/graph` is
  not `memory_links` and the plan's own finding #3 says so.
* **"Observations have no semantic adjacency" is parity, not divergence.**
  §Binding decisions #5e presents it as a MemGarden gap to be recorded in
  `docs/parity-gaps.md`; legacy stores 0 too (4,603 == 4,603 above). It is a
  post-condition worth asserting and *not* a divergence row.

### 4b. Semantic links only ever form inside one embedding batch — every MemGarden database, not just this one

Our semantic count came out at **6,890 against legacy's 65,127**. The first
draft blamed the streaming build and, by its own arithmetic, accounted for
about 2× of a 10× gap. Review called that insufficient and it was. The real
cause is measurable and much sharper.

Two measurements over the migrated database:

1. **Every one of the 6,890 semantic edges connects two nodes whose rowids
   differ by at most 7.** `embedding.batch_size` is 8 (`config.py:405`). Not one
   edge crosses a batch boundary.
2. Over our own migrated vectors, a whole-corpus top-20 pass would emit
   **68,537** edges — **97 % of every node's top-20 neighbours clear the 0.7
   threshold** (per-bank top-1 cosine p50 0.86–0.95). Legacy's 65,127 is within
   5 % of that.

So the threshold is right and the embedding space is fine. The mechanism is at
`embed_task.rs:178-179`: `graph::node_types(&db, &ids)` is built from **the
just-embedded batch's ids only**, and `links::semantic_links` drops any
neighbour missing from that map (`links.rs:143` — *"a candidate missing from it
(deleted between the KNN and this call) is skipped rather than linked blind"*).
The KNN correctly returns the best 100 neighbours in the bank; all but the
handful that happen to be in the same batch of 8 are then discarded.

**This is a CE-7 production defect, not a migration one.** Every MemGarden
database ever built — including every one the shadow run produced — has a
semantic arm roughly a tenth as dense as the rule intends. The fix looks like
one line (widen `node_types` to cover the neighbour ids as well as the batch
ids), and it is deliberately **not** in this PR: it changes the recall graph of
every database by 10×, which is an AX-2-measured CE-7 follow-up and not
something a migration gets to do on the way past. A migration whose graph was
unlike every other MemGarden database would be the worse outcome.

Recorded here rather than in a comment because it is the answer D3 needs before
it sets a semantic band: **do not band this number until CE-7 has decided.**

**CE-7 decided on 2026-08-09.** The fix was the one line predicted here —
`node_types` widened to cover the neighbour ids as well as the batch ids, which
also meant moving the KNN ahead of the type lookup so the neighbour ids exist
to ask about. Re-importing this same snapshot: **6,918 → 62,199 semantic edges,
0.11× → 0.96×, out-degree max 7 → 20**. The prediction of ~10× held at 9.0×,
and max out-degree moving from `batch_size - 1` to `SEMANTIC_LINK_TOP_K` is the
cleanest evidence that the fact_type filter had been acting as a batch filter.

Holding it out of the migration PR was right for the reason given, and it is
worth keeping the reason legible now that both halves have happened: the
migration produced a graph exactly as dense as every other MemGarden database
of its day, and the fix then moved all of them together.

**AX-2 has since been re-run, and the density bought nothing.** The gold corpus
rebuilt through the fixed worker holds 43,830 semantic edges against 681 — a
64× change, out-degree max 3 → 20 — and recall@10 went **0.3881 → 0.3792**,
nDCG@10 0.3236 → 0.3168, with no stratum improving (ledger line 11). Read as
*no measurable gain* rather than *a regression*: 0.9 points over 14 scored
queries is inside what this set resolves. But the assumption written above — and
in the README until now — that these numbers were being held down by the thin
graph is retired. They were not.

### 5. §5 and the runbook disagree about `sessions`, and the reason given for the winner is not the right reason

§Binding decisions #5 says `--replace` *"does **not** touch `sessions`,
`retain_jobs`, `benefit_ledger` or `metric_snapshots`"*. The runbook's step-3
comment says it *"purges nodes+documents+entities+mental_models+
consolidation_runs+sessions"*, and D2's own test list says it *"**deletes** the
bank's `mental_models`, `consolidation_runs` and `sessions` rows"*. Two sites
against one, and the two are the later ones.

An earlier draft of this section attributed the winning side to §5c and cited
`parity-gaps.md`'s *"every session starts at offset 0"* as the reason. Both
halves were wrong, and review caught them. §5c's `DELETE FROM sessions` is a
**shell comment inside a runbook block**, introduced by *"Making that true is a
runbook step, not an inference"* — it does not say `--replace` does it. And the
offset-0 goal is not served by deleting the rows at all: §5c's own analysis is
that `confirmed_offset` is consulted **only** when `state::load` returns `None`,
so it is `rm -f ~/.local/share/memgarden/hooks/*.json` that produces offset 0,
and the `sessions` row contributes nothing either way.

The honest reason is the plain one: a `sessions` row describing a corpus that
no longer exists is stale, and two of the three plan sites say to delete it.
That is sufficient, and it is what the code does.

It is still **measurement data** (AC-2/AC-6), so `purge` now prints the row
count before deleting it — the runbook's step 3a dump is a runbook step and
nothing in the binary can enforce it, but an operator who skipped it finds out
while the terminal still says how much there was. One caveat the runbook
inherits: step 3a is `mg-migrate verify --dump-only`, **which does not exist
until D3**. Until then the only rehearsal-safe path is `--db <scratch>`, which
is what the runbook's step 2 already says.

### And a sixth, which is not an error but a fact that moved

**A fifth non-dropped bank appeared.** `claude-code::memgarden` exists in
legacy as of 2026-08-06 with 0 nodes and 0 documents. It was not named to
`--drop-bank` — the drop set is *named* precisely so the emptiness assertion is
not circular — so `snapshot` archives it, and `import` has to
decide what to do with an empty archive.

It skips it and prints `skip claude-code::memgarden: empty archive, not
migrated`, for the same reason the four named banks are dropped: creating a
bank row whose only content is a mission string puts a number in the AC-3
report that overstates what was verified, and `hook session-start`'s `POST
/v1/banks` (`session_start.rs:159-166`) recreates it on first use.

---

## Temporal links are not legacy's, and `recall_bench` said otherwise

`recall_bench.rs:264-271` used to claim that passing the whole corpus as both
sides of `temporal_links` *"converges to the same edge set (a node's 20 best
24h-neighbours) without replaying the ingest order"*. It was never measured and
it is wrong; D2 corrects it in the tree, because leaving it reproduces the
error in the next reader — it is where the plan's first draft got the claim
from.

Measured three ways over the archive: whole-corpus **70,192**, replayed by
`chunk_index` **68,781**, replayed by the `created_at` batch boundary
**69,771**. Ordering moves the result by 2 %, so it is not nothing — a rolling
window caps each node against the neighbours it had *at the time*, the whole
corpus caps against every neighbour that ever existed.

And none of the three is legacy's 43,657, because **the rule differs**, not the
order: `fetch_temporal_neighbors` (`engine/db/ops_postgresql.py:562-593`) takes
the 20 nearest by `event_date` in each direction and applies **no 24-hour
predicate**, while `links.rs:69` filters on one. The proof is on legacy's own
side: **72 stored fact-to-fact temporal edges in `bank-a` carry weight
exactly 0.3**, the `max(0.3, 1 − h/24)` floor, reachable only at `h ≥ 24`.

Two precisions the plan does not make, both from §4 above. The 0.3 census is
**72, not the 213** the plan quotes — that number is over `/graph`, where 141
of the 213 are projection copies. And *"no 24-hour predicate anywhere in it"*
is true of `fetch_temporal_neighbors` and **false of legacy's rule as a
whole**: the within-batch half applies `time_diff_hours <= time_window_hours`
(`link_utils.py:400`) and the relink path filters `if time_diff_h > 24:
continue` (`engine/memories/pg/graph.py:664`, commented *"Mirror the 24h window
enforced at retain time"*). Legacy's rule is **hybrid** — windowed on two paths
and unwindowed on the LATERAL neighbour query — which is a better explanation
of a 1.61× than a rule with no window at all would be.

---

## What `--replace` deletes, and the two things it must not

One transaction, raw SQL through `Db::write` because there is no store helper
for any of it and Cross-PR rule 5 forbids growing one for the importer's sake
— `nodes::delete` is per-row, and `banks::delete` would cascade `retain_jobs`.

| deleted | why |
|---|---|
| `memory_nodes` | cascades `links`, `node_tags`, `node_entities`, `node_sources`; drops `vec_nodes` through the trigger at `0001_init.sql:91-93` |
| `documents` | — |
| `entities` | cascades `entity_cooccurrences` |
| `mental_models` | hangs off `banks`, not off `memory_nodes` (`0006_mental_models.sql:22`), so the three-statement purge in the plan would leave a stale model over a replaced corpus — a wrong answer rather than a missing one, with `vec_mental_models` still holding vectors for text nobody can reach. 0 rows on :9100 and `mental_model_count = 0` in every manifest, so nothing is lost either way |
| `consolidation_runs` | or the stale watermark survives and hides the front of the re-import: measured live, `watermark = 12` against `max(id) = 24` |
| `sessions` | the runbook and D2's test list against §5's stale sentence; a row describing a corpus that no longer exists is stale. **Measurement data** (AC-2/AC-6) — `purge` prints the count before deleting, and the runbook's step 3a dump is the only thing that preserves it |

**`retain_jobs` rows are spared**, and not out of sentiment: a job left
`Pending` whose row vanishes resolves to a 404, and `cmd/retain.rs:498-504`
reads a 404 as `Failed`, which rolls the client cursor back and re-sends.
Deleting them causes re-ingestion, not cleanliness. Sparing them is not free
either — `retain_jobs.document_id` is `ON DELETE SET NULL`
(`0002_retain_jobs.sql:10`), so the `documents` delete severs the join that
makes those rows AC-2 evidence. The test asserts exactly that: the row
survives, its `document_id` does not.

**`--replace` cannot reset the retain cursor**, because the cursor is a *file*:
`SessionState::offset` lives in `<state_dir>/<session_id>.json`
(`crates/memgarden-cli/src/state.rs:105-113`) and the daemon's
`confirmed_offset` is consulted only when `state::load` returns `None`. The
runbook's `rm -f ~/.local/share/memgarden/hooks/*.json` is the other half and
is not optional; without it the replaced content is never re-ingested.

---

## `real-dup/`, the second fixture, and why `real/` could not do this job

The `node_sources` gate is **2,114 distinct pairs, not 2,200 raw** ones,
because `link_sources_tx` is `INSERT OR IGNORE` against the
`(observation_id, source_id)` PK (`consolidate.rs:638-650`). Showing that needs
a fixture with duplicate `(document_id, fact_index)` source pairs in it — and
**all 86 duplicates in the live corpus are in
`claude-code::bank-b`**. `bank-a`, the bank `real/` is
sliced from, has zero: 294 raw source references, 294 distinct.

So D2 takes a second redacted slice. `real-dup/README.md` records every edit;
the shape it carries is 70 facts, 65 observations, **114 raw source references
against 68 distinct**, one observation with more than one distinct source, and
43 of 65 `proof_count` values that disagree with `len(distinct sources)` —
legacy's `or len(source_ids)` fallback (`export.py:457`) against our
unconditional `recount_proof_tx` (`consolidate.rs:658-666`). Tier 2, reported,
never asserted equal.

`test_support::Snapshot` composes the two committed one-bank fixtures into a
single temporary snapshot directory with `stats.json` merged and `SHA256SUMS`
regenerated, which is how multi-bank driving is covered without committing a
third fixture that is just the other two side by side. It reseals on every
edit, because `import::run` verifies the checksums *before* it writes and a
mutation test that forgot would fail on the guard instead of on the property
under test.

---

## Manual verification — D2, 2026-08-06

Against the live legacy daemon (GETs only) and a scratch database, **with
`memgardend` left running on :9100 untouched throughout**.

```
$ ss -ltnp | grep -E '9077|9090|9100'
LISTEN 0 5    127.0.0.1:9090 users:(("python3",pid=2120,fd=3))
LISTEN 0 128  127.0.0.1:9100 users:(("memgardend",pid=1786490,fd=18))
LISTEN 0 2048 127.0.0.1:9077 users:(("python",pid=13097,fd=19))

$ mg_migrate snapshot --out <scratch>/snapshot-d2          # 1.59 s, exit 0
$ mg_migrate import --snapshot <scratch>/snapshot-d2 --db <scratch>/rehearsal.db
```

| bank | docs | nodes | `caused_by` | obs | `node_sources` | temporal (fact / obs) | entities | pending | watermark |
|---|---|---|---|---|---|---|---|---|---|
| `bank-b` | 22 == **22** | 3,198 == **3,198** | 64 == **64** | 1,177 | 1,417 | 63,522 | 2,491 | 0 | 3,198 |
| `bank-c` | 1 == **1** | 953 == **953** | 113 == **113** | 132 | 170 | 19,060 | 286 | 0 | 4,151 |
| `bank-a` | 1 == **1** | 536 == **536** | 4 == **4** | 258 | 294 | 10,652 | 585 | 0 | 4,687 |
| `memgarden` | — | — | — | — | — | — | — | — | *skipped, empty* |
| `bank-d` | 1 == **1** | 601 == **601** | 19 == **19** | 180 | 233 | 11,782 | 555 | 0 | 5,288 |
| **total** | **25 == 25** | **5,288 == 5,288** | **200 == 200** | **1,747** | **2,114** | **105,016** = 70,212 + 34,804 | **3,917 == 3,917** | **0** | — |

Bold is legacy's own `/stats`, **read back from `snapshot-d2/stats.json`** —
the snapshot this run imported, not the plan-era one. That distinction is not
pedantry: the plan quotes 43,637 temporal / 65,107 semantic / 5,287 nodes and
this snapshot froze **43,657 / 65,127 / 5,288**, because the corpus grew by one
fact and twenty edges between them. A document whose thesis is *"the legacy
side was measured, ours was read from notes"* does not get to cite legacy
numbers from a different snapshot than the one it ran against.

Every Tier-1 count that *can* be equal is equal, and **`node_sources` lands on
2,114** — the number §Binding decisions #5b's revision predicted from the
`INSERT OR IGNORE` collapse, reproduced by the importer without being told
about it. The `entities` column is `count(*) FROM entities`, not a sum of the
per-document batches: the first version summed `write_entities`' return maps
and reported 2,939 for the 22-document bank, which is the same
overstates-what-was-verified failure the empty-bank skip exists to avoid. Every
fixture is single-document, so no test could have caught it.

**The entity graph is exact**, and §1 explains why that took two attempts:
3,917 distinct normalized names in the archive against **3,917** entity rows,
10,379 mentions against **10,379** `node_entities` edges. The first version ran
`entities::resolve_fact`'s fuzzy pass and lost 77 names and 22 mentions to
merges like `ce-4` → `ce-1`.

**Two fixtures, because one of them was not a guard.** `edge::two-documents`
pins that legacy's raw names are *not* carried — case variants must collapse.
It does **not** catch the fuzzy pass coming back, and an earlier version of
this note claimed it did: its `Postgres`/`SQLite` pair shares no co-occurring
partner, so it scores under the gate either way, and review found that
reverting the fix left the whole suite green. `edge::fuzzy-merge-bait` is the
fixture that discriminates — two documents sharing a date, one naming `CE-1`
and `Phase A`, the other `CE-4` and `Phase A`, where `ce-4` scores
`0.375 + 0.3 + 0.2 = 0.875` and collapses. Verified both ways: reverting
`write_entities` to `resolve_fact` turns it red with
`["ce-1", "phase a"]` against `["ce-1", "ce-4", "phase a"]`.

Post-conditions MG-2 will gate on, queried against the same database:

```
embedding IS NULL OR embedding_model <> 'fastembed:BAAI/bge-small-en-v1.5'   0
document_id IS NULL AND fact_type <> 'observation'                           0
observations with a NULL event_date                                          0
observations with a NULL mentioned_at                                        0
semantic edges FROM an observation                                           0   (parity — legacy stores 0 too)
entities whose canonical_name <> lower(canonical_name)                       0
facts carrying a legacy (document_id, fact_index) key            3,541 of 3,541
distinct such keys                                                       3,541
observations carrying legacy.observation_of                      1,747 of 1,747
raw source refs in metadata / distinct in node_sources           2,200 / 2,114
consolidation_runs, one per migrated bank, status done, watermark = MAX(id)   4
banks.disposition.mg_import.state                                    done x 4
```

Wall time **167 s** in a `dev` build — down from 207 s once §1's fuzzy resolver
came out, which was O(mentions × candidates) against a candidate list that grew
to 2,491. The plan's ~90 s estimate is a release-build figure.
`--defer-embeddings` moves the embedding half out of the maintenance window
entirely.

`ss -ltnp` unchanged before and after; the legacy daemon (pid 13097) and
memdash (pid 2120) were never signalled, and every request this PR issues is a
`GET`. The MemGarden daemon on :9100 (pid 1786490) held its own database open
throughout and was not touched — which is the run §3 above exists to make
possible.

---

## What D3 inherited, and what it did with it

**Shipped as `docs/design/mg-2-verification.md`.** Three of the items below
turned out differently from the handoff, and each is written up there:
entities became a Tier-1 equality (as predicted), the temporal band held at
[1.45, 1.75] fact-to-fact (1.608 observed), semantic got **no** band at all,
and the temporal self-consistency check needed a scope that took three attempts
— the manual verification found the first two, both of which failed on a
*correctly working* daemon.

## What D3 inherited

* `import::BankReport` carries legacy's `documents` / `nodes` / `caused_by`
  beside ours, and `BankReport::reconciles()` is the comparison — the binary
  exits non-zero on a mismatch, so `verify`'s Tier-1 table has a shape to match
  rather than invent.
* `banks.disposition.mg_import` is `{state, at, snapshot}` where `snapshot` is
  the sha256 of the snapshot's `SHA256SUMS`. Tier 1 gates on `state == "done"`
  and on that hash matching the snapshot being verified.
* **The temporal band `[1.45, 1.75]` stands — over fact-to-fact edges only.**
  §4. Ours is 70,212 against legacy's 43,657, **1.61×**. The 34,804
  observation-to-observation edges are a new class with no legacy counterpart
  and must be reported unbanded, not folded into the ratio; a band on the total
  would pass a run in which the fact-edge rule broke.
* **Do not set a semantic band yet.** §4b: the first-run value is 6,890 against
  65,127, and the cause is a CE-7 defect (`embed_task.rs:178-179` confines every
  semantic edge to one 8-node embedding batch), not a property of the
  migration. Over the same vectors a whole-corpus pass would emit 68,537. The
  number will move by ~10× the day CE-7 lands, so a band derived from it now
  would be a band on a bug.
  **CE-7 landed 2026-08-09 and it moved by 9.0×** — 6,918 → 62,199, 0.11× →
  0.96×. The band still stays off, now for the reason that outlives the defect:
  ours is not legacy's embedding space, so no ratio is the one to expect.
* **`semantic edges FROM an observation = 0` is parity, not a divergence.**
  Legacy stores none either (4,603 == 4,603, §4). Assert the post-condition;
  do not file a `parity-gaps.md` row for it.
* **Entities can be a Tier-1 equality**, which the plan did not expect: 3,917
  distinct normalized archive names == 3,917 rows, 10,379 mentions == 10,379
  `node_entities` edges. §1.
* `proof_count` disagrees with legacy in 43 of `real-dup/`'s 65 observations,
  the same construction that produces 93 of 1,747 across the corpus. Tier 2.
* The database now holds **both** sides of the `node_sources` arithmetic:
  `metadata.legacy.observation_of` carries the archive's 2,200 raw references,
  `node_sources` holds the 2,114 distinct ones. The Tier-1 gate can be computed
  without the archive.
* `--defer-embeddings` leaves `embedding IS NULL` rows behind on purpose, so
  the embedding-coverage gate must run **after** the daemon has drained the
  backlog, not immediately after an import that used the flag.
* **Done** — `docs/parity-gaps.md` gained a Phase D section with eleven rows,
  including the two this PR created:
  **document `tags`** (dropped — `document_tags` has no reader or writer
  anywhere, and every document tag also appears on at least one of its facts,
  25/25) and **`retain_params.context`** (dropped — `"claude-code"` in 25/25,
  and our own retain records no equivalent).
* One runbook caveat: step 3a is `mg-migrate verify --dump-only`, which does
  not exist until D3, while `--replace` already deletes `sessions`. Rehearsals
  are unaffected — they use `--db <scratch>` — but the cutover ordering depends
  on D3 shipping that flag.
