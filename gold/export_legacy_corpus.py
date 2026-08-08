#!/usr/bin/env python3
"""Snapshot the legacy hindsight bank's facts to `gold/corpus.jsonl` (AX-2).

Strictly read-only: two GETs against the live legacy daemon and nothing else.
That daemon holds the only copy of this corpus, so this script must never
issue a mutating method.

The snapshot — not the daemon — is the reproducibility anchor for
`gold/queries.jsonl`. Gold labels key on the legacy fact uuid, and
`recall_bench import` rebuilds the MemGarden corpus from this file, so the
labels survive the legacy daemon being retired. Export once and keep the file:
a re-fetch returns a *different* corpus (hooks write to that bank
continuously) and would silently invalidate every label keyed to the old one.

It is deliberately **not** committed — a corpus exported from a real bank is
your working history. `gold/README.md` has the rest.

Only the columns MemGarden actually stores are kept. Colours, chunk ids and
consolidation bookkeeping are legacy-internal and would just be noise.

Usage:  python3 gold/export_legacy_corpus.py BANK_ID > gold/corpus.jsonl
"""

import json
import sys
import urllib.parse
import urllib.request

BASE = "http://127.0.0.1:9077"

# Fetched in ONE request rather than paged. `memories/list` has no ORDER BY
# guarantee, so a paged read of a bank being written to concurrently both
# duplicates and drops rows across the page boundary — an earlier paged run of
# this script returned 2717 unique ids against a reported total of 2718. The
# headroom absorbs facts written between the probe and the fetch.
PAGE_HEADROOM = 1000

KEEP = (
    "id",
    "text",
    "context",
    "fact_type",
    "date",
    "mentioned_at",
    "occurred_start",
    "occurred_end",
    "entities",
    "proof_count",
    "tags",
)


def get(bank: str, limit: int, offset: int) -> dict:
    url = (
        f"{BASE}/v1/default/banks/{urllib.parse.quote(bank)}/memories/list"
        f"?limit={limit}&offset={offset}&state=valid"
    )
    with urllib.request.urlopen(url, timeout=180) as r:  # noqa: S310 - loopback GET
        return json.load(r)


def main() -> int:
    # Required rather than defaulted. The default used to be the bank this
    # project measured against, which is nobody else's bank — and a silent
    # default here exports the wrong corpus under the right filename, which
    # then invalidates every label keyed to it.
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    bank = sys.argv[1]
    total = get(bank, 1, 0)["total"]
    items = get(bank, total + PAGE_HEADROOM, 0)["items"]

    # `state=valid` is already in the query string; this is the belt. An
    # invalidated fact is not part of the corpus a user recalls against.
    items = [i for i in items if i.get("state") == "valid"]

    # Sorted by uuid and de-duplicated, so the file — and therefore its
    # checksum — is a function of the corpus contents alone.
    by_id = {i["id"]: i for i in items}
    for fact_id in sorted(by_id):
        row = {k: by_id[fact_id].get(k) for k in KEEP}
        row["tags"] = sorted(row["tags"] or [])
        print(json.dumps(row, ensure_ascii=False, sort_keys=True))

    print(f"exported {len(by_id)} facts from {bank} (reported total {total})", file=sys.stderr)
    if len(by_id) != total:
        print("WARNING: unique count != total; the bank was written mid-read", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
