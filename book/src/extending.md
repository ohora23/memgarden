# Extending it

> 🇰🇷 [한국어](ko/extending.md)

Where the seams are, what they cost to open, and the two rules that keep the
system honest while you do.

---

## Start with config

Most "can it do X" questions are already a TOML key. `config.example.toml` is
the reference and every entry carries the reason for its value — several carry
the measurement that chose it.

| you want | knob |
|---|---|
| a different extraction model | `[ollama] model` — any `ollama list` entry |
| the reranker on | `[reranker] enabled = true` (off by default, see below) |
| retain more or less often | `[hooks] retain_every_n_turns` |
| a bigger recall budget | `[hooks] max_inject_bytes`, and the request's own `limit` |
| a per-directory bank | `[hooks] directory_bank_map` |
| consolidation to run harder | `[consolidation] batch_size`, the schedule keys |
| everything off, now | `MEMGARDEN_HOOKS_DISABLE=1` |

Every key has a `MEMGARDEN_*` environment override, which is the right way to
try something for one session without editing a shared file.

---

## The three places code goes

### A new REST endpoint

`crates/memgardend/src/routes/` — one module per resource, registered in
`routes/mod.rs::router`. The conventions are not optional:

- **every write goes through `Db::write`**, and no `rusqlite` handle crosses an
  `await`;
- the response is `Json<T>` with a typed error, never a bare string;
- if it is expensive, it is `POST` and it takes a caller timeout, because
  synchronous LLM work behind a `GET` is how a dashboard hangs.

Both `check_host` and `stamp_token` apply to every route automatically — you
do not opt in, and you must not opt out.

### A new hook subcommand

`crates/memgarden-cli/src/cmd/`, plus one arm in `lib.rs::dispatch`. Read
[How it works](design.md#the-hook-binary) first, then honour these:

- **it cannot exit 2**, ever. No `clap`, no `?` out of `main`, no path that
  returns a non-zero code;
- **stdout is a protocol**: on `UserPromptSubmit` and `SessionStart` it is the
  model's context channel, so anything you print lands in the conversation.
  Everywhere else, print nothing;
- state goes in the per-session file under a lock, and if you take a lock
  another hook takes, **use `with_try_lock`** — see the lock lesson in
  [How it works](design.md#the-lock-lesson);
- the dependency closure is CI-enforced. `use memgarden_store::…` compiles
  fine and silently adds 1.5 MB of SQLite to a process that runs thousands of
  times per session.

Then measure it, paired, and put the numbers in the PR:

```bash
cargo build --release -p memgarden-cli --bins
./target/release/hook_bench --arm-a "hook yours" --stdin-a payload.json --n 300
```

### A new stage in the pipeline

`crates/memgardend/src/retain/` (ingest) or `recall/` (retrieval). The retain
pipeline is: caps → chunk → extract → facts + entities + links → embed. The
recall pipeline is: query analysis → FTS5 BM25 + `sqlite-vec` KNN → RRF fusion
→ optional rerank → token budget.

**If you touch ranking, run the gold harness.** A ranking change reports a
delta or it does not land:

```bash
# import the frozen corpus once, then measure against the graded queries
recall_bench import gold/corpus.jsonl <db-path>
recall_bench bench  <db-path> gold/queries.jsonl gold/corpus.jsonl results.jsonl
```

`bench` refuses to run if the database's node count differs from the corpus's
line count — "benched the wrong database" otherwise looks exactly like a
quality regression. `now_ms` is pinned, so the baseline does not drift daily.

This is not ceremony. The embedded reranker looked like a clear win until a
temporal bug was fixed, after which the same measurement showed it *losing*
recall@10 — which is why it ships off by default.

---

## Things deliberately left out, and what would let them in

`docs/parity-gaps.md` is the list, and every row carries a **re-entry
criterion** — the specific fact that would have to become true. A few worth
knowing about:

| gap | what would open it |
|---|---|
| reranker on by default | a caller whose budget absorbs +14 ms p50, **or** a rerank path that does not starve the ingest loop |
| cross-bank recall | a shared user-profile bank worth recalling alongside the project bank |
| multi-turn query composition | an AX-2 run showing multi-turn queries beat single-turn on the gold set |
| daemon lifecycle management | a start path that cannot race a migration |
| the reflect agentic tool loop | a model whose tool calling holds for 10 turns without recovery scaffolding |

The criteria are written to be falsifiable in both directions. Meeting one is a
reason to build the thing; not meeting it is a reason to close the discussion,
which is most of what the file is for.

---

## The two rules

### Everything lands as a PR, with evidence

The template (`.github/pull_request_template.md`) asks for the PRD item ID, the
test count, one manual check with its observed output, and a `Measured:` line.
A PR that changes a latency-sensitive path and reports no numbers is
incomplete — that is a stated rule, not a preference.

Each PR also ships a design note at `docs/design/<id>-<slug>.md` with a
`## Diverged from legacy` section. The notes are meant to stand alone without
the diff, because the diff is the thing nobody re-reads in six months.

### Measure paired, never across runs

Absolute cross-run comparison is invalid on this hardware — re-benchmarking an
identical commit has returned **+1.5 ms on identical bits**. So:

- hooks use `hook_bench`, which alternates `A,B,A,B…` in one driver process
  with `hook noop` as arm B, and reports `A_i − B_i`;
- daemon-side numbers come from `/metrics.json`'s exact `under_35ms` /
  `under_60ms` counts, **not** its interpolated percentiles;
- "the binary grew, so it got slower" is a hypothesis, not a result. Pair the
  two builds (`hook_bench --bin-b <old>`) and find out — that control has
  already caught one attribution that was right in mechanism and wrong by 5×.

---

## Testing conventions

- **Test names are sentences** describing what would break:
  `a_poisoned_at_in_the_future_does_not_throttle_a_session_out_of_existence`.
- **Two distinguishable values, never one.** A timeout test asserting
  `150ms <= elapsed < 600ms` passes on the hardcoded 400 ms it was written to
  catch.
- **The user's real files are never written by a test.** `~/.claude/settings.json`
  is off limits; every settings test passes `--settings <tempfile>` and
  redirects `HOME` on top of that.
- **Legacy is untouchable.** Never restart the hindsight daemon, never bind
  9077 or 9090. Test listeners bind port 0.
- **In-memory SQLite is shared-cache**, so two connections writing one table get
  `SQLITE_LOCKED`, which `busy_timeout` does not retry. Use a `tempfile` + real
  `Db::open` for concurrency tests.

---

## A caution about one-hook-at-a-time

The single sharpest bug of the hook phase was invisible to the entire test
suite because **every test ran one hook at a time**: a retain held a lock
across the network while recall took the same lock on every prompt, and the
mutation that reintroduced it survived the whole suite.

If your change makes two hooks share anything — a lock, a file, a socket — the
test that would catch the regression has to run **two processes**. That is the
default blind spot of this harness, and it is worth re-reading before you
assume green means safe.
