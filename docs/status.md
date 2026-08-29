# Status — phases, cutover gates, and what is still open

Split out of the README on 2026-08-27. The README carries a two-line
summary and links here.

## Status

Work lands as PRD-tracked pull requests (template in `.github/`), each 3-way reviewed (functional / security / code) before merge.

| Phase | Scope | State |
|---|---|---|
| A — Foundation | workspace/CI, SQLite schema, REST skeleton, metrics plumbing (CE-1..3, MX-1) | ✅ merged |
| B — Core pipeline | embeddings CE-4 · Ollama extraction CE-5a · retain ingest CE-5b · hybrid recall CE-6 · entities/graph CE-7 · temporal CE-8 · consolidation CE-9 · reflect CE-10 · reranker CE-11, plus vector-space tagging AX-1 and the recall-quality harness AX-2 | ✅ merged |
| C — Hooks | session/turn state ✅ · CLI foundation + hook-latency harness ✅ · session-start ✅ · recall ✅ · transcript delta ✅ · retain ✅ · install & cutover switch ✅ | ✅ code-complete |
| D — Migration | read-only legacy snapshot MG-1a ✅ · archive → SQLite importer MG-1b ✅ · AC-3 verifier MG-2 ✅ | ✅ **code-complete** |
| E — UI & metrics | dashboard, graph API, WebGL viewer (pan/zoom/drag, live SSE), ledger views, the bank survey | ✅ merged |
| F — Cutover | quality-parity A/B + performance gates + lossless migration → legacy shutdown | ✅ **done 2026-08-21** |

**v1 is complete.** Every acceptance criterion in `PRD.md` is ticked. AC-7 — every
PR on the template, `cargo test` passing — was the last, signed 2026-08-26 in two
halves: the template clause holds for #14–#27 without exception, with #1–#13
recorded as predating its adoption rather than retroactively edited to pass; the
`cargo test` clause on 20 consecutive clean workspace runs. [The audit](evidence/ac-7.md).

**Where the cutover gates stand.** All three must pass before the old system is shut down:

| | requirement | state |
|---|---|---|
| **AC-1 quality** | recall quality ≥ legacy on a fixed query set, human-judged | ✅ **met — signed by the user 2026-08-20**. Judged **blind**: both systems frozen on the settings they ship, each query split into `A`/`B` by a per-query hash, three independent judges apiece. On the shipping configuration **13 better / 5 worse / 1 equivalent**, nothing unjudgeable; at `semantic_alpha = 0` it reads 12/4/4 — both satisfy `worse ≤ better`. The first run, in August, was scored by the author against a comparison that gave legacy half the token budget; [what was wrong with it and how it was redone](evidence/ac-1-blind-panel.md) |
| **AC-2 performance** | recall p50 ≤35ms / p95 ≤60ms, hook overhead <10ms, retain cap savings held | ✅ **met** — 7.1/7.8ms recall, **0.85ms of hook per turn**, −75…−87% savings |
| **AC-3 lossless migration** | node/link/document counts match across the legacy banks + 50-sample content diff | ✅ **met on the live database**, by the instrument rather than the importer — `mg_migrate verify` exits 0 against the cutover import of 2026-08-08: every Tier-1 equality green (28 documents, 5,311 nodes, 201 causal, 2,125 provenance edges, 3,945 entities), temporal self-consistency exact at 105,199 in both directions, and **no content difference in the 50-sample diff**. Report: [`docs/evidence/ac-3.json`](evidence/ac-3.json) |

### Migrating off the old system

Only if you have one. This phase reads a legacy hindsight daemon on `:9077` through its own
transfer-archive API; a fresh install skips it entirely and just lets the bank fill.

Three subcommands of one binary, and only the middle one writes a database row — `snapshot`
issues nothing but `GET` to the old daemon, and `verify` issues no `INSERT`, `UPDATE` or
`DELETE` at all:

```bash
mg_migrate snapshot --out migration/<date>/            # read-only GETs, ~2s, refuses on 15 identities
mg_migrate import   --snapshot <dir> --db <path>       # archive → SQLite, ~167s for the whole corpus
mg_migrate verify   --snapshot <dir> --db <path>       # the AC-3 report; exit 0 / 1 / 2
```

