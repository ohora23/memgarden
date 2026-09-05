---
name: update
description: Install the latest MemGarden release after the person approves it. Use when the recall hook said a newer MemGarden is available, or when the person asks to update MemGarden.
disable-model-invocation: true
argument-hint: "[--version vX.Y.Z]"
---

# Update MemGarden

This is the adopter's deploy. The approval is the permission prompt on the
one command below — it is deliberately **not** pre-approved, because a
daemon that rewrites the person's memory store is not something to update
without them looking.

1. Show what is out there and what is installed, without changing anything:

   ```bash
   memgarden self-update --dry-run $ARGUMENTS
   ```

   The first line reads `release vX.Y.Z; this binary is <build>`. If it says
   `already up to date`, stop and say so.

2. Tell the person, in two or three lines: the release tag, the release page
   (`https://github.com/ohora23/memgarden/releases/tag/<tag>`), and that the
   update will back up the database first if the schema changes and restart
   the daemon (the socket unit keeps the port open; a restart loses no
   memory — the hooks re-post anything in flight).

3. Run the update. The permission prompt for this command is the approval:

   ```bash
   memgarden self-update $ARGUMENTS
   ```

   It verifies the sha256, refuses a release older than the database, backs
   up before a schema change, installs `memgardend` and `memgarden` beside
   the running ones (previous kept as `.prev`), restarts
   `memgardend.service`, and waits for `/healthz` to report the new build.

4. Relay the last two lines (`/healthz status=… build=…`, `updated to …`).
   If it failed, relay the `self-update:` line verbatim; the message names
   the fix (restore a backup, restart by hand, check the journal).

If the person would rather not update now:

```bash
memgarden self-update --snooze 7
```
