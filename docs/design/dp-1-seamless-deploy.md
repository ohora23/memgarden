# DP-1 — Seamless deploys: a restart the client cannot see and the data cannot feel

**Status** proposal, 2026-09-06 · **Prompted by** the #57 deploy, which was
`cargo install` + `systemctl --user restart` and verified by PID and uptime
because nothing else could say what was running.

## 0. What a restart costs today — measured, not assumed

Every number below was read off the live system during the #57 deploy or out
of the code on `master` at `0b1f436`.

| Question | Answer | Evidence |
|---|---|---|
| How long is the port dark? | Under one second. `Stopped` and `Started` share a log second; `listening` follows in ~0.3 s | `journalctl --user -u memgardend.service`, 00:05:52 |
| When is the embedder back? | 0.11 s after `listening` | same log, `embedding model ready` |
| Does a hook that lands in the gap lose memory? | No. `retain` never commits its cursor on a bare 202; `confirmed_offset` advances only when the job is `done` and `clean`. A refused connection leaves the delta where it was, and the next gated `Stop` — or the next session's detached `catchup` — posts it again | `crates/memgarden-cli/src/cmd/retain.rs`, `catchup.rs` module doc |
| What happens to the in-memory queue? | Lost, and closed out honestly: startup marks every `pending`/`running` row `failed` with reason `daemon restarted before the job finished`; the hook reads `failed`, keeps its cursor, re-posts | `retain_jobs::fail_stale`, `main.rs:37` |
| A job mid-chunk at SIGTERM? | The worker stops at the chunk boundary (`daemon shut down at chunk i/N`), commits what it has, withholds the content hash, so the re-post is not dismissed as a duplicate | `retain/mod.rs:311`, `clean` |
| A consolidation round mid-flight? | Its `running` ledger row keeps a `NULL` watermark; startup closes it out; the next round re-reads from the last committed watermark. Nothing is skipped | `main.rs:51`, `round.rs:1246` |
| Does the gap trip the hook's circuit breaker? | Only if three transport failures accumulate (`breaker_failures = 3`); one refused connection in a sub-second gap does not. A 60 s cooldown *would* follow a third | `config.rs:579-580`, `recall.rs:405` |
| Is the recall the person sees affected? | One prompt's recall may be empty. `connect_timeout_ms = 50`, so a refused connection costs 0 ms of wait and an empty injection | `config.rs:572` |
| Can the daemon tell you what it is? | No. `/healthz` reports `CARGO_PKG_VERSION`, `0.1.0` since day one | `routes/health.rs:69` |
| Can an older daemon open a newer DB? | It could, silently, until this note's PR: `migrate` skipped every entry `<= user_version` and returned `Ok`; a v12 binary on a v13 file started, then failed on the first statement that named a column v13 dropped. Now refused at `Db::open` with a named error | `migrate.rs`, `migrate_refuses_a_database_newer_than_the_binary` |
| Is a schema-changing deploy backed up? | Only by hand: `backup-pre-v11-*.db`, `backup-pre-v12-*.db`, `backup-pre-v13-*.db` exist because someone remembered | `~/.local/share/memgarden/` |
| Are the daemon and the hook binary installed together? | No. Two `cargo install` lines; #57 installed one of them | `README.md:79-80` |

The conclusion that shapes everything below: **the data layer already survives
a restart by design** — the cursor protocol, `fail_stale`, the NULL watermark
and `catchup` were built for a daemon that could die at any moment, and a
deploy is just a death with a scheduled resurrection. What a deploy is *not*
protected against is (a) the sub-second window where a hook is refused,
(b) a migration with no backup, (c) a rollback to a binary that does not know
the schema it is opening, and (d) two binaries drifting apart with nothing to
say so. Three of those four are not about downtime at all.

## 1. Concept

A deploy has three layers, and each has its own definition of "seamless":

