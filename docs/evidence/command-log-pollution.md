# Command logs in the bank — where they come from, and the knob that was already there

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
