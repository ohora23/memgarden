# Runbook — migrating the legacy banks

Phase D's three commands, in the order they are run, with the steps that are
**not optional** marked as such. `mg_migrate` is a `memgardend` binary:

```bash
cargo build --release --bin mg_migrate
```

Two daemons are involved and only one of them is ever stopped. The legacy
daemon on **:9077** is read-only throughout — `mg-migrate` contains no code
path that issues anything but `GET` to it. The MemGarden daemon on **:9100**
is stopped for the cutover import and for nothing else.

---

## 1. Snapshot — read-only, ~2 s, both daemons untouched

```bash
mg_migrate snapshot --out migration/$(date +%Y-%m-%d)/
```

`--drop-bank <id>` is repeatable and defaults to none. Naming a bank says *this
one holds nothing and is not being migrated* — the run fails if it turns out to
hold something, and `verify` re-checks that claim from the frozen `stats.json`
rather than from a drop set you would have to re-type identically. If you have
no such claim to make, pass none: an unnamed empty bank is snapshotted anyway
and skipped at import for having an empty archive.

Writes, per bank: the transfer archive verbatim as `<bank-slug>.zip`, the same
archive unpacked beside it, `banks.json`, `stats.json` (the frozen `/stats`,
`/documents` and invalidated-fact census) and `SHA256SUMS`. It unpacks the ZIPs
itself — there is no manual `unzip` step, because every integrity assertion is
about bytes inside them.

It refuses, non-zero and naming the property, on any of fifteen identities that
are true of the live corpus today. The two to read on the way past are the
coverage identity (`facts + observations == /stats total_nodes`) and the causal
identity (`archive causal == /stats caused_by`).

Verify it independently:

```bash
cd migration/<date> && sha256sum -c SHA256SUMS
```

---

## 2. Rehearsal — zero downtime, both daemons up

```bash
mg_migrate import --snapshot migration/<date>/ --db /tmp/mg-rehearsal.db
mg_migrate verify --snapshot migration/<date>/ --db /tmp/mg-rehearsal.db \
                  --out /tmp/mg-report.json --sample 50 --seed 1
```

`--db` anywhere other than the daemon's own database is safe with the daemon
running: `import` refuses only when something is listening on the configured
port **and** `--db` resolves to the file that daemon holds. Read the report's
verdict, not just its exit code — `PASS`, `REVIEW` or `FAIL`.

Expect `REVIEW` (exit 2) if a Tier-2 ratio has moved outside its band. That is
a signal for a human, not a failure; §"the acknowledgement" below is how it is
discharged.

---

## 3. Cutover — the maintenance window

### 3.0 Re-snapshot. **Not optional.**

```bash
mg_migrate snapshot --out migration/<fresh>/ && \
  (cd migration/<fresh> && sha256sum -c SHA256SUMS)
```

The legacy banks are still being written. Between the rehearsal and the cutover
they grow, and importing a rehearsal-era archive loses exactly the facts
written in between. Measured during D1: **one new document and one new fact
inside an hour**, and a fifth bank appeared between D1 and D2.

### 3.1 Cut over with one session open

Every migrated bank's sessions restart at offset 0 and each does one initial
retain bounded by `retain.max_initial_messages`. Each of those costs an Ollama
extraction, so open one Claude Code session, not five.

### 3.2 Stop the daemon

```bash
systemctl --user stop memgardend   # or kill it
```

Hooks fail closed and lose nothing while it is down: `hook recall` gets
`ECONNREFUSED` and exits 0 in 0.286 ms with no injection, its circuit breaker
opens after three failures, and `hook retain` does not advance `byte_offset` on
a failed POST — the transcript is the durable spool, and C2b's catch-up replays
whatever was missed on restart.

### 3.3 Preserve the shadow run's measurements. **Not optional.**

```bash
mg_migrate verify --snapshot migration/<fresh>/ \
                  --db ~/.local/share/memgarden/memgarden.db \
                  --out docs/evidence/pre-cutover-state.json --dump-only
```

Step 3.4's `--replace` **deletes `sessions`**, which is AC-2/AC-6 measurement
data, and severs `retain_jobs.document_id` through `ON DELETE SET NULL`. This
is the only thing that preserves either. `--dump-only` performs no comparison,
so it works against a database that has not been migrated yet.

**It exits 2, on purpose.** A dump verifies nothing, so its report carries
`"mode": "dump"`, an empty `tier1` and a verdict of `REVIEW` — a run that
exited 0 with an all-green `tier1` would sit in `docs/evidence/` looking
exactly like a passed migration. Under `set -e`, expect to have to say
`|| true` here and nowhere else.

`purge` also prints the row count it is about to delete, so an operator who
skipped this step finds out while the terminal still says how much there was —
but the program cannot enforce a runbook step, which is why this line is here.