Five non-empty legacy banks — **5,311 nodes, 28 documents, 1,757 consolidated observations, 201
authored causal links** — carried through legacy's own supported transfer format, re-embedded here, with
temporal and semantic adjacency **rebuilt by our rules rather than copied**. Of the four link
types only the authored one (`caused_by`) can be equal, and the report says so rather than
averaging it away: a semantic edge is a function of an embedding space that is ours and not
legacy's, legacy's temporal neighbour query applies no 24-hour window where ours does, and
`entity` links exist in neither system's storage.

The snapshot is the migration source, not the live daemon — legacy is still being written, and
a re-fetch between importing and verifying would surface a fact written in between as a count
mismatch that is not a defect. Rehearsals cost zero downtime: `--db /tmp/…` builds a complete
migrated database beside the live one with both daemons untouched.

[Runbook](runbook-migration.md) · [design](design/mg-1-migration.md) ·
[verification](design/mg-2-verification.md)

**The SIGSEGV under test load was a kernel fault, and it is closed.** For weeks
`cargo test --workspace` intermittently died with a SIGSEGV inside SQLite under
concurrent load, and neither ASAN (24 workspace runs, zero reports) nor a
shrinking reproducer could pin it. On 2026-08-27 the cause was found in a place
nobody had looked: **`/var/crash/` held three kernel panic dumps**, written by
kdump at each of August's abrupt machine deaths — the "22-second boots" in the
logs were the crash kernel saving them.

| dump | CPU | task | fault |
|---|---|---|---|
| `202608200204` | **3** | `swapper/3` | page fault in `sched_ttwu_pending` |
| `202608210441` | **3** | `tokio-rt-worker` | `Oops: Bad pagetable` |
| `202608260115` | **3** | `migrate::import` | `irq_fpu_usable` WARN -> `scheduling while atomic` -> page fault in `futex_wait_setup` |

Three unrelated kernel paths, all on CPU 3. The 08-20 dump is decisive: the
faulting task is `swapper/3`, the idle task, so no userspace code was running
and none can be the cause. A kernel that schedules while atomic can corrupt
anything downstream, which is why the crash presented in the test process and
why ASAN found nothing — there was no userspace heap bug to find.

Every panic and every reproduction is on kernel `-29`. The machine now runs
`-30`, and at 22 h 43 m of uptime **20 consecutive workspace runs passed with
zero SIGSEGV and zero kernel warnings** (867 passed / 0 failed). AC-7 was signed
on that evidence, with the CPU-3 cause split out as its own experiment rather
than closed by assumption.

**That experiment has since run, and CPU 3 is not the cause.** Both arms on
`-30`, one variable:

| arm | cores | duration | new panics |
|---|---|---|---|
| treatment | `cpu3`/`cpu11` offline, 14 threads | 43 h 55 m | 0 |
| **control** | **all 16 threads** | **25 h 27 m** | **0** |

The suspect core ran 25 hours under normal load and nothing happened. **CPU 3 is
where the fault landed, not what caused it** — a kernel bug corrupting scheduler
or FPU state faults on whichever CPU holds the wreckage. The cores are back
online for good. `-29` is now the best-supported explanation (every panic on it,
none on `-30` across 69 h), though the arms are not equal exposure: the control
stopped at 25 h, short of the 31 h longest-observed interval, on the user's
judgement that the evidence sufficed.
[The audit, the dumps and both arms](evidence/ac-7.md).

**The cutover ran on 2026-08-21.** The legacy hooks are gone, `hindsight-api` and its dashboard are
stopped, and MemGarden runs under a systemd user unit as the only memory system wired to Claude Code
on this machine. 811 memories that existed only in legacy were migrated first — two banks the
original migration never covered — and `mg_migrate verify` passes on both.
[What the cutover took, including what it nearly got wrong](evidence/cutover.md). The Python-era
system's own repository carries [the final record](https://github.com/ohora23/memgarden-legacy),
which is where its reproduction steps and runbook still live.

AC-4's rendering benchmark [was taken on 2026-08-19](evidence/ac-4-render.md) and is met —
3,200 nodes and 57,890 edges at **p50 3.6ms / p95 5.3ms** under pan and zoom, a 3.2× margin on a
60fps budget. What has no margin at that size is the layout: d3-force is on the main thread at
13ms a tick.
