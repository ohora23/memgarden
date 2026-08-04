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
| **Ollama**, with a model pulled | fact extraction. Default `qwen3-14b-nothink` | `curl -s localhost:11434/api/tags` |
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

```ini
# ~/.config/systemd/user/memgardend.service
[Unit]
Description=MemGarden daemon
After=network.target

[Service]
ExecStart=%h/repositories/memgarden/target/release/memgardend
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now memgardend
```
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
