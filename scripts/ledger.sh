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
    for f in ("goal", "done", "open", "next_action"):
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
    if not (r["done"] or "").strip() and not (r["open"] or "").strip():
        print("  ^^ FLAG: both done and open are empty (goal with no substance)")
        empty += 1

print(f"\n{len(rows)} row(s) · collapsed open/next_action: {collapsed} · hollow: {empty}")
if collapsed:
    print("A repeating collapse is a prompt defect, not a data defect — fix the")
    print("prompt in crates/memgardend/src/retain/ledger.rs or merge the fields.")
EOF
