# MX-4 — does MemGarden help a user who keeps no notes?

Written and committed before any task was drawn and before either arm ran.

## Why this exists

MX-3 lost 11 to 7, and could not answer its own question: 24 of its 27 tasks
had their answer on disk already, because this machine's owner writes nearly
everything down — an Obsidian vault, `MEMORY.md` files, handoff notes, long
commit messages. A live counterfactual on 2026-10-17 found the same thing from
the other side: 0 of 214 injected memories were used, and most of the relevant
ones repeated a note already in context.

A typical user writes none of that. MX-4 asks the question for them: **with
the notes taken away, does the bank answer what the repository cannot?**

## How the tasks are drawn

From real user prompts in this machine's Claude Code transcripts dated
2026-08-15 to 2026-10-16 inclusive, in this order:

1. **question-shaped** — MX-3 rule 1, same keyword list;
2. **20..600 characters** — MX-3 rule 2;
3. **not a paste** — dropped if it contains `<pasted_content` or is a
   compaction summary, a slash command, or a task notification. A handoff
   paste is the notes this experiment removes;
4. **stands alone** — MX-3 rule 3′: three independent classifiers, given the
   prompt but not the conversation, majority of three;
5. **the bank existed** — the session's bank holds at least 200 nodes created
   before the prompt;
6. **deduplicated** by normalised text.

Then a **random sample of 30, `seed = 11`**, or all survivors if fewer.

### Amendment, 2026-10-18 — the transcripts do not reach back far enough

**Found after rules 1–3, 5 and 6 ran, before rule 4 and before any arm.** Claude
Code deletes transcripts after 30 days; the oldest left is from 2026-09-26, and
rules 1–3, 5, 6 over them leave **27** candidates. `~/.claude/history.jsonl`
keeps every prompt — text, time, project, session — but not the answers.

So the pool becomes: transcripts as above, **plus** prompts from
`history.jsonl` dated 2026-08-15 to 2026-09-25 whose transcript no longer
exists, through the same rules. For those tasks there is **no reference
answer**: the stratifier and the judges work from the machine alone, as MX-3's
judges did. Each task records whether it had a reference.

This cannot favour either arm: the arms never see the reference.

## The bank, frozen per task

Each task sees the bank **as it stood when the prompt was typed** — MX-3 used
one snapshot newer than its prompts, so a session's own conclusions could be
recalled back to it. Here, on a copy of the live database:

* every node created at or after the prompt's timestamp is deleted;
* every observation that cites a source created at or after it is deleted,
  because consolidation rewrites an observation's text from its sources.
  `updated_at` cannot be used: the v15 re-embedding touched every row.

Deletion goes through the store (`examples/delete_nodes.rs`), tasks are taken
newest first, and deletion only ever grows, so one copy serves all tasks.
Recall runs on a separate daemon on its own port; the live daemon is not
touched.

## The two arms

| | |
|---|---|
| **A — no notes, no memory** | the prompt alone |
| **B — no notes, MemGarden** | the prompt, plus exactly what `memgarden hook recall` emits for it against the frozen bank, delivered by a `UserPromptSubmit` hook |

Held equal: model (`claude-sonnet-5-5`), turn cap (40), the instruction, the
workspace, the sandbox.

**The workspace** is the session's working directory as of the last commit
before the prompt, copied, with the notes removed: every `*.md` except
`README.md`, `docs/`, `.omc/`, `CLAUDE.md`, `AGENTS.md`. A task whose working
directory was not a git repository gets an empty directory at that path.

**The sandbox** (`bwrap`): the root filesystem read-only; `$HOME` replaced by
an empty tmpfs into which only toolchains and the Claude credentials file are
bound — so the vault, every `MEMORY.md`, all transcripts, `CLAUDE.md`, and
the memgarden database are absent; `/run/user/1000` hidden, so no service can
be restarted. The network is shared, because the agent needs the API.

*Amendment, 2026-10-18, before any arm ran:* the live daemon answers recall
on loopback without a token, so a sandboxed agent could have queried today's
bank with `curl` — arm A would have become a memory arm. The sandbox
therefore preloads a `connect()` shim that refuses loopback ports 9100, 9114
and 9077 (verified from inside: `curl` exit 7, Python `ConnectionRefused`,
the API still reachable). Statically linked binaries bypass it; the
transcript check below is the second line. The workspace's git history is
also rewritten so that no commit holds any removed note — otherwise
`git show <old>:docs/…` would have handed the notes back.

**Every run asserts its arm and its isolation.** A's transcript must contain
no `<memgarden_memories>` block and B's must contain the expected one. A run
that touched port 9100, the frozen daemon's port, the vault, or any
transcript is discarded and reported, not judged.

## Stratification, before the arms run

A separate agent, in arm A's sandbox, is given the task and the reference
answer (below) and asked only whether the reference's key content can be
found in that sandbox. Labels: **findable** / **memory-only** / **neither**.
Totals are reported per stratum and never merged into one headline.

## Judging

Blind, three judges per task, majority of three. Answers are relabelled by a
hash of the task id and a salt.

Judges get the prompt, both answers, the **reference** — what the original
session actually answered, with the owner's notes available — and read access
to the whole machine, notes included, so they can check facts. They are told
the reference may itself be wrong or rely on things said earlier in its
conversation.

Per task: **A better / B better / equivalent / neither answered**, plus, per
answer, a factual-error flag and an "asked the user / could not proceed" flag.
A tie is `equivalent`.

Token counts, tool calls and wall-clock are read only after the verdicts.

## What would count as "MemGarden helps a notes-less user"

All three, stated now:

1. memory better ≥ no-memory better **+ 3**, overall;
2. in the memory-only stratum, memory better > no-memory better;
3. factual-error flags for the memory arm ≤ those of the no-memory arm.

Anything else is reported as it is, with the losses quoted.

## Limits, stated in advance

1. **The sandbox removes notes; it cannot remove what the code itself
   records** — commit messages, code comments, test names. A notes-less user
   still has those, so this is intended.
2. The machine's live state (GPU, services, installed packages) is today's,
   not the prompt's. It is the same for both arms.
3. The model is not the one the original session used.
4. One machine, one operator; the bank was built by someone who also wrote
   notes, so its contents may be richer or poorer than a notes-less user's.
5. 30 tasks at most, fewer per stratum.
6. The author built the system. Mitigation: mechanical selection, blind
   independent judges, rules committed first. Not a proof of neutrality.

## Re-run (v2), 2026-10-18 — written after the first result, before the re-run

The first run's sandbox emptied the whole home directory, not only the notes,
and its error flags measured that (see the result). The re-run changes **only
the sandbox**; tasks, frozen injections, workspaces, instruction, model, judges
and the three conditions above are unchanged.

* The home directory is visible read-only: `~/.claude/plugins`, the real
  `~/.claude/settings.json`, `~/.config` (systemd unit files included), other
  repositories, toolchains, the memgarden embedding models.
* Hidden: the vault; `~/.claude/CLAUDE.md`, `projects/` (transcripts and every
  `MEMORY.md`), `history.jsonl`, `file-history/`, `paste-cache/`, session and
  backup directories; other agents' homes (`.codex`, `.gemini`, `.opencode`,
  …); `~/.omc`; the memgarden data directory except its models; every other
  checkout of the task repositories.
* User settings are not loaded (`--setting-sources project`); the arm's hook
  arrives through `--settings`, so the settings file the agent can read is the
  real one.
* The stratifier is re-run in the new sandbox, before the arms.

Both runs are reported. The re-run's table is the one the conditions are read
from, because it is the one whose sandbox matches the question.