| Layer | Seamless means | Who guarantees it |
|---|---|---|
| **Connection** | No client ever sees `ECONNREFUSED`; at worst it waits | the kernel, holding the listening socket across the process boundary |
| **Work** | No accepted work is lost; interrupted work is re-driven from durable state | the cursor protocol (already), `fail_stale` (already), the watermark (already) |
| **State** | The schema moves forward under a backup, never backward without one, and every process can say which version it is | the daemon at startup, the deploy script, `/healthz` |

The design principle is **one process, one writer, no coordination**. MemGarden
is a single binary over a single SQLite file with in-process background tasks
(the consolidation bank guard, the embed drain, the mental-model ticker, the
Ollama prober). Every zero-downtime pattern that overlaps two processes —
blue/green, `SO_REUSEPORT` handoff, hot swap behind a proxy — buys its
overlap by making those tasks run twice or by adding a leader election. For a
gap that is already under a second, that trade is wrong. The connection layer
is solved by *moving the socket out of the process*, not by adding a second
process.

## 2. Design

### D1 — systemd socket activation (connection layer)

systemd owns `127.0.0.1:9100`. The daemon inherits the listening socket as
file descriptor 3 and never binds it. During a restart the socket stays open,
SYNs complete, and connections queue in the kernel backlog until the new
process calls `accept`. From the hook's side: `connect()` succeeds
immediately, and the wait moves into the read, which is bounded by
`recall_timeout_ms` (400) for recall and `retain_timeout_ms` (5000) for
retain. The measured restart (~0.3 s to `listening`) fits inside both.

```ini
# ~/.config/systemd/user/memgardend.socket
[Socket]
ListenStream=127.0.0.1:9100
Backlog=128
NoDelay=true
[Install]
WantedBy=sockets.target

# ~/.config/systemd/user/memgardend.service  (delta)
[Unit]
Requires=memgardend.socket
After=memgardend.socket
[Service]
KillSignal=SIGTERM
TimeoutStopSec=30
```

Daemon side, in `main.rs`, replacing the one `TcpListener::bind` line:

```rust
// systemd socket activation: LISTEN_PID/LISTEN_FDS name fd 3 as the
// listening socket. Absent, bind as before — `cargo run` and the tests
// never see systemd.
let listener = match std::env::var("LISTEN_FDS").ok().and_then(|v| v.parse::<u32>().ok()) {
    Some(n) if n >= 1 && std::env::var("LISTEN_PID").ok() == Some(std::process::id().to_string()) => {
        // SAFETY: fd 3 is handed to us by systemd for exactly this purpose.
        let std_listener = unsafe { std::net::TcpListener::from_raw_fd(3) };
        std_listener.set_nonblocking(true)?;
        tokio::net::TcpListener::from_std(std_listener)?
    }
    _ => tokio::net::TcpListener::bind(&cfg.bind).await?,
};
```

No new crate. `listenfd` exists and does the same twelve lines with more
surface; the `LISTEN_PID` check is the part people forget, and it is what
stops a child process from stealing the socket.

`TimeoutStopSec=30` is the budget the existing graceful shutdown already
wants: `axum::serve … with_graceful_shutdown`, then join the retain worker,
which stops at the next chunk boundary. A chunk is one Ollama call, measured
in seconds on the GPU and in minutes on the CPU (`gpu-lost-since-0902`), so 30 s
finishes a chunk on the GPU and cuts it on the CPU — and the cut is the
already-recoverable path.

**What D1 does not do.** It does not make requests *in flight at SIGTERM*
survive; axum's graceful shutdown lets them finish (that is its job), and a
request that arrives after the old process stopped accepting waits in the
backlog for the new one. It does not cover a crash-loop: a daemon that dies
on startup leaves the backlog to fill to 128 and then refuse, which is the
same failure as today with a 128-connection delay in front of it —
`Restart=always` + `RestartSec=5` already handle the loop and the hook's
breaker handles the third failure.

### D2 — the deploy is a script, and the script is the runbook (state layer)

`scripts/deploy.sh`, run from a checkout at the commit being deployed. Every
step prints what it observed, because the #57 deploy's verification was "PID
changed and uptime is 3 s", which is not a verification of *what* is running.

