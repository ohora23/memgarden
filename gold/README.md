# `gold/` — the AX-2 recall-quality harness

`recall_bench` turns a corpus and a set of graded queries into recall@k, MRR and
nDCG@10, so a ranking change reports a delta instead of a feeling. The design and
the measurement conventions are in
[`docs/design/ax-2-recall-quality.md`](../docs/design/ax-2-recall-quality.md).

## The corpus and the queries are not in this repository

`corpus.jsonl` was a snapshot of a real personal memory bank, and
`queries.jsonl` was twenty real questions with 331 graded labels whose rationales
quote the facts they grade. Both carried private content — working history, file
paths, identities — and neither is committed.

What is committed is everything needed to build your own:

| file | what it is |
|---|---|
| `export_legacy_corpus.py` | read-only snapshot of a legacy hindsight bank → `corpus.jsonl` |
| `queries.example.jsonl` | the label-file schema, with two synthetic entries |
| `results.jsonl` | the append-only results ledger. Metrics, query ids and result uuids only — no text |

## Building your own

```bash
python3 gold/export_legacy_corpus.py 'your::bank-id' > gold/corpus.jsonl
sha256sum gold/corpus.jsonl > gold/corpus.sha256          # your audit anchor

cargo build --release --bin recall_bench
./target/release/recall_bench import gold/corpus.jsonl /tmp/gold.db
./target/release/recall_bench bench  /tmp/gold.db gold/queries.jsonl gold/corpus.jsonl
```

`bench` refuses to run when the database's node count differs from the corpus's
line count — "benched the wrong database" otherwise looks exactly like a quality
regression. That also means the harness runs against its own purpose-built
database, never against a live bank.

Writing the queries is the part that cannot be automated. Follow
`queries.example.jsonl`: five strata, grades 2 (answers the query) / 1 (useful
context) / 0 (examined and rejected), and **every label carries a rationale** —
`read_gold` rejects the file if any `why` is empty. A query with no answer in
your corpus should be left unlabelled rather than graded; the harness excludes it
and reports it as unanswered, which is honest, where grading the top ten would
invent relevance.

Both files are gitignored. Keep them out of a public fork — a corpus exported
from a real bank is your working history.

## About the committed numbers

`results.jsonl` records measurements taken against the private corpus described
above (2,718 facts, 20 queries, 14 scored). **They are not externally
reproducible**, and they are not a benchmark to compare your own run against —
recall@k depends entirely on the corpus and the labels. They are kept as this
project's own before/after record: line 8 is the ratified baseline that later
ranking changes were measured against, and the ledger is append-only so the
earlier generations stay auditable.
