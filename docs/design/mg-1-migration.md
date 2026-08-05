# MG-1 — migrating four legacy banks

`crates/memgardend/src/migrate/` and `src/bin/mg_migrate.rs`. First of three
Phase D PRs. **D1 writes no database row anywhere**: it reads legacy over
HTTP, writes files, and refuses. D2 adds `import`, D3 adds `verify`; this note
grows with them.

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
| a bank on `DROPPED_BANKS` is no longer empty | 0/0 in all four | "nothing to lose" is only true while it stays true, and two of the four are live directories |
| a bank id that slugs to `""`, `.` or `..` | 8/8 fine | all three are made of characters `slug()` passes through, and `out.join("..")` is the snapshot directory's **parent** — which is where `unzip` would then extract |

`DROPPED_BANKS` is a **named constant, not derived from emptiness**. Deriving
the drop set from "is it empty right now" makes the emptiness assertion
circular and unable to fire. A bank that appears later and is not on the list
gets snapshotted whether or not it has content.

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
drop claude-code::user: empty, not migrated
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

### Left for D2, deliberately

* Two assertions D1 does **not** make, both currently 0 across the corpus: a
  non-null `observation_scopes` on a *fact* (D1 checks observations only,
  matching the plan's list), and a non-zero `manifest.mental_model_count` /
  `directive_count` / `webhook_count` (the plan puts mental models out of scope
  with "there is nothing to migrate", which is a claim worth asserting rather
  than assuming).
* **`run()` has no automated coverage.** Everything it composes is unit-tested
  — `assert_integrity`, `assert_every_bank_loaded`, `assert_slugs_usable`,
  `assert_dropped_bank_empty`, the checksum pair — but the wiring between them
  is covered only by the manual run. An `axum` stub plus an overridable
  `LEGACY_BASE` is the shape, and it is what would have made the one-direction
  reconciliation a red test instead of a review finding.
* `manifest.archive_type` and `includes_history` are parsed and asserted
  against nothing. A `"bank"` archive carries mental models, directives and
  webhooks in files this loader does not read.
* `collect_files` skips any file named `SHA256SUMS` at any depth, not only the
  one at the root. Harmless today (legacy emits no such entry) and wrong in
  principle.
