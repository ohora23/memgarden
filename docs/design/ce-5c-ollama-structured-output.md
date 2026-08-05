# CE-5c — the extraction schema was never enforced

The `format` JSON schema this daemon has sent on every LLM call since CE-5a was
**silently ignored** by Ollama, because it was sent to the wrong endpoint. The
only thing asking for JSON was the prompt.

Found by investigating the chunk failure that opened the cursor gap HK-1g
fixes, on the first real retain of the shadow run.

Branch `fix/ollama-truncation-diagnosis`.

---

## The measurement that settles it

Same model, same schema, same options, one endpoint apart:

```
POST /api/chat      format {"n": integer}   prompt "reply about the weather, no JSON"
  -> "It's such a lovely day outside—perfect weather for a walk or some fresh air!"

POST /api/generate  format {"n": integer}   prompt "reply about the weather, no JSON"
  -> { "n": 1 }
```

Ollama 0.21.2 enforces `format` on `/api/generate` by constraining the
decoding grammar. On `/api/chat` it accepts the field and ignores it.

The source comment said as much —

> Ollama ignores it for `/api/chat` … but it's cheap and it's what the plan's
> verification runs used

— and the conclusion drawn from it was to keep sending it anyway. The
conclusion available was **to use the endpoint where it works**.

Re-verified with the real extraction schema, not a toy: enums, nested arrays,
per-item `required`. `/api/generate` returned valid JSON with the required keys
present, `done_reason: stop`.

---

## What it cost

The first forced retain of the shadow run:

```
done · chunks 2/3 · failed 1 · facts 12
error: failed to parse ollama response as JSON after retries:
       expected `,` or `}` at line 1 column 4681
```

One chunk's facts lost, and — because a `done` job with a failed chunk leaves
the durable cursor behind (HK-1g) — 3.4 MB of transcript left unsettled.

**Reproduced**, same transcript into a throwaway bank, with the diagnosis below
already in place:

```
done · chunks 6/7 · failed 1 · facts 57
error: ollama reply was truncated at the output limit after 8192 tokens
       (24525 chars) — the JSON is a prefix, not malformed
```

So the reply was not garbage: it was **valid JSON that stopped**. A
3,000-character chunk produced a 24,525-character answer and ran into
`num_predict`. Unconstrained decoding let the model keep listing facts until
the budget ran out.

---

## Three changes

### 1. `/api/generate` — the schema is enforced

`try_chat` posts `system` + `prompt` instead of a two-message `messages`
array; Ollama renders both through the same model template. The reply field
moves from `message.content` to `response`.

A structurally invalid reply is no longer producible. That removes the failure
*class*, not this instance of it.

### 2. `maxItems: 24` — the runaway is bounded by the grammar

Enforcement is not advisory: verified live, a schema capped at 3 answered
"list twenty fruits" with exactly 3 items.

24 is ~2.5× what a 3,000-character chunk actually yields (measured: 57 facts
over 6 chunks ≈ 9.5). It bounds a runaway without touching an ordinary chunk,
and it puts a ceiling on output length that `num_predict` can no longer be
reached from.

**The trade is explicit and it is not free.** A chunk with more than 24 real
facts loses the tail. Before this, such a chunk lost *everything* — the
truncation took the whole reply and the job recorded zero facts for it. Losing
a tail beats losing the body, and the cap is one constant away from being
raised if a shadow run shows chunks legitimately exceeding it.

One cosmetic effect worth recording: when the grammar forces closure at the
cap, the final item can end mid-token (observed: `"Orange', "`). The document
stays valid and parses; the last fact of a capped chunk may read oddly.

### 3. The failure is now diagnosable, and not retried blindly

Three things were wrong with how a parse failure was handled, and all three
made this investigation harder than it needed to be:

* **`done_reason` was never read.** A truncation and a garbage reply produced
  the same log line and the same error type. `OllamaError::Truncated` now
  carries `eval_count` and the length, and says in its message that the JSON is
  a prefix.
* **The log kept the first 512 characters.** For a truncated reply the only
  informative part is the *end*. It now logs head **and** tail, plus
  `done_reason`, `eval_count` and the character count.
* **A truncation was retried three more times.** At `temperature 0.1` that is
  the same computation reaching the same limit — the original failure's four
  attempts all died at column 4681. Truncation now fails fast and frees the
  Ollama permit instead of spending the GPU three more times on a foregone
  conclusion.

---

## Blast radius

Every LLM call in the daemon goes through `chat_json_inner`: extraction,
consolidation, dedup adjudication, mental-model refresh, reflect. All of them
now get an enforced schema; only extraction gets `maxItems` (the others have
their own shapes and no observed runaway).

Six test stubs moved from `/api/chat` to `/api/generate` and from
`{"message":{"content":…}}` to `{"response":…}`. They are stubs of *our*
client, so they had to move with it — and the fact that they all passed
against a mock that ignored `format` is why the real behaviour went unnoticed
for six PRs.

---

## Diverged from legacy

Legacy calls Ollama through the OpenAI-compatible chat completions API and
relies on prompt instructions plus lenient parsing, with no schema enforcement
available at all. Its `fact_extraction.py` carries the repair paths that
implies. We keep the lenient parser (`extract::parse`) as a second line, but it
is no longer the only one.

---

## Known limits

**`num_predict` is still reachable in principle.** `maxItems` bounds the array;
a single pathological `what` string could still run long. No cap on individual
string fields — `maxLength` in the schema would add one, at the cost of the
grammar cutting sentences mid-word. Left alone until something shows it matters.

**The template is not byte-identical.** `/api/chat` with two messages and
`/api/generate` with `system` + `prompt` render through the same template, but
"same template" is not "same tokens" in every edge case. Extraction quality is
not measured by the gold harness (that measures *ranking*), so this change is
verified for shape, not for judgement quality. A shadow run comparing fact
counts per chunk against the numbers in this note is the cheap check.

**The other callers were not re-verified live.** Consolidation, reflect and
mental-model refresh now get an enforced schema they previously did not. That
should only ever remove failures, but "should only" is not "was measured".
