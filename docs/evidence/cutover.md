# The cutover — 2026-08-21

The legacy system is down. MemGarden is the only memory system wired to Claude
Code on this machine.

This is the record of what was done, in the order it was done, and of the two
things found on the way that would have made it a silent failure.

## What the gates said

AC-1 signed 2026-08-20 (13 better / 5 worse / 1 equivalent, blind panel, on the
shipping configuration), AC-2 and AC-3 already met. All three, so the shutdown
was permitted.

## Two things found before anything was removed

**The hooks were installed in `shadow` mode, and shadow injects nothing.**
`[hooks] mode` defaults to `shadow` by design — "installing the switch must
not throw it" — and no `config.toml` had ever set it. Removing the legacy
hooks at that point would have left a machine with two memory systems wired
and neither one speaking: legacy gone, MemGarden silently returning nothing on
every prompt. The mode was switched, and the switch was verified by feeding
the hook a real payload and reading a `<memgarden_memories>` block back out of
its stdout — not by reading the config.

**The installed CLI was two weeks old**, from 2026-08-06, predating everything
in Phase E and every recall fix since. Reinstalled from this tree first.

## 811 memories existed only in legacy

Comparing banks before the shutdown, four matched exactly — the four AC-3
verified — and two did not:

| bank | legacy | MemGarden | |
|---|---|---|---|
| four migrated banks | 5,290 | 5,290 | equal |
| one live project | 1,280 | 533 | **747 short** |
| one small bank | 64 | 0 | **never migrated** |

Neither was ever in the migration's scope; MemGarden had built the 533 itself
from scratch while legacy's 1,280 reached eight days further back. Shutting
down over the top of that would have made 811 of the user's memories
unreachable through any running system.

Both were migrated first. The comparison that decided it:

* legacy's archive spans 2026-07-30…08-20, MemGarden's 08-07…08-20;
* **0** MemGarden nodes were newer than legacy's newest fact, so `--replace`
  on that bank swapped an extraction for a longer one rather than trading away
  a live edge.

## The legacy API had moved on

`mg_migrate snapshot` was built against a synchronous
`GET …/document-transfer`. The live daemon answers **410** — *"Synchronous
document export has been removed because it could take down the shared API on
large banks"* — and offers POST-export / poll / download instead.

Two changes, both narrow:

* `snapshot::archive` tries the direct GET and falls back to the async flow
  **only on 410**. Not switched outright: the archive bytes are identical, so
  the importer and `verify` are untouched, and a daemon old enough to serve
  the direct GET is exactly the one whose snapshot was ratified for AC-3. A
  tool that only spoke the new flow could not re-run the migration it is the
  record of.
* the manifest grew `knowledge_page_count`, and `deny_unknown_fields` refused
  it. **Modelled rather than waved through** — that strictness is the module's
  whole defence against a legacy release growing content we would silently not
  carry — and asserted zero at import beside `mental_model_count`,
  `directive_count` and `webhook_count`.

## The import, and the verifier

The snapshot froze all ten banks (integrity assertions green, `SHA256SUMS`
written and verified). It was then pruned to the two target banks by deleting
the other eight and **filtering `SHA256SUMS` to the surviving files** — the
hashes are still the ones the snapshot tool wrote, because a hash recomputed
by the operator is a hash the verifier trusts for no reason.

```
ok   <small bank>: docs 1 == 1 | nodes 64 == 64 | causal 3 == 3
ok   <live project>: docs 14 == 14 | nodes 1280 == 1280 | causal 13 == 13
wall 17.1s
```

`mg_migrate verify` on the two banks: **Pass, exit 0.** Every Tier-1 equality
green, no content difference in the 50-sample diff, Tier-2 adjacency inside
its bands. Report kept beside the snapshot as `verify.json`; acceptance hash
`1cad477b21ee…`.

Coverage after: **nodes reachable only in legacy — 0.** Legacy 6,634,
MemGarden 6,656.

### What `--replace` cost, which the runbook says to dump first and I did not

`--replace` deleted **4 `sessions` rows** for the live project bank —
per-session counters (`turns`, `retains`, `messages_sent`, `compactions`) used
for AC-2/AC-6 measurement. The importer warned, naming them, and the warning
was passed over.

Checked afterwards rather than assumed: `benefit_ledger` is **9 rows before
and 9 after**, `metric_snapshots` 10,588 → 10,589, and the retain cursors that
prevent re-ingestion live in the CLI's own state files (4 present, intact),
not in that table. So the loss is bounded to four rows of per-session
counters. It should not have happened, and it is recorded rather than tidied
away.

## Shutdown

* legacy hooks removed from `settings.json` — 4 entries; the 4 MemGarden
  entries and 13 unrelated ones untouched;
* `hindsight-api` (:9077) stopped;
* `memdash-web.service` (:9090) stopped and disabled — it monitors hindsight
  and agentmemory, not MemGarden, which has its own dashboard at
  `/ui/dashboard`;
* **legacy's postgres left running and its data untouched.** The shutdown is
  reversible; the archive is not something to prove a point with.

## The daemon had to stop dying with the session

`memgardend` was not installed and had no unit: it ran from a terminal and
died with it. That was survivable only while legacy was still wired. It is now
`~/.cargo/bin/memgardend` under `memgardend.service`, enabled.

Tested rather than declared: the process was killed, systemd brought it back
inside 8 seconds, `/healthz` answered 200, and the recall hook injected
through the restarted daemon.

## Restoring, if it comes to that

Everything needed is in one directory, `~/.local/share/memgarden/cutover-backup-<ts>/`:
`settings.json` with the legacy hooks, `config.toml` before the mode switch,
the hindsight profile, the previous CLI binary, and the database as it stood
before the two-bank import.

The one-line reversal is `[hooks] mode = "shadow"`, which stops MemGarden
injecting without touching anything else.
