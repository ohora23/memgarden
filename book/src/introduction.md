# MemGarden

> 🇰🇷 [한국어](ko/introduction.md) · [Repository](https://github.com/ohora23/memgarden)

An AI coding assistant starts every session amnesiac. It re-reads the same
files, re-derives the same conclusions, and asks you things you settled last
week. MemGarden is the layer that stops that: conversations are captured
automatically, distilled into facts by a local LLM, linked to each other, and
served back — in single-digit milliseconds — as the handful of memories that
matter for the prompt you just typed.

It is a ground-up **Rust rebuild** of a personal long-term memory system that
previously ran as a Python daemon over embedded Postgres. Everything runs on
one machine, in two processes, against one file.

---

## This wiki

| page | what it answers |
|---|---|
| [Installation](install.md) | build it, run the daemon, wire the hooks, verify |
| [Usage](usage.md) | the two switches, day-to-day commands, what to check when something looks wrong |
| [How it works](design.md) | the decisions — why SQLite, why CPU, why the hooks cannot exit 2, why `settings.json` is spliced |
| [Extending it](extending.md) | config knobs, where code goes, the measurement and review rules |
| [Roadmap](roadmap.md) | what is done, what remains, and the gates that decide the cutover |

English is canonical; the Korean pages carry the same content. Design notes for
individual changes live in the repository under `docs/design/`, one per merged
pull request.

---

## What it does

- **Captures** every Claude Code session automatically, through four hooks that
  cost under a millisecond a turn.
- **Extracts** facts with your own Ollama model, then links them into a graph
  of entities and relations.
- **Recalls** with hybrid search — FTS5 BM25 and vector KNN fused by reciprocal
  rank — inside a token budget, filtered by time when the question is temporal.
- **Consolidates** in the background: duplicates merged rather than rejected,
  observations grafted into knowledge.
- **Measures itself**, writing what its input caps saved into a benefit ledger
  on every retain.

Per-project banks keep one repository's memory out of another's. Nothing leaves
the machine it grows on.

---

## What makes it different

- **One binary, one file, zero external processes.** SQLite with `sqlite-vec`
  and FTS5 does vectors, keywords and the graph in-process. No database server
  to start, no restart race to lose data to, and a backup is `cp memgarden.db`.
- **Fast enough to be invisible.** Recall runs an order of magnitude inside its
  budget, and the whole hook layer costs under a millisecond per turn against a
  10 ms allowance.
- **Local by construction, not by policy.** Extraction uses your Ollama;
  embeddings and reranking are compiled in and CPU-forced so they never fight
  the LLM for VRAM. There is no cloud path to enable by accident.
- **Korean works properly.** CJK has no word boundaries, so a naive full-text
  index silently returns nothing — the FTS layer is built for it and guard-
  tested so recall cannot quietly degrade.
- **Bounded everywhere it touches untrusted input.** Every cap in this system
  exists because its absence produced a real incident in the previous one.
- **Recall quality is measured, not asserted.** A frozen corpus and graded gold
  queries produce recall@k / MRR / nDCG, so a ranking change reports a delta
  instead of a feeling — which has already reversed one decision that "felt"
  obviously right.
- **The switch does not throw itself.** Installing the hooks wires them in
  *shadow* mode: real calls, real latency, real ingestion, and nothing reaches
  the model until you say so in a second, separate step.

---

## Status

Phases A, B and C (foundation, core pipeline, hooks) are merged or
code-complete. Migration, the web UI, and the cutover itself remain. The old
system is still running, and stays running until three acceptance gates —
quality parity, performance, lossless migration — are all met.

[Roadmap](roadmap.md) has the detail, including the open defect that currently
gates going live.
