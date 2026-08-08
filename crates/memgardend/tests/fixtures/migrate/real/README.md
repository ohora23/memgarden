# `real/` — a redacted slice of the live legacy archive

A snapshot directory in exactly the shape `mg-migrate snapshot --out` writes
one, built from a read-only `GET
/v1/default/banks/claude-code::bank-a/document-transfer?include_observations=true`
against the legacy daemon on 2026-08-06.

It exists because the shapes that break a migration are shapes nobody invents:

* a tag list mixing `file:` tags with a **bare document uuid** — the same list
  is repeated on the document, on every fact, and on the observations;
* `causal_relations` pointing **both forward and backward** within one
  document — 2 each, where a generated fixture writes only forward edges;
* `occurred_start`/`occurred_end` null with `mentioned_at` set — 78 of the 86
  facts, which is what makes `event_date = occurred_start.or(mentioned_at)`
  (`writes.py:80` parity) fall through to `mentioned_at` in the common case;
* `consolidated_at` set on every fact and `consolidation_failed_at` on none.

## What was changed, and what was not

| field | treatment |
|---|---|
| `facts`, `observations` | the live records sliced to source indices **125..210** and re-indexed from 0 — the window that contains all four causal endpoints (130→147, 155→160, 198→166, 204→191). Every field is the live value **except `text`**, which is synthetic (see below) |
| `causal_relations.target_fact_index` | shifted by the same −125, so the slice stays internally resolvable |
| `observations` | only those whose sources all land inside the window (79 of 258); `fact_index` shifted likewise |
| `original_text` | **replaced.** The real value is a 180,516-character Claude Code transcript and does not belong in a repository. `stats.json`'s `content_hash` is `sha256` of the stand-in, so the identity the snapshot asserts still holds |
| `chunks` | two kept, text replaced — nothing reads chunk text (`0001_init.sql:18-27` has no column) |
| `tags`, `metadata.files_modified` | trimmed to 3 entries / 2 paths so the file stays ~180 KB; the operator's home directory → `/home/user`, including inside the encoded project slugs, which an earlier pass missed |
| `text` on every fact and observation | **synthetic.** These were the last verbatim memories in the fixture. Each is replaced by a deterministic stand-in that keeps what the fixture is measured on: texts stay **unique** (the importer dedups by content hash, so collapsing them would move the node count), and each keeps the **script** of the record it replaces, so FTS tokenisation still sees both Hangul and ASCII. Nothing in `src/migrate/` asserts fixture text |
| everything else | untouched, including the real `exported_at`, document uuid and session ids |

That last row **reverses** what this file used to argue. Committed fact text was
the house precedent, on the reasoning that a fixture is worth more the closer it
sits to the corpus it was cut from — `gold/corpus.jsonl` was 3.8 MB of the same
memories. Preparing this repository to be readable by others withdrew that
precedent: the corpus is gone (`gold/README.md`) and these records are stand-ins.

What the argument got right is kept. The fixture's value was never in the
sentences — it is in the *shapes* listed above, and every one of them survives a
text substitution because none of them is a property of text.

## What it deliberately does **not** carry

**Duplicate `(document_id, fact_index)` source pairs.** They are real — 86 of
them — but all 86 are in `claude-code::bank-b`; `bank-a`
has none. The shape lives in `../edge/` with that fact recorded next to it.

`SHA256SUMS` covers every file in this directory and is what
`snapshot::verify_sha256sums` is tested against. Regenerate it if you change
anything here.
