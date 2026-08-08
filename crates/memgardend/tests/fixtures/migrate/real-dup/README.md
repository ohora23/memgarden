# `real-dup/` — the second real bank, and the only one that can carry duplicate sources

A snapshot directory in exactly the shape `mg-migrate snapshot --out` writes
one, built from a read-only `GET
/v1/default/banks/claude-code::bank-b/document-transfer?include_observations=true`
against the legacy daemon on 2026-08-06.

`../real/` cannot do this fixture's job, and that is a measurement rather than
a preference. **All 86 duplicate `(document_id, fact_index)` observation source
pairs in the live corpus are in `claude-code::bank-b`** —
`bank-a`, the bank `../real/` is sliced from, has zero (294 raw source
references, 294 distinct). The duplicates are what make MG-2's `node_sources`
gate **2,114 and not 2,200**: `link_sources_tx` is `INSERT OR IGNORE` against
the `(observation_id, source_id)` primary key (`consolidate.rs:638-650`), so
they collapse on import and a gate on the raw count would fail every run.

Measured over the whole bank at snapshot time: **1,503 raw source references,
1,417 distinct, 86 duplicates** — every one of the corpus's duplicates, in one
bank.

## What this slice carries

| shape | this slice |
|---|---|
| duplicate `(document_id, fact_index)` source pairs | **46**, across 43 of the 65 observations |
| an observation with more than one *distinct* source | 1 |
| `proof_count` that disagrees with `len(distinct sources)` | **43 of 65** — legacy stores it with a `or len(source_ids)` fallback (`export.py:457`) where we always derive it from `node_sources` (`recount_proof_tx`, `consolidate.rs:658-666`). Tier 2, never asserted equal |
| a second bank id, so a snapshot can hold more than one | `claude-code::bank-b` |

It carries **no** `causal_relations`: the window has none, and `../real/` already
carries four pointing both forward and backward. Slicing a window that held both
shapes would have meant 330 facts instead of 70.

## What was changed, and what was not

| field | treatment |
|---|---|
| `facts` | document `874a3a6d-…`'s live records sliced to source indices **280..350** and re-indexed from 0. Every field is the live value **except `text`**, which is synthetic (see below) |
| `observations` | only those whose sources all land inside that window and inside that document — 65 of 1,177; `fact_index` shifted by −280 |
| `original_text` | **replaced.** The real value is a 219,049-character Claude Code transcript. `stats.json`'s `content_hash` is `sha256` of the stand-in, so the identity the snapshot asserts still holds |
| `chunks` | two of 89 kept, text replaced — nothing reads chunk text (`0001_init.sql:18-27` has no column) |
| `tags`, `metadata.files_modified` | trimmed to 3 entries + the document uuid / 2 paths, matching the live shape; the operator's home directory → `/home/user`, including inside the encoded project slugs, which an earlier pass missed |
| `text` on every fact and observation | **synthetic.** These were the last verbatim memories in the fixture. Each is replaced by a deterministic stand-in that keeps what this fixture is measured on: the duplicate `(document_id, fact_index)` pairs and `proof_count` disagreements live in `sources`, which is untouched, and texts stay **unique** so the importer's content-hash dedup still produces 135 nodes. Nothing in `src/migrate/` asserts fixture text |
| `banks.json` | the live mission and disposition verbatim, minus the timestamps |
| everything else | untouched, including the real `exported_at`, document uuid, session id, `consolidated_at` and `proof_count` |

`stats.json`'s `links_by_link_type` carries zeroes for `temporal`, `semantic`
and `entity` exactly as `../real/` does: those are legacy's own derived counts
over the *whole* bank and have no meaning for a 70-fact slice. The two numbers
the fixture's integrity assertions actually read — `caused_by` and
`total_nodes` — are consistent with the slice.

## Composing the two

`test_support::two_bank_snapshot()` copies `../real/` and this directory into
one temporary directory and rewrites `SHA256SUMS`, which is how the importer's
multi-bank driving is covered without committing a third fixture that is just
the other two side by side.

`SHA256SUMS` covers every file in this directory and is what
`snapshot::verify_sha256sums` is tested against. Regenerate it if you change
anything here.