1. **Build both binaries** — `cargo build --release --bin memgardend --bin memgarden`.
   Never one. The hook binary is the daemon's only client and they are
   versioned together (§4).
2. **Ask the new binary what schema it wants** — `memgardend --schema-version`
   (a new flag that prints `LATEST_VERSION` and exits; ten lines). Compare to
   `PRAGMA user_version` on the live file.
3. **If they differ, back up first** — `sqlite3 … "VACUUM INTO 'backup-pre-vN-<ts>.db'"`
   (or the daemon's own `--backup-to` flag, same statement, no sqlite3
   dependency). `VACUUM INTO` is a consistent snapshot under WAL without
   stopping the daemon. This replaces the hand-made `backup-pre-v11/12/13`
   files with the same files made every time.
4. **Install** — `cargo install --path … --bin` for both, or `install(1)` of
   the built artefacts; either way both, in one step.
5. **Restart** — `systemctl --user restart memgardend.service`. With D1 the
   socket stays up.
6. **Verify by identity, not by liveness** — poll `/healthz` until
   `status = HEALTHY` **and** `build = <sha>` matches `git rev-parse HEAD`
   of the checkout. `build` is a new field (§D3). Print the line.
7. **Verify the hook binary agrees** — `memgarden hooks doctor` (extended,
   §D3) prints the daemon's `build` next to its own; the script fails if they
   differ.
8. **Say what the restart cost** — count of `retain_jobs` the startup marked
   `failed` with the restart reason, and of consolidation runs closed out.
   Both already land in the log at WARN; the script greps them so the number
   is in the terminal the deploy was run from.

Steps 2–3 are the ones that turn a schema-changing deploy from "remember to
back up" into "cannot forget". Step 6 is the one that would have made #57's
verification a real one.

### D3 — a build identity the daemon and the hook can compare (version skew)

* `/healthz` gains `"build": "<git sha>[-dirty]"`, from a `build.rs` that
  shells out to `git rev-parse --short HEAD` and falls back to `"unknown"`
  when there is no `.git` (a `cargo install` from crates.io, a tarball).
  `version` stays `CARGO_PKG_VERSION` — that is the release number, and it
  is fine that it is `0.1.0` until there is a release.
* The hook binary sends `X-MemGarden-Client: <its own build>` on every
  request. The daemon logs a mismatch **once per client build** at WARN and
  serves the request anyway.
* `memgarden hooks doctor` prints both builds side by side and exits non-zero
  on mismatch. That is the check the deploy script runs, and the check a
  person runs when memory "feels off".

The compatibility rule that makes a mismatch survivable rather than merely
visible is in §4.

### D4 — an interrupted job is not a failed job (work layer, optional)

`fail_stale` and the SIGTERM abort both write `failed`. That is honest about
the outcome and wrong about the cause: `retain_jobs_failed` and the
`failed` count in `/healthz` go up on every deploy, and the ledger's
"which jobs lost chunks" question gets deploy noise in its answer. A fourth
terminal status, `interrupted`, with the same cursor semantics as `failed`
(the hook already treats anything not `done` as "keep the cursor"), keeps
the deploy out of the failure metrics. Ten lines and a migration that adds
nothing to the schema (`status` is text). Deferred until the count is
actually a nuisance; it is listed so nobody mistakes the `failed` rows a
deploy leaves for a regression.

## 3. Direction and philosophy of the code changes

**Add the socket, not a proxy.** The temptation is nginx/caddy in front of
two daemons. That is two more processes to version, and it moves the problem
(which daemon has the bank guard?) rather than removing it. Socket activation
is the operating system doing the one thing a proxy would be doing here.

**Make the existing recovery paths the deploy's recovery paths.** Nothing in
D1–D4 adds a recovery mechanism. The cursor protocol, `fail_stale`, the NULL
watermark and `catchup` were designed for a dying daemon and reviewed as
such; a deploy should exercise *those*, so that every deploy is also a test
of the paths a real crash will need. A separate "graceful deploy" mode that
drains queues and spools payloads would be a second recovery path with
1/100th the exercise, which is where the next silent failure would live
(`project-review-2026-09-05`: the paths that fail are the ones nothing
watches).