### 3.4 Reset the hook's client-side cursors. **Not optional.**

```bash
rm -f ~/.local/share/memgarden/hooks/*.json
```

`--replace` cannot do this: the retain cursor is a **file**, not a row.
`SessionState::offset` lives in `<state_dir>/<session_id>.json`
(`crates/memgarden-cli/src/state.rs:105-113`) and the daemon's
`confirmed_offset` is consulted only when `state::load` returns `None`. Without
this step the replaced content is never re-ingested, and `parity-gaps.md`'s
standing "every session starts at offset 0 after cutover" is false.

### 3.5 Import

```bash
mg_migrate import --snapshot migration/<fresh>/ \
                  --db ~/.local/share/memgarden/memgarden.db --replace
```

`--replace` purges each migrated bank's `memory_nodes` (cascading `links`,
`node_tags`, `node_entities`, `node_sources` and `vec_nodes`), `documents`,
`entities`, `mental_models`, `consolidation_runs` and `sessions`, in one
transaction, before writing. `retain_jobs` rows are **spared**: a job left
`Pending` whose row vanishes resolves to a 404, which `cmd/retain.rs:498-504`
reads as `Failed` and rolls the client cursor back — deleting them causes
re-ingestion, not cleanliness.

Measured on the four-bank corpus: **167 s** in a dev build, of which the
embedding is most. Add `--defer-embeddings` to leave the *fact* backlog for the
restarted daemon — observations are embedded either way, because
`consolidate::insert_observation` takes the vector by value.

A failed bank leaves rows. That is by design and it is never silent: the
`disposition.mg_import` marker stays at `running`, `import` refuses to touch
that bank again without `--replace`, and `verify` fails Tier 1 on it.

### 3.6 Start the daemon

```bash
systemctl --user start memgardend
```

It drains any remaining fact-embedding backlog, which is also what writes the
semantic links.

### 3.7 Verify, and read the verdict

```bash
mg_migrate verify --snapshot migration/<fresh>/ \
                  --db ~/.local/share/memgarden/memgarden.db \
                  --out docs/evidence/ac-3.json --sample 50 --seed 1
```

`verify` writes nothing and is safe with the daemon up. Exit **0** pass, **1** a
Tier-1 mismatch or a content difference, **2** a Tier-2 review stop, **3**
usage.

**Running it after the daemon has started is fine**, and that ordering is
deliberate: every Tier-1 equality is scoped to the nodes the import wrote
(`metadata.$.legacy`), so a retain that lands between 3.6 and 3.7 — and one
will, because 3.4 just reset the cursors — does not move a single expected
count. An earlier version was unscoped and turned the smallest possible retain
into `documents 1 != 2` and a `sentence` reading "AC-3 is NOT met".

If `--defer-embeddings` was used, run this **after** the daemon has drained —
until then `embedding coverage` is non-zero for a reason that is not a
migration defect.

The report's `sentence` field is what Phase F pastes into the cutover note.

---

## The acknowledgement

A `REVIEW` (exit 2) is discharged by a human deciding the recomputed adjacency
is acceptable, and recording that decision against **that specific result**:

```bash
mg_migrate verify … --accept-tier2 <acceptance hash>
```

The hash is printed by every run. It covers the snapshot and the Tier-2 counts
only, so it identifies *what is being accepted* and cannot be laundered: it
does not change when the verdict does, it does not accept a different result,
and it never downgrades a Tier-1 failure.

Do not put this in a script. A permanent `--accept-tier2` is the same thing as
no exit code at all.

---

## Two user-visible consequences, so nobody thinks it broke

1. **The empty banks are gone and reappear empty on first use.** `hook
   session-start` calls `POST /v1/banks` on every session
   (`session_start.rs:159-166`), so opening a shell in one of those directories
   recreates the bank with the default mission — empty, which is what it was.
2. **Every session re-retains from offset 0 once**, bounded by
   `retain.max_initial_messages`. That is step 3.4 working.

---

## What the numbers should look like

From the last rehearsal, four banks:

| | legacy | ours |
|---|---|---|
| documents | 25 | **25** |
| nodes | 5,288 | **5,288** |
| `caused_by` | 200 | **200** |
| `node_sources` | 2,114 distinct (2,200 raw) | **2,114** |
| entities / mentions | 3,917 / 10,379 | **3,917 / 10,379** |
| temporal, fact to fact | 43,657 | 70,212 — **1.61×**, in band |
| temporal, observation to observation | — | 34,804, no counterpart |
| semantic | 65,127 | 6,890 — a CE-7 defect, not a migration one |

`docs/design/mg-1-migration.md` and `docs/design/mg-2-verification.md` carry
the reasoning; `docs/parity-gaps.md` carries what was deliberately not ported.
