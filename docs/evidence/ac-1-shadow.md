# AC-1 — shadow evidence, collection state

Measured 2026-08-08. What the shadow run had produced, why it could not be
judged, and what was done about it.

## The instrument was running; the bank behind it was not filling

`hooks status` reported both systems wired, shadow mode, `memgardend` up. The
recall side was healthy — p50 25 ms, 33 records in `shadow-recall.jsonl`, 28 of
them returning 12–20 memories at ~950–1,020 tokens. The five thin records
(`returned` 1–2) were all `claude-code::memgarden`, a bank legacy holds only 21
nodes for.

So the sample count was never the problem. The bank was.

| | |
|---|---|
| bytes posted, all 6 sessions | 45,025,079 |
| nodes in the database | 312 |
| retain jobs | 4 done, 3 failed, 1 running, 2 pending |
| chunks | 58 of 137 done, 19 failed |

Two of five banks held **zero** nodes — `bank-c` (32.5 MB posted)
and `bank-d` (1.8 MB) — their jobs sitting `pending` behind a
`ollama.max_concurrent = 1` semaphore.

Three `failed` jobs carried `retain wall timeout after 7200s`, all three on the
same bank, same session, and the **same `offset_from 3459173`**. Each burned two
hours, committed partial facts, and left the cursor where it started. Two `done`
jobs carried `ollama call exceeded the 600s total deadline` with 3 and 2 chunks
lost. This is the `chunks_failed > 0` cursor gap
(`docs/design/c4b-hook-retain.md` §Known limits) meeting the GPU contention the
hooks runbook warns about, and the contention is not marginal: legacy holds
`qwen3-14b-nothink` on the same box, MemGarden's retain queues behind it
serially, and the backlog grew faster than it drained.

**`links_written` is not evidence of anything.** The metric is declared,
initialised and serialised, and incremented nowhere in the workspace — it
reports 0 whatever the graph does. The database held 3,043 links at the time it
read 0.

## What was done: seed the bank from the legacy archive (MG-1b)

Waiting for live retain to catch up was not viable at ~2 h/job with a growing
queue. Instead the bank was seeded through Phase D's importer, which is also
`docs/runbook-migration.md` steps 3.0–3.7 executed against the live database.

```
snapshot  6 banks, 5 with content — 5,311 nodes, 28 documents, 201 causal
rehearsal import 117.2s → verify PASS (exit 0), /tmp database, daemon untouched
live      dump-only → daemon stop → db backup → cursor reset → import --replace
          → daemon restart → verify
```

`bank-g` is empty in the archive, so the importer **skips** it
and its 22 live nodes survive `--replace`. The one real loss is
`claude-code::memgarden`, 33 nodes → 21: MemGarden's own retain had got further
on that bank than legacy ever did.

Backup before the import:
`~/.local/share/memgarden/backup-pre-seed-20260808-131816/` (database + hooks
state). Pre-import census preserved in `pre-cutover-state.json` per runbook
step 3.3 — `sessions` 6, `retain_jobs` 10, `memory_nodes` 312, `links` 3,043,
`metric_snapshots` 4,556.

**AC-3 now holds on the live database**, not a rehearsal — see `ac-3.json`.
Every Tier-1 equality green, temporal self-consistency exact at 105,199 in both
directions, no content difference in the 50-record diff, verdict `Pass`.

Recall against the seeded bank, same daemon, one probe: 335 candidates → 13
memories, 970 tokens, on-topic. Before the seed that bank held 257 nodes.

## What AC-1 still needs

The seed makes the *next* days of shadow recording meaningful; it does not
retroactively fix the 33 records already written against a 312-node bank.
Discard them as a baseline.

1. **Shadow leg — outstanding.** Accumulate fresh `shadow-recall.jsonl` records
   against the 5,333-node bank, then compare each against what legacy injected
   on the same prompt. Legacy's injections are recoverable from the transcripts
   (`<hindsight_memories>`); the shadow records carry `ts`, `session_id` and
   `bank_id` to join on. This is the leg that carries the "≥ legacy" judgement.
2. **Gold leg — run, and it is a regression check rather than a comparison.**
   `recall_bench` on a purpose-built 2,718-node database reproduces the ratified
   baseline (`gold/results.jsonl` line 8) **bit-identically** across all six
   aggregates — recall@10 0.3881002983944160, MRR 0.5221088435374149, nDCG@10
   0.3235625243282635, recall@5 0.2256905676023324, recall@1 0.0348595848595849,
   ceiling 0.8587764359823182, 14 of 20 queries scored. Nothing in Phase D moved
   retrieval. It runs on its own database, not the live one: `bench` refuses when
   the node count differs from the corpus line count.

   The gold harness scores MemGarden against graded labels. It holds **no legacy
   score**, so it cannot discharge "≥ legacy" on its own.

**One decision AC-1 needs before it can be judged**, carried over from
`docs/design/ax-2-recall-quality.md`: the gate cannot be evaluated on
conclusion-type questions against this corpus, because the answers live in the
user's curated store rather than in auto-captured material. Closing it needs
either a second corpus covering the curated store, or an explicit decision that
AC-1 scopes to auto-captured recall only. Conclusion questions are arguably the
highest-value class a memory system has, so this is not a rounding error.

Both legs are independent of retain throughput now. The throughput problem is
still real and still unfixed — it is what makes `mode = full` premature — but it
is no longer what blocks AC-1.