**Verify by identity.** Every check in D2 compares a value the new artefact
produced against a value the running system reports. "It is up" is not a
verification; "it is `0b1f436` and the hook is `0b1f436`" is. This is the
Premises box of the PR template applied to operations.

**The fast path stays fast.** The hook's `connect_timeout_ms = 50` and
`recall_timeout_ms = 400` are the budget for a *normal* prompt, measured at
~1 ms. D1 must not add a millisecond to it: inheriting an fd is cheaper than
binding one, and `NoDelay=true` on the socket unit keeps the loopback
round-trip where it is. Anything in this design that touched the per-prompt
path would need the hook budget re-measured (`scripts/hook-budget.sh`).

**No new dependency for what the stdlib does.** `from_raw_fd` + two
environment variables replace `listenfd`; `VACUUM INTO` replaces a backup
tool; `git rev-parse` in `build.rs` replaces `vergen`.

## 4. Version-to-version risk: what can go wrong as the number goes up

This is the section the question actually asked. Each row is a risk that
exists **today**, what a version bump does to it, and what in D1–D4 (or
elsewhere) covers it.

### 4.1 Schema

| Risk | Today | Covered by |
|---|---|---|
| Migration fails halfway | Each migration is its own `BEGIN IMMEDIATE`; a failure leaves `user_version` at the last good step and the daemon does not start. Safe, but the daemon is down until someone acts | D2 step 3's backup; `Restart=always` will crash-loop, which is loud |
| Migration succeeds and the code is wrong | v13 dropped a column; a bad migration is irreversible without a backup | D2 step 3 — the backup exists *because* the version changed, not because someone remembered |
| **Rollback to the previous binary after a migration** | **Silent.** `migrate` has no `user_version > LATEST_VERSION` guard. The old binary starts, passes `/healthz`, and fails on the first query that names something the new schema removed — possibly hours later, in a background task | **The guard, shipped with this note**: `migrate` refuses a file whose `user_version` is above `LATEST_VERSION`, naming both versions and the backup to restore. Rollback then means "restore the backup, then the old binary", which is the only rollback that was ever correct |
| Two processes race the migration | Serialised on the write lock; the second sees the committed version and skips | already handled (`migrate.rs:42-48`); D1 does not introduce a second process |
| Backup restore loses the writes made since | Inherent to backups. The cursor protocol re-posts whatever the hook still has locally; consolidation re-runs from the restored watermark | the recovery paths — state that lived only in the daemon is by design re-derivable |

### 4.2 Wire protocol between the hook binary and the daemon

| Risk | Today | Covered by |
|---|---|---|
| Daemon installed, hook not (or vice versa) | Happened in #57. Both ignore unknown JSON fields, so a new field is silently dropped by the old side — the `maxTokens` / `max_tokens` incident is this exact shape | D2 step 1 (always both), D3 (mismatch is visible), and the compatibility rule below |
| A field's meaning changes without its name changing | Undetectable by any schema check | Policy: **never**. Rename, and let the old name fall through as unknown. Same rule the archive format already follows with `deny_unknown_fields` |
| A new endpoint the old hook does not call | Harmless; the hook simply does not use the feature | nothing needed |
| An endpoint the new daemon removed and the old hook still calls | 404 → the hook's transport-failure path → breaker after three | D3 makes it visible; policy: keep a removed route answering 410 for one version |

**The compatibility rule**, stated once so it can be tested: *the daemon at
build N accepts every request the hook at build N−1 sends, and the hook at
build N tolerates every response the daemon at build N−1 returns.* Additive
fields on both sides, defaults for absence, no renames without a
deprecation version. The test is a fixture directory of recorded requests
per version that the route tests replay.

### 4.3 Configuration

