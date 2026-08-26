# Command logs in the bank — where they come from, and the knob that was already there

> **Correction, 2026-08-27.** The central claim below — that the `coding`
> profile is applied here and has `include_tool_calls` set to `true` — **is
> false.** The live loader was finally read rather than inferred:
>
> ```
> profile.name              = ""
> retain.include_tool_calls = false
> profile.retain_mission    = ""
> ```
>
> `~/.config/memgarden/config.toml` has no `[profile]` section and no env
> override, so the preset never applies and the knob sits at its `false`
> default. What misled the original investigation was the **bank** `mission`
> strings, which do carry the coding wording — they were written into the
> database by the 2026-08-08 legacy migration and are not the current config.
> Reading a stored value and calling it a setting is the mistake.
>
> Everything the correction changes is in
> [the section at the end](#what-the-correction-changes); the original text is
> left intact above it so the error stays legible.


Investigated 2026-08-26 from a live observation: of the 106 memories injected
into one working session, **22% were records of commands having been run** —
"Bash 명령어를 사용하여 migrations 목록을 나열함", "Command executed in
background with ID b820wij1j". They are not knowledge; they are the exhaust of
producing it, and they cost about a fifth of every turn's recall budget.

## The measurements

**Injected, one session, 6 recall calls:**

| | |
|---|---|
| memories injected | 106 (17.7 per turn) |
| tokens injected | 6,662 (1,110 per turn) |
| command-log class | 23 of 106 = **22%** |

**Stored, the whole live database:**

| | |
|---|---|
| nodes | 7,224 |
| command-log class | 176 = **2.4%** |
| by type | world 105 · observation 61 · experience 10 |

Classifier precision, hand-checked on a random sample of 12: **11 of 12** are
pure command logs; one is a false positive (a real decision whose text happens
to contain `hook recall`). So ~92%, and the shape of the error is known.

**2.4% of the corpus is taking 22% of the injection budget** — roughly nine
times its share. That over-retrieval is the ranking problem that three
measured attempts already failed to fix, and it is on the do-not-re-propose
list. This document is about the other end: not retrieving them less, but not
making them at all.

## Where they come from

`[retain] include_tool_calls`. It already exists, and it is already the switch
this problem turns on:

| value | `retain/transcript.rs` path | what the extractor sees |
|---|---|---|
| `false` (built-in default) | `text_transcript()` | text blocks only — `thinking`, `tool_use` and `tool_result` are all excluded |
| `true` | `json_transcript()` | tool calls and results, capped but present |

**The `coding` profile flips it to `true`** (`config.rs:363` — "that is the
whole point of the two tool-input caps"), and every bank on this machine runs
the `coding` profile. So the extractor reads the tool traffic and dutifully
writes down that a command was run.

The same profile's retain mission already says:

> "Ignore greetings, **routine tool output**, and transient operational chatter."

**The instruction is already there and the model does not follow it.** That is
the single most useful thing learned here, because it predicts what more
prompt engineering is worth.

## The prompt fix was tried and it failed. Reporting it rather than re-rolling.

The first plan was to teach the extraction prompt what the `coding` profile's
mission could not: `prompts.rs` is a verbatim port of a general-purpose
personal-memory prompt whose "skip the trivia" examples are greetings, weather
and coffee. Nothing in it says that in an engineering transcript, *running a
command* is the coffee.

A skip rule and a worked engineering example were added, and both prompts were
run against 12 real transcript chunks (4–21 tool calls each) through the
configured `qwen3-14b-nothink` model:

| arm | command-log facts |
|---|---|
| current prompt | 1 / 14 = **7.1%** |
| with the new rule + example | 5 / 19 = **26.3%** |

**It went the wrong way.** And the harness is not trustworthy either: chunks
were truncated to 3,200 characters mid-JSON, so 8 of 12 produced zero facts,
leaving 14 and 19 facts to compare. The added example also contains literal
command text (`cargo test --workspace`), which plausibly primed the model
toward exactly what it was told to skip.

Two unreliable numbers pointing opposite ways is not a result. The prompt
change is **dropped**, not re-run until it looks better — the rule that
retired the first AC-1 measurement and the gold-harness claim applies here
too.

## What the knob would actually cost — traced, on one case

The worry about `include_tool_calls = false` is that it also discards findings
that live only in tool output. The test case is concrete: this session's
breakthrough came from a recalled memory,

> node 6366 — "`cargo test --workspace` intermittently dies with SIGSEGV or
> abort in SQLite's FTS5 index due to heap corruption" (2026-08-09)

Traced to document 53, session `8e6edeef`. Blocks in that transcript carrying
both `SIGSEGV` and `FTS5`:

| block type | count |
|---|---|
| `tool_result` | 5 |
| **`assistant` text** | **4** |
| `tool_use` | 2 |

The sentence that became the node is at line 835, in a `tool_result` — the
output of a `grep` over `book/src/roadmap.md`. So text-only extraction would
have missed *that* wording.

But the same knowledge is in the assistant's own prose four times over:

> line 1524 — "위치 | raw rusqlite + 번들 SQLite + **FTS5**, MemGarden 코드
> 0줄 … 증상 | **SIGSEGV**, `stack smashing detected`, `free(): invalid pointer`"

> line 1550 — "`stack smashing` + 무작위 SIGSEGV + 메모리 압력에 비례하는
> 비율은 **하드웨어(RAM) 결함**의 전형이기도 합니다 … 소프트웨어 쪽 이분법이
> 다 막히면 `memtest86`이 다음 순서입니다"

The second one predicts, in August, the exact path this session took in the
end. It is assistant prose, not tool output.

**So `false` looks cheaper than feared: the findings get written down in prose
anyway, and the tool output is the raw material rather than the only record.**

That is one case, chosen *because* it was already known to have helped. It is
an anecdote, not a sample, and it must not be quoted as evidence that the knob
is free.

## What would settle it — the next job

Re-extract the same set of sessions twice, once with `include_tool_calls =
true` and once with `false`, and compare the two fact sets:

* how many command-log facts each produces (the benefit);
* how many substantive facts appear only in the `true` arm (the cost);
* judged blind, per the AC-1 method, so the author is not scoring the arms.

No code change is needed — the knob is config. The cost is compute: dozens of
chunks per session through `qwen3-14b`, which is why it is a job rather than a
step.

Until that runs, the honest statement is: **command logs are 2.4% of storage
and 22% of injection, the switch that would stop them exists and is on, and
what turning it off would cost has not been measured.**

## What the correction changes

<a id="what-the-correction-changes"></a>

**The knob is already off, so there is nothing to turn off.** The section
above proposed `include_tool_calls = false` as the intervention. It is the
current value. The A/B that was built to price it was therefore measuring the
opposite question — *what would turning it on buy?* — which nobody asked, and
it was stopped after 32 chunks.

**Splitting the 178 command-log nodes by how they entered says the ingest
problem is already closed:**

| era | nodes | command-log | rate |
|---|---|---|---|
| legacy migration, 2026-08-08 | 5,310 | 90 | 1.7% |
| cutover catch-up, 2026-08-21 | 1,344 | 86 | **6.4%** |
| **native retain, 2026-08-23 →** | **676** | **2** | **0.3%** |
| total | 7,330 | 178 | 2.4% |

And the two in the newest era are **classifier false positives**: their text is
*"the `include_tool_calls` switch is already present and is set to `true` for
the `coding` profile"* — MemGarden faithfully retaining this investigation's
own wrong claim. Discounting them, **native retain has produced no command
logs at all.**

**Text-only extraction still produces them, which the knob cannot fix.** The
08-21 catch-up ran with `include_tool_calls = false` and still made 86, at
more than triple the migration's rate. They come from the assistant's own
prose — "Bash 명령어를 사용하여 …를 확인했습니다" is a sentence the extractor
can mine without ever seeing a tool block. So dropping tool blocks was never
going to close this path, and the earlier framing of the knob as the root fix
was wrong twice over.

**What survives from the original investigation:**

* the injection measurement — 106 memories, 6,662 tokens, **22% command-log**;
* the storage measurement — **178 of 7,330 = 2.4%**;
* therefore the ~9× over-retrieval, which is unaffected by any of this;
* the observation that more prompt engineering is a poor bet, now on firmer
  ground: the retain mission that says "ignore routine tool output" is `""`
  here, so it is not that the model disobeys the instruction — **the
  instruction was never sent.** Whether sending it would help is untested.

**So the remaining problem is not ingest.** New pollution has stopped on its
own. The 178 rows are a historical residue, entirely from the migration and
the cutover catch-up, and they are being retrieved at nine times their share.
That is a ranking or a cleanup question, and the cleanup end — 178 rows out of
7,330, at ~92% classifier precision, behind a database backup — is the one
this investigation never priced.

**Harnesses kept**, since both are reusable and both earned their place:

* `crates/memgardend/examples/effective_config.rs` — prints what the daemon
  actually resolves. Two minutes of work that would have prevented all of the
  above.
* `crates/memgardend/examples/tool_calls_ab.rs` — runs the real ingest path
  with `include_tool_calls` as the only variable. Its free half stands on its
  own: across all four sessions, turning tool calls **on** would take chunks
  from 61 to 240 and capped tokens from 70,868 to 221,992 — a 3.1× increase in
  extraction input for whatever the tool traffic adds.
