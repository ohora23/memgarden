# `include_tool_calls` A/B — rules, fixed before any fact was extracted

Committed before the extraction arms ran. The commit timestamp is what makes
that checkable, which is the only reason this is separate from the result.

The question: **turning `include_tool_calls` off removes command logs at the
source — what knowledge does it remove with them?**

## The arms

| | |
|---|---|
| **with_tools** | `include_tool_calls = true` — today's shipped setting (the `coding` profile flips it on) |
| **text_only** | `include_tool_calls = false` — the built-in default; `text_transcript()` drops `thinking`, `tool_use`, `tool_result` |

Held equal: the messages, `cwd`, `is_initial`, every other `RetainConfig`
field, `chunk_size`, the model, the retain mission, and the extraction prompt.
**One variable.**

Both arms run the real ingest path — `retain::plan_ingest` then
`chunk::chunk_text` then `extract::extract` — via
`crates/memgardend/examples/tool_calls_ab.rs`. This is deliberate: the first
attempt at this question used a hand-rolled harness that truncated chunks
mid-JSON, produced 14 facts against 19, and had to be thrown away. A harness
that does not reproduce the pipeline is not evidence about the pipeline.

## The sample

**Every session in this project's bank with a transcript, excluding the one
still running.** Not a draw — the population is four, so it is a census:

`b9081c27`, `e8f339bf`, `6c648742`, `8e6edeef`.

Four sessions from one project by one operator. Nothing generalises; that
limit is restated in the result.

## What is counted

**Free and deterministic (no model involved), already measured:**

| | with_tools | text_only | |
|---|---|---|---|
| chunks | 240 | 61 | **−74.6%** |
| capped tokens | 221,992 | 70,868 | **−68.1%** |

**From the extraction arms:**

1. **facts produced**, per arm;
2. **command-log facts**, per arm — the benefit;
3. **facts present only in `with_tools`** — the cost.

## The classifier, fixed here

Command-log class = the regex below over `text`, applied **identically to both
arms**, so any error it makes it makes symmetrically:

```
(Bash 명령|명령어를 (사용|실행)|명령을 실행|Command executed|command executed
|ran (a |the )?command|executed a Bash|User ran|를 실행했|을 실행했
|스크립트를 실행|background with ID|커맨드|명령어 `)
```

Hand-checked on a random sample of 12 drawn from the live database: **11 of 12
correct**, the miss being a real decision whose text contains `hook recall`.
≈92%, and the shape of the error is known. It is a heuristic and is reported
as one.

## How "only in `with_tools`" is decided

The arms chunk differently, so facts cannot be paired positionally. Matching is
by meaning, using the daemon's own embedder (`POST /v1/embed`, the same model
the bank is indexed with):

* every `with_tools` fact is matched against **all** `text_only` facts of the
  same session by cosine similarity;
* its score is the **maximum** over that set;
* the count of unmatched facts is reported **at several thresholds**
  (0.70 / 0.75 / 0.80 / 0.85) rather than at one chosen after seeing the data,
  and the full distribution is plotted as counts per bucket.

**No single threshold is treated as the answer.** A threshold picked to make a
number come out is the failure this project retired the first AC-1 measurement
for.

## The judgement, and who makes it

Counting unmatched facts says how many are *different*, not whether they are
*worth keeping*. So:

* a **random sample of 20** unmatched `with_tools` facts (seeded, seed
  recorded) is judged;
* each is labelled **durable knowledge** / **command log** / **transient
  chatter**, by a judge given the fact and the repository but **not** which arm
  it came from and **not** this document's hypothesis;
* the same 20-fact treatment is applied to a control sample drawn from
  `text_only` facts, so the judge cannot infer the arm from the question.

## What would decide it

* **text_only loses few durable facts** → turning it off is close to free, and
  the 68% input-token reduction is a bonus rather than a cost.
* **text_only loses durable facts at a material rate** → the knob stays on and
  the command-log problem needs a different answer than this one.

Either way the number is reported. There is no arm this document prefers, and
the free half already measured (−68% tokens) is **not** an argument for
`text_only` on its own: cheaper input that loses knowledge is not a saving,
which is exactly what MX-3 established about the layer above.

## Limits, stated in advance

1. **Four sessions, one project, one operator.** A census of this bank, not a
   sample of anything wider.
2. **The classifier is a regex at ~92%.** It bounds the precision of the
   command-log counts, not of the judged sample.
3. **Cosine matching is not meaning.** Two facts can say the same thing and
   score low, or differ and score high. That is why the threshold sweep and
   the human-judged sample both exist.
4. **The author built the system.** The judge is independent, the sample is a
   census, the rules are in this file, committed first. That is the
   mitigation, not a proof of neutrality.