| Risk | Today | Covered by |
|---|---|---|
| New binary, old `config.toml` with a key it no longer knows | `TomlConsolidation` is `deny_unknown_fields`; some sections are not. A removed key in a strict section **stops the daemon at startup** with a parse error | Policy: a removed key is *ignored with a WARN* for one version, then rejected. `deny_unknown_fields` stays for typos of live keys; removed keys go on an explicit allow-list |
| New key with a default that changes behaviour | The 2026-09 review found `config.example.toml` drifted from the live config (`profile.name`) | D2 prints the effective config diff (`effective_config`, already exposed) between old and new binary before restarting. Read it; the Premises memory says config read from a doc is not a check |
| `[hooks] mode` flips on upgrade | It cannot — it is read from the file, not the binary | nothing needed; recorded because it is the one-line rollback the cutover relies on |

### 4.4 Models and vectors

| Risk | Today | Covered by |
|---|---|---|
| Embedding model changes between versions | Every stored vector is in the old space; recall degrades silently across the whole bank | Out of scope for deploy mechanics; `ax-1-embedding-model-tag.md` tags vectors with the model so a mismatch is *detectable*. A deploy that changes the embedder must be a re-embed job, not a restart |
| Ollama model tag changes | Extraction quality changes — the 14B-vs-8B experiment is the measurement of exactly that | `docs/evidence/extraction-8b-result.md`; the deploy script prints the configured tag next to `ollama list` |
| Tokenizer version changes | Chunk boundaries move; a re-post would re-chunk differently and the dedup probe has to absorb it | already absorbed by the content-hash + dedup design; worth one line in the deploy output |

### 4.5 The deploy mechanism itself

