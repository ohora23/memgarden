# Installation

> 🇰🇷 [한국어](ko/install.md)

MemGarden is two processes' worth of code in one repository: a long-lived
daemon (`memgardend`) and a hook binary (`memgarden`) that Claude Code spawns.
Neither needs root, a database server, or a network.

---

## Prerequisites

| | why | check |
|---|---|---|
| **Rust 1.95+** | the whole thing | `cargo --version` |
| **Ollama**, with a model pulled | fact extraction. Default `qwen3-14b-nothink` needs a 16 GB card (12.2 GB measured); a 12 GB card takes the Q5_K_M quant, an 8 GB card fits `qwen3:8b` (5.6 GB) but its extraction measured well short (`docs/evidence/extraction-8b-result.md`), so 12 GB is the practical minimum. The GPU is background-only — a spare card is fine. See the README, *What GPU it needs* | `curl -s localhost:11434/api/tags` |
| **~500 MB disk** | embedding model cache + the SQLite file | |
| **Linux** | the only tested platform. `File::lock()` is portable; nothing else is tested elsewhere | |

No Postgres, no vector database, no Docker. SQLite with `sqlite-vec` and FTS5
is compiled in, and so are the embedding and reranking models.

---

## 1. Build

```bash
git clone https://github.com/ohora23/memgarden
cd memgarden
cargo build --release --workspace
```

Two binaries land in `target/release/`:

- `memgardend` — the daemon
- `memgarden` — the hook binary and installer

The first build compiles ONNX Runtime and SQLite from source; expect several
minutes. Later builds are seconds.

Optional, and worth doing once:

```bash
cargo test --workspace          # ~700 tests, no network needed
./scripts/hook-budget.sh        # binary size, ldd set, dependency closure
```

---

## 2. Configure

```bash
mkdir -p ~/.config/memgarden
cp config.example.toml ~/.config/memgarden/config.toml
```

The example file is the documentation — every knob carries the reason for its
value, and several carry the measurement that chose it. The defaults are the
ones this system runs on; you can skip straight to step 3 and come back.

The three you are most likely to touch:

```toml
[ollama]
model = "qwen3-14b-nothink"     # whatever `ollama list` shows

[storage]
db_path = "~/.local/share/memgarden/memgarden.db"

[hooks]
mode = "shadow"                 # shadow | full — see Usage
```

Every value has a `MEMGARDEN_*` environment override; `MEMGARDEN_CONFIG` points
at a different config file entirely.

---

## 3. Run the daemon

```bash
./target/release/memgardend
```

On first start it creates its data directory `0700`, runs the schema
migrations, downloads the embedding model into `<data>/models`, and listens on
`127.0.0.1:9100`.

```bash
curl -s localhost:9100/livez                 # ok
curl -s localhost:9100/healthz | jq .        # HEALTHY | DEGRADED | UNHEALTHY
curl -s localhost:9100/metrics.json | jq .   # counters, histograms, ledger
```

**Nothing else starts this process.** The hooks deliberately never spawn,
restart or stop the daemon — a hook that starts a model-loading service is the
restart race this rebuild exists to remove. Run it under systemd, a terminal
tab, or whatever supervises your other user services.

<details><summary>A systemd user unit, if you want one</summary>

Two units, in `scripts/systemd/`: the **socket** unit owns `127.0.0.1:9100`
and hands it to the daemon, so a restart (a deploy, a crash) never refuses a
hook — connections wait in the kernel backlog for the new process. The daemon
detects the handed-over socket (`LISTEN_FDS`/`LISTEN_PID`) and binds itself
only when there is none.

```bash
cp scripts/systemd/memgardend.{socket,service} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now memgardend.socket memgardend.service
```

The service's `ExecStart` must be the binary itself, not a wrapper script:
systemd names the process the socket is for by PID.

**Upgrading** is one script from a clean checkout at the commit you want:

```bash
scripts/deploy.sh
```

It builds both binaries, compares the schema this build wants with the
database's `PRAGMA user_version` and takes a `VACUUM INTO` backup first if
they differ, installs by rename, restarts the service (the socket unit keeps
the port open), and then refuses to finish until `/healthz` reports the commit
it just built and `memgarden hooks status` says the hook binary matches.

