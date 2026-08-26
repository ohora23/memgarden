# The command-log cleanup — 158 rows out of 7,330

Run 2026-08-27, after [the correction](command-log-pollution.md) established
that new pollution had already stopped on its own and the whole problem was a
historical residue being over-retrieved.

**Result: the command-log share of what recall injects fell from 22% to 0.9%.**

## What was removed

| | |
|---|---|
| candidates matched by the classifier | 178 |
| **kept** after reading all 178 | **20** |
| **deleted** | **158** |
| nodes before → after | 7,330 → **7,172** |

The keep/delete decision for every id is committed beside this file as
[`command-log-cleanup-plan.json`](command-log-cleanup-plan.json), so the call
is auditable rather than described.

## The classifier was not trusted to decide

Its precision was estimated at ~92% from a 12-item sample. **Reading all 178
put the real figure at 11.2%** — 20 false positives, nearly double the
estimate, and the misses were exactly the expensive kind:

* **777** — *"Hindsight recall 지연 830ms→20ms 해결. 원인 2개 실측으로 규명:
  (1) 쿼리 임베딩 0.38s = RTX 5080 VRAM 경합 … (2) 재랭킹 0.44s"*. The single
  most substantive latency finding in the bank, matched because it names the
  `curl` used to measure it.
* **1077 / 2674** — the decision that the hooks are Rust subcommands rather
  than Python, with the ~1–5 ms startup measurement behind it.
* **4638 / 4900 / 4922** — the reflection gate, described in the bank as "the
  most original idea in this codebase".
* **6670 / 7335** — *"the embedding model does not distinguish 'agentmemory
  문제와 해결' from 'agentmemory doctor를 실행했다'"* — a finding **about this
  very failure mode**, which the regex would have deleted.
* **309, 531, 5205, 5343, 6327, 6495 / 7152** — hook configuration, project
  isolation via `claude mcp add -s local`, the Discord parser's `!` prefix, the
  `setfacl` ACL repair and its gotcha, the `vec0` test-ordering defect, and the
  `exit=101` → `retain_jobs` lock finding.

A 12-item sample said 92% and the population said 89%. The difference between
those two numbers is six irreplaceable memories. **The sample was not wrong;
it was too small to see what it was about to cost**, and that is the argument
for reading all of them rather than trusting a rate.

Borderline calls went to **keep**, because deletion is the irreversible
direction: a stale test tally (2126), a thin latency note duplicating 777
(6632 / 7264), and an operational note about `hermes-qwen3` (5201).

## Two rows were deleted on a different ground

**8171** and **8221** are not command logs. Their text is *"the
`include_tool_calls` switch … is set to `true` by default for the `coding`
profile"* — this investigation's own wrong claim, faithfully retained by
MemGarden hours before it was disproved. They were removed because they are
**false**, not because they are noise, and that is recorded here rather than
folded into the 158.

## How it was done

Raw SQL cannot do this. The `memory_nodes_vec_ad` trigger deletes from
`vec_nodes`, a `vec0` virtual table that only a connection with the sqlite-vec
extension can touch; a `python3 -c` attempt failed with `no such module: vec0`
before writing anything. So deletion went through the store's own
`nodes::delete`, via `crates/memgardend/examples/delete_nodes.rs` — dry-run by
default, `--apply` to commit — with the daemon stopped for the duration.

A full backup was taken first:
`~/.local/share/memgarden/backup-before-cmdlog-cleanup-20260827-023937.db`,
87 MB, `integrity_check ok`, 7,330 nodes.

## Verification

| check | result |
|---|---|
| `nodes::delete` calls | 158 ok, **0 errors** |
| `PRAGMA integrity_check` | **ok** |
| `PRAGMA foreign_key_check` | **clean** |
| orphans across `links`, `node_entities`, `node_tags`, `node_sources` | **0** on all six columns |
| `memory_nodes` vs `memory_nodes_fts` | 7,172 = 7,172 |
| daemon after restart | **HEALTHY**, 7,172 nodes, 9 banks, schema v9, **unembedded 0** |
| links | 245,532 → 237,449 (cascade) |

The four cascade tables are declared `ON DELETE CASCADE` and the app's
`init_pragmas` sets `foreign_keys=ON` (`conn.rs:48`) — which matters, because
SQLite's default for that pragma is **off**, and deleting with it off would
have left every one of those orphan counts non-zero.

## The measured effect

Six prompts from the session that found the problem, replayed against the live
recall endpoint:

| | injected | command-log | share |
|---|---|---|---|
| before | 106 | 23 | **22%** |
| after | 109 | 1 | **0.9%** |

The one survivor is a hit on the question *"can command logs be stopped from
being stored at all?"* — a query for which a command log is the correct answer.

## Limits

1. **One classifier, one reader.** The 20 keeps are one person's judgement
   against a stated rule ("if the fact is useless once the command's output is
   known, it is the command"), not a blind panel.
2. **The before-figure is from one session's six recalls**, and the after
   figure replays the same six against a changed bank. It is a
   before/after on one query set, not a sample of queries in general.
3. **Recall of the deleted class was never measured** — whether any of the 158
   would ever have been usefully retrieved is unknown. The claim here is that
   they crowded the budget, not that they were individually harmful.
4. **Nothing prevents recurrence from the prose path.** The 08-21 catch-up made
   86 of these with tool calls already excluded, from the assistant narrating
   its own commands. That path is open and unmeasured.
