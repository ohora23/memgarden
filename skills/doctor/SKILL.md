---
name: doctor
description: Check a MemGarden install — daemon up, hook binary and daemon built from the same commit, Ollama on the GPU, hooks wired. Use when memory feels off, after an update, or when the person asks whether MemGarden is healthy.
allowed-tools: Bash(memgarden hooks status) Bash(memgardend --version) Bash(curl -s http://127.0.0.1:9100/livez)
---

# MemGarden doctor

Run these, then read them for the person — do not paste raw output.

```bash
memgarden hooks status
memgardend --version
```

What to look for, in order:

1. **`daemon url … up`** — otherwise the daemon is down; it is a user
   service and no hook starts it: `systemctl --user start memgardend.service`.
2. **`build: <sha> (daemon and this binary)`** — a `MISMATCH` line means the
   hook binary and the daemon were installed separately; `memgarden
   self-update` (from a release) or `scripts/deploy.sh` (from a checkout)
   installs both. `predates the build field` means the daemon is older than
   v0.1.0.
3. **`ollama`** in the `/healthz` excerpt — `cpu-only` means the model is off
   the GPU (extraction is slow or failing; `nvidia-smi` first); `failing`
   means the last call failed after retries.
4. **The four hook entries** — `SessionStart`, `UserPromptSubmit`, `Stop`,
   `SessionEnd` wired to this binary, and `mode` (`shadow` injects nothing;
   `full` injects memories).

Say what is wrong and the one command that fixes it. If everything is fine,
say so in one line with the build.