| Risk | Covered by |
|---|---|
| Socket unit present, service unit lacks `Requires=` → daemon binds itself, `EADDRINUSE` against systemd's socket | D1 has the daemon prefer fd 3 whenever `LISTEN_FDS` is set, and the unit files ship together in `scripts/` |
| `LISTEN_PID` mismatch (a wrapper script `exec`s the daemon and the PID changes) | `exec` preserves the PID; a non-`exec` wrapper does not, and the daemon then falls through to `bind` and fails with `EADDRINUSE`, which is loud. Document: the unit's `ExecStart` is the binary, never a script |
| `TimeoutStopSec` expires mid-chunk | SIGKILL; the chunk's partial write is inside a transaction and rolls back; the job is `pending` in the DB and `fail_stale` closes it on restart. Same as a crash, which is the tested path |
| Backlog fills during a slow start (first-run model download) | `load_at_startup` is spawned *after* the bind (decision #1 in `main.rs`), so the port is accepting during the download; the backlog only sees the ~0.3 s |
| The deploy script is run from the wrong checkout | Step 6 compares the running `build` to the checkout's `HEAD`; the wrong checkout fails the check rather than passing it |

## 5. Anticipated difficulties, and what to do about them

**"The restart is already sub-second — is D1 worth a unit file?"** The value
is not the 300 ms; it is that the hook never sees `ECONNREFUSED`, so a
deploy can never contribute to the breaker's count of three, and a deploy
that happens during a burst of `Stop` hooks (several sessions, `async:
true`) does not produce a scatter of empty recalls. It also makes the
restart safe to script without thinking about timing, which is what D2 needs.

**Socket activation and `cargo run` / tests.** The fallback branch keeps
every non-systemd path identical. The one new test is a unit test that sets
`LISTEN_FDS=1` with the wrong `LISTEN_PID` and asserts the daemon binds
instead of stealing fd 3.

**The 30 s stop budget on a CPU-only day.** When the GPU is gone
(`gpu-lost-since-0902`), a chunk takes minutes and every deploy cuts one.
That is the recoverable path, but it means a deploy on a bad-GPU day
re-extracts a chunk it had nearly finished. Accept it; the alternative
(waiting for the chunk) makes `systemctl restart` hang for minutes and the
`/api/ps` prober already tells the operator the card is gone before they
deploy.

**`VACUUM INTO` on a 113 MB file.** Seconds, on the daemon's read
connection, without blocking writers under WAL. On a much larger file it
becomes the slowest step of the deploy; when that happens, move the backup to
the daemon's own scheduled task and have the deploy script only *check* that
a backup newer than the last migration exists.

**The rollback guard will refuse a rollback someone urgently wants.** That
is the point. The refusal message names the backup file the deploy script
made for this exact version and the two commands to restore it. A rollback
that skips the restore is the one that corrupts, and it should be harder
than the one that does not.

**`build.rs` and reproducibility.** A `-dirty` build is allowed to run and
is reported as such; the deploy script refuses to *install* one. That keeps
"what is running" answerable from `git log`.

**Two hook binaries on the machine.** `~/.cargo/bin/memgarden` is what the
hook entries in `settings.json` call; a stale build in `target/release` is
not. D3's client header makes any stray binary show up in the daemon log the
first time it calls.

## 6. What this does not try to do

* **Zero-downtime for a crash-looping new build.** That is what a staging
  daemon on another port is for, and the gold-corpus harness already runs
  one (`recall_bench` binds its own). A pre-deploy smoke test against a
  second daemon over a `VACUUM INTO` copy of the live DB is the natural
  extension of D2 and is not designed here.
* **Live schema changes without a restart.** SQLite DDL under a running
  daemon is possible and not worth it for a sub-second restart.
* **Automatic rollback.** A failed deploy leaves a loud daemon and a backup
  with the version in its name; the operator rolls back by hand with the two
  printed commands. Automating that is where the silent-failure class comes
  back in.

## 7. Order of work

1. ~~The `user_version > LATEST_VERSION` guard (§4.1).~~ Shipped with this
   note: the only item here that closes a silent-corruption path.
2. ~~D1 socket activation + unit files under `scripts/systemd/`.~~ #59; measured on
   the live machine: 219 `/livez` probes across a restart, 0 refused, 55 ms worst.
3. ~~D3 build identity: `build.rs`, `/healthz.build`, client header, `hooks status`.~~ #60.
4. ~~D2 `scripts/deploy.sh` using 1–3.~~ Shipped; the `doctor` extension became
   a line in `hooks status`.
5. D4 `interrupted`, when the `failed` count from deploys becomes a nuisance.

Each is a PR with a Premises box whose commands are the ones in §0.

## 8. Distribution: how an adopter gets a new release, and how Claude Code tells them

DP-1 §1–7 is the operator's deploy on the machine that builds the code. An
adopter who installed from the repo has no `deploy.sh` checkout and may have
no Rust toolchain. What they have is Claude Code, and the flow they already
know is the plugin one: something updates in the background, a line says so,
they approve, it is done.

### 8.1 What Claude Code provides (official docs, checked 2026-09-06)

* Hook output may carry `systemMessage` (shown to the person) and
  `additionalContext` (added to the model's context); `SessionStart` and
  `UserPromptSubmit` both support them; 10,000-character cap. The recall hook
  already uses both for the Ollama notice.
* A plugin has `plugin.json` → `version`; marketplaces auto-update installed
  plugins in the background after startup (random delay up to 10 min); the
  person sees `Run /reload-plugins`. A plugin ships `hooks/hooks.json`,
  `skills/` and commands together.
* Skills are invoked as `/plugin:skill`; `allowed-tools` can pre-approve
  specific commands, and everything else goes through the normal permission
  prompt.
* **Not provided:** a plugin install/update lifecycle hook, and any documented
  "tool announces → person approves → update runs" pattern. A plugin update
  changes files. Nothing replaces a daemon binary.

So the flow is composed from two halves: the **signal and approval** live in
Claude Code; the **delivery** is MemGarden's own.

### 8.2 The flow

```
release vX (tag + GitHub Release + per-target artefacts + sha256)
   │
   ├─ plugin `memgarden` version = X   ──(marketplace auto-update)──▶ plugin files at X
   │                                                                     │
   └─ (no plugin) SessionStart spawns a detached child, once per day,    │
      GET releases/latest → ~/.local/share/memgarden/update-check.json   │
                                                                          ▼
SessionStart hook (the `memgarden` binary): compares plugin/cache version with
the daemon's /healthz build (DP-1 D3) and its own
   │  newer available → systemMessage (once per session) + one line of additionalContext
   ▼
person runs /memgarden:update
   │  skill shows release notes + schema delta, then runs ONE command:
   ▼
memgarden self-update [--version vX]        ◀── the Bash permission prompt IS the approval
   │  download artefact for host target → verify sha256 → write temp → rename over
   │  both binaries (previous kept as .prev) → DP-1 D2 steps: schema check →
   │  VACUUM INTO backup if the version differs → restart (systemd) or print the
   │  restart command → poll /healthz until build == X
   ▼
prints what it did; next SessionStart is quiet
```

### 8.3 Decisions

* **The plugin carries the skills; the binary is the payload; the signal is
  the daily check.** Revised while building: the docs say a plugin's
  `hooks.json` and a `settings.json` hook with the same command **both run
  and nothing deduplicates them**, so a plugin that also wired the four
  events would double every hook for anyone who ran `hooks install`. The
  plugin therefore ships `skills/update` and `skills/doctor` only, and the
  signal comes from `hook update-check` — the detached child `session-start`
  spawns beside `catchup`, once a day, one request, one cache file. The
  recall hook reads that file (one `read`, no network) and says one line when
  the release's `published_at` is newer than the binary's own mtime — which
  works for a source build as well as a release build, where comparing tags
  would not. Plugin version and release tag are still the same number; the
  release workflow refuses a tag that does not match `plugin.json`.
* **The approval is the permission prompt.** `self-update` is deliberately
  *not* in the skill's `allowed-tools`. A pre-approved update is an
  auto-update, and a daemon that rewrites the person's memory store is not
  something to auto-update.
* **`self-update` embeds D2.** One code path for the operator's deploy and
  the adopter's update, so the backup-before-migration and the
  verify-by-build-id rules cannot be skipped by taking the other road.
* **Prebuilt artefacts, `cargo install --git --tag` as the fallback.** An
  update that needs a Rust toolchain does not feel like a plugin update.

### 8.4 Risks specific to this half

| Risk | Covered by |
|---|---|
| The check blocks a hook | Network only in the detached child; the hook reads a cache file with a 50 ms budget it already has |
| A tampered artefact | sha256 from the release is mandatory and a mismatch refuses the install; a signature (minisign / sigstore) is the follow-up |
| Plugin `hooks.json` and `memgarden hooks install` both wire the four events → every hook runs twice | `hooks install` detects the plugin and steps back; `hooks doctor` reports double wiring |
| Hook binary and daemon drift | `self-update` installs both, always; D3 compares them on every `doctor` |
| No systemd (macOS, hand-run daemon) | `self-update` stops after the install and prints the restart command; the next SessionStart sees the old build still running and says so |
| The signal nags | Once per session, once per day, `memgarden update --snooze <days>`; the notice names the schema delta so the person can judge, not just click |
| An adopter is several versions behind | Migrations are sequential and idempotent (`migrate.rs`); the backup is taken once, before the first; release notes are shown for the whole range |

### 8.5 Order of work (after §7)

6. ~~Tag `v0.1.0`, a release workflow that builds artefacts and sha256 for
   Linux x86_64 first, `memgarden self-update`.~~ Shipped: `.github/workflows/release.yml`
   (notes from `docs/releases/<tag>.md`, build id = tag), `memgarden self-update`
   with the schema check, `--backup-to` backup and restart included — no
   "half": the schema gate is what makes an update safe to approve.
7. ~~The plugin (`skills/update`, `skills/doctor`) and the notice.~~ Shipped;
   no `hooks.json` (see §8.3). The notice rides the recall hook's
   `systemMessage`, because `session-start` emits nothing by design.
8. ~~The detached releases-API check.~~ Shipped as `hook update-check`, for
   every install, plugin or not.
