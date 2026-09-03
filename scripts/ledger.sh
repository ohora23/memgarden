#!/usr/bin/env bash
# Read the task ledger, and flag the two ways it is known to go wrong.
#
# The read path is deliberately unbuilt (migration 0012), so these rows exist
# to be judged by a person before any of them reach a prompt. This script is
# the reading half of that; `docs/evidence/task-ledger-observation.md` is the
# judging half and says what to look for.
#
# `sqlite3` is a separate package MemGarden does not depend on and is not
# installed on the author's machine, so this uses the Python that is. Opening
# read-only keeps it out of the daemon's way.
set -euo pipefail
DB="${MEMGARDEN_DB:-$HOME/.local/share/memgarden/memgarden.db}"

python3 - "$DB" <<'EOF'
import sqlite3, sys, datetime as dt, json

db = sys.argv[1]
c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
c.row_factory = sqlite3.Row
rows = list(c.execute("SELECT * FROM task_ledger ORDER BY updated_at DESC"))

if not rows:
    print("task_ledger is empty. A retain job has to finish and find a goal first;")
    print("`select status, count(*) from retain_jobs group by 1` says whether any has.")
    raise SystemExit(0)

# The two failure shapes worth catching automatically. Everything else about
# whether a ledger is any good needs a person, which is the point of the stage.
collapsed = empty = 0

for r in rows:
    when = dt.datetime.fromtimestamp(r["updated_at"] / 1000).strftime("%m-%d %H:%M")
    print(f"\n=== {r['bank_id']}  ({when})")
    for f in ("goal", "open", "next_action"):
        v = (r[f] or "").strip()
        print(f"  {f:12} {v if v else '(empty)'}")
    try:
        a = json.loads(r["anchors"])
        print(f"  {'anchors':12} cwd={a.get('cwd')}  paths={len(a.get('paths') or [])}")
    except (ValueError, TypeError):
        print(f"  {'anchors':12} UNPARSEABLE: {r['anchors'][:60]!r}")

    # `open` and `next_action` answer different questions — "what is
    # outstanding" and "what do I do now". The first live row had them
    # byte-identical, which halves the ledger's value if it repeats.
    if (r["open"] or "").strip() and (r["open"] or "").strip() == (r["next_action"] or "").strip():
        print("  ^^ FLAG: open == next_action (the two fields collapsed)")
        collapsed += 1
    if not (r["open"] or "").strip() and not (r["next_action"] or "").strip():
        print("  ^^ FLAG: both open and next_action are empty (a goal with nothing to do)")
        empty += 1

print(f"\n{len(rows)} row(s) · collapsed open/next_action: {collapsed} · hollow: {empty}")
if collapsed:
    print("A repeating collapse is a prompt defect, not a data defect — fix the")
    print("prompt in crates/memgardend/src/retain/ledger.rs or merge the fields.")

# How stale each row was the moment it was written (Q11). The transcript is
# captured when the job is POSTed and the ledger is written when the job
# finishes, so a row is born exactly one job-duration behind. The first live
# row was 107 minutes behind, by which time its `goal` was already finished.
def job_of(row, cols):
    """The `retain_jobs` row that wrote this ledger row, or None.

    Tolerant on purpose. This is a reader run casually over days against
    whatever database is at hand, including fixtures and older copies, and a
    missing column must degrade one line rather than kill the report — the
    same posture the unparseable-`anchors` branch above takes.
    """
    if not row["job_id"]:
        return None
    try:
        return c.execute(
            f"SELECT {cols} FROM retain_jobs WHERE job_id = ?", (row["job_id"],)
        ).fetchone()
    except sqlite3.Error:
        return None

lags = []
for r in rows:
    j = job_of(r, "created_at")
    if j:
        lags.append((r["updated_at"] - j["created_at"]) / 60000)
if lags:
    lags.sort()
    mid = lags[len(lags) // 2]
    print(f"born-stale by (min): min {lags[0]:.0f} · median {mid:.0f} · max {lags[-1]:.0f}")
    if mid > 10:
        print("A row this far behind can name a task that finished while it was")
        print("being written — and `anchors` does not move when a task merely ends.")

# Q14/Q16. The row keeps only its newest writer, so how much work that writer
# actually saw is the thing to watch: a 1-chunk job overwrote an 18-chunk one
# on 2026-09-03 and reduced a whole session to a stray `/config` exchange.
#
# `upsert` overwrites in place, so replaced rows leave no trace. This prints
# what CAN be recovered — the current writer's size, and whether the row has
# been rewritten at all — and says plainly what it cannot.
print()
for r in rows:
    j = job_of(r, "chunks_total, facts_written")
    rewritten = r["updated_at"] != r["created_at"]
    size = f"{j['chunks_total']} chunk(s), {j['facts_written']} facts" if j else "writer unknown"
    print(f"{r['bank_id']}: written by {size}" + (" · rewritten at least once" if rewritten else ""))
    if j and j["chunks_total"] <= 1:
        print("  ^^ FLAG: a single-chunk job owns this row. If a larger job ran for")
        print("     the same bank, its version is gone — replacement keeps no history.")
EOF