Without a checkout, the same steps from a GitHub release:

```bash
memgarden self-update              # latest release for this machine's target
memgarden self-update --version v0.1.0
```

It downloads the release asset, checks its sha256, refuses a build older than
your database, backs up before a schema change, installs both binaries beside
the running ones (previous kept as `.prev`), restarts the service, and waits
for `/healthz` to report the new build.

**Being told.** Once a day, a detached child of the session-start hook asks
GitHub for the latest release and caches the answer; when a release newer
than your install exists, the recall hook shows one line — once a day, not
every prompt — saying so. `memgarden self-update --snooze 7` defers it.

**From Claude Code.** The repository is also a plugin with two skills:

```
/plugin marketplace add ohora23/memgarden
/plugin install memgarden@memgarden
```

`/memgarden:update` runs the dry run, tells you what would change, and then
runs `memgarden self-update` — the permission prompt on that command is the
approval, on purpose. `/memgarden:doctor` reads `memgarden hooks status` for
you. The plugin carries **no hooks**: a plugin hook and a `settings.json` hook
both run and nothing deduplicates them, so the wiring stays with
`memgarden hooks install`.
</details>

---

## 4. Wire the hooks

```bash
./target/release/memgarden hooks install --dry-run   # look first
./target/release/memgarden hooks install             # shadow mode
./target/release/memgarden hooks status
```

`install` edits `~/.claude/settings.json` by **splicing in one line per
event** — it never rewrites the file. Four entries are added:

| event | subcommand | timeout | async |
|---|---|---|---|
| `SessionStart` | `hook session-start` | 5 s | — |
| `UserPromptSubmit` | `hook recall` | 10 s | — |
| `Stop` | `hook retain` | 30 s | ✔ |
| `SessionEnd` | `hook session-end` | 5 s | — |

Three things to know before you run it:

- **It takes effect immediately** in every running Claude Code instance. The
  settings file watcher picks the edit up mid-session; there is no restart.
- **It injects nothing.** The default mode is `shadow`: the hooks run, the
  daemon is called, the bank fills, and the model sees none of it. Turning
  injection on is a separate, explicit edit to `config.toml`.
- **Every write takes a timestamped backup first** and prints the path.

Put the binary somewhere stable before installing — the absolute path is what
goes into `settings.json`. `cargo install --path crates/memgarden-cli` puts it
in `~/.cargo/bin`; if you move it later, re-run `install`.

---

## 5. Verify

Open a Claude Code session and type anything. You should see the
`memgarden: recalling` status message, and then:

```bash
memgarden hooks status                                # everything, in one screen
ls ~/.local/share/memgarden/hooks/                    # one state file per session
tail -1 ~/.local/share/memgarden/hooks/shadow-recall.jsonl   # what it would have injected
curl -s localhost:9100/metrics.json | jq .recall_latency
```

If the bank is filling and `shadow-recall.jsonl` is growing, the installation
is complete. [Usage](usage.md) covers what to do with it.

---

## Uninstalling

In increasing order of effort — stop at the first one that is enough:

```bash
export MEMGARDEN_HOOKS_DISABLE=1                    # instant, per shell
# [hooks] enabled = false in config.toml            # instant, everywhere
memgarden hooks uninstall --dry-run                 # see what would go
memgarden hooks uninstall                           # remove the four lines
```

`uninstall` removes only the lines it inserted, so `settings.json` returns to
its pre-install bytes. Your data survives all four: the SQLite file and every
session cursor stay on disk, and re-installing resumes where it left off.

---

## Coexisting with the legacy hindsight hooks

Supported, and it is how the cutover evidence gets collected. Install in
`shadow` mode and leave both wired: legacy keeps driving the conversation while
MemGarden fills its own bank and logs what it *would* have injected.

`--mode full` **refuses** while legacy is wired, because legacy's tag-stripping
does not know `<memgarden_memories>` and would ingest our injections into its
own bank. Pass `--allow-double-injection` only if you mean it.

See [`docs/runbook-hooks.md`](https://github.com/ohora23/memgarden/blob/master/docs/runbook-hooks.md)
for the full coexistence and rollback procedure.
