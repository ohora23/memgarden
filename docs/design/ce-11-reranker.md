# CE-11 — Embedded cross-encoder reranker (PR B10)

Branch `feat/ce-11-reranker`. No migration, no schema change, no new REST
endpoint. One new direct dependency (`hf-hub`, already in the graph — see
below), and **zero new crates**: `Cargo.lock` gains exactly one line, the edge
from `memgardend` to `hf-hub`.

Legacy ports: `engine/cross_encoder.py:103,131` (the model),
`engine/search/reranking.py:272-286` (date decoration), `:298-312` (sigmoid),
`:318-324` (NaN sanitization), `engine/memory_engine.py:5266` (truncate to the
rerank depth, drop the tail).

## What this adds

| Path | What |
|---|---|
| `crates/memgardend/src/rerank.rs` | `Reranker { inner: Mutex<TextRerank> }`, `sigmoid`, `decorate`, `rerank_inputs`, `top_k_warning`, `load_at_startup`, the pinned revision + digests. |
| `crates/memgarden-core/src/config.rs` | `[reranker]` — `enabled`, `model`, `top_k`, `threads`, `batch_size`, with validation. |
| `crates/memgardend/src/recall/mod.rs` | The one branch: with the cross-encoder on, its sigmoid-normalised logit replaces `scoring::passthrough_base`. |
| `crates/memgardend/src/routes/metrics.rs` | `reranker_loaded` — "configured on, silently running off" is otherwise invisible. |
| `crates/memgardend/src/bin/recall_bench.rs` | `rerank=<top_k>` — the only measurement knob the harness exposes. |
| `crates/memgardend/tests/recall_api.rs` | `MEMGARDEN_BENCH_RERANK=<top_k>` on the latency bench; the parity test; the live end-to-end test. |
| `gold/results.jsonl` | Three appended runs: the reproduced baseline, `top_k = 10`, `top_k = 20`. |

## How to read this note

The measurement in this PR did **not** cleanly agree with the recommendation,
and an earlier draft resolved that by choosing a presentation — comparing
tables built on different query sets, and convicting `top_k = 20` on a stratum
declared inadmissible two sections earlier. Three reviewers caught it
independently. The recommendation was right the whole time; the reasoning was
not, and the difference matters because this note outlives the diff.

So, explicitly: **every quality table below is 13 queries with the conclusion
stratum excluded, both arms.** Where the evidence is split it is shown split.
What choosing `top_k = 10` gives up is stated as a cost, not presented around.

## Off by default, and off IS parity

This is the single most important sentence in the note, so it goes first and
plainly: **shipping the reranker disabled is not a reduction relative to the
system being matched.** The live legacy daemon runs
`HINDSIGHT_API_RERANKER_PROVIDER=rrf` — a passthrough, no cross-encoder —
adopted deliberately as part of its 830 ms → 20 ms latency fix. Every Phase B
number, every AC-1 comparison and the whole AX-2 baseline were taken against
that configuration on both sides. Turning the cross-encoder on is a
*divergence from the reference system*, not a return to it.

What CE-11 ships is therefore a measured, supported opt-in with the numbers
attached, and the numbers turn out to be good enough that the opt-in is worth
recommending for latency-tolerant callers. It is still off in the default
config, because AC-2 is a hard requirement and the reference system is the
thing AC-1 compares against.

## Design

### The model, and why the download is hand-rolled

`Xenova/ms-marco-MiniLM-L-6-v2` is the ONNX export of
`cross-encoder/ms-marco-MiniLM-L-6-v2`, which is exactly what legacy loads
(`cross_encoder.py:103,131`). fastembed has no built-in `RerankerModel` entry
for it, so `TextRerank::try_new` — the variant that downloads — is unavailable
and the user-defined path (`try_new_from_user_defined`) is the only one. That
path takes *file bytes*, not a repo id.

fastembed's own downloader (`pull_from_hf`) is `pub fn` but sits inside a
private `mod common` (`fastembed-5.17.4/src/lib.rs:75`) and is not re-exported,
so it cannot be reached from outside the crate. The plan assumed `hf-hub` was
already a direct dependency; it is not — it is present only as fastembed's
`hf-hub-rustls-tls` feature.

**Resolution: declare `hf-hub` directly, pinned to `=0.5.0` with no features of
its own.** Cargo unifies features across the graph, so it resolves to the
identical build fastembed already links (`ureq` + `rustls-tls`). Verified
rather than asserted: `git diff Cargo.lock` is a single `+ "hf-hub",` line
inside the `memgardend` dependency list — no new crates, no version changes,
nothing rebuilt that was not already being built. Six lines of `ApiBuilder`
call the same code fastembed would have.

Rejected alternatives:

* **`reqwest` with a TLS feature.** The workspace comment on `reqwest` says in
  so many words not to add one — Ollama is loopback HTTP and has nothing to
  negotiate. Adding rustls to `reqwest` to fetch five files, when the exact
  same TLS stack is already linked via `hf-hub`, is a second copy of the
  problem.
* **No download at all** — require the operator to place five files by hand and
  fail with a clear message. Genuinely tempting for an off-by-default feature,
  and it needs no dependency at all. Rejected because the daemon already
  auto-downloads its embedding model into the same directory, and a feature
  that behaves differently from its neighbour for no user-visible reason is a
  papercut that costs more in confusion than six lines cost in code.

The ONNX path in that repo is **`onnx/model.onnx`**, not `model.onnx` at the
root; the four tokenizer files are at the root and all four are mandatory
(`fastembed/src/common.rs:32` — `TokenizerFiles` has no optional field). Both
facts are recorded in `rerank.rs` next to the constants, because the wrong
guess produces a 404 at load time and nothing else.

The files land in `[embedding] model_dir`, deliberately shared rather than
given a knob of its own: it is the daemon's one model cache, and hf-hub already
namespaces each repo under `models--<org>--<name>/` inside it.

### The *artifact* supply chain, which is a separate problem from the crate one

The section above is about the dependency graph. It says nothing about the
90 MB of executable ONNX the daemon downloads and runs, and an earlier draft
left that silent — which against this repo's own claims reads as an oversight
rather than a decision.

Two things here are genuinely different from the embedding model, and they are
why silence was not good enough:

1. **A third-party org, not a fastembed-curated built-in.** `EmbeddingModel` is
   an enum; the repo, revision policy and file list are fastembed's. Here they
   are ours.
2. **`[reranker] model` is the daemon's first operator-settable "which remote
   artifact do we execute" knob.** `EmbeddingConfig` has no `model` field at
   all. A typo in this one selects executable content.

Default `ApiBuilder::new().model(...)` resolves the moving `main` ref and keys
the cache on `blob_path(&metadata.etag)` — an etag is a cache key, not
verification. TLS *is* verified, and `HF_ENDPOINT` is **not** honoured (we call
`new()`, not `from_env()`), so the trust anchor would have been "HuggingFace
plus the `Xenova` account", permanently and invisibly: the cache is written
once and every later boot is offline.

**So it is pinned.** `PINNED_REVISION` is the exact commit this PR measured
(`a09144355adeed5f58c8ed011d209bf8ee5a1fec`) and `PINNED_DIGESTS` carries a
SHA-256 for all five files, checked on every load — so the check also catches
local cache corruption, not just a bad download. `sha2` was already a direct
dependency of `memgardend`, so this costs zero new crates.

The operator-settable knob is handled honestly rather than by pretending it
does not exist, in two branches:

* `model == DEFAULT_RERANK_MODEL` → pinned revision, all five digests verified,
  load refused on mismatch.
* anything else → the `main` ref, **no** revision pin and **no** digest, plus a
  startup `WARN` naming exactly that. Refusing outright would make the knob
  useless for the AC-1 experiments it exists for; refusing *silently* is what
  this avoids.

Verifying all five files rather than only `onnx/model.onnx` costs one loop and
removes the need to reason about which of them counts as "executable".

### Score normalization

Local cross-encoders emit unbounded logits, so `sigmoid` maps them to `[0, 1]`
— the same range `passthrough_base` produced, which is what keeps the three
multiplicative boosts meaningful (each is `1 + alpha * (signal - 0.5)`, and a
base outside `[0, 1]` would make the ±21 % envelope meaningless). Legacy does
the same at `reranking.py:298-312`, including the branch that passes calibrated
`[0, 1]` scores through untouched — that branch is for hosted rerankers
(Cohere, Jina) and has no analogue here, so it is not ported.

`sigmoid` saturates rather than overflows at the extremes (`(-x).exp()` is
`inf` for very negative `x`, giving exactly 0.0), and NaN maps to 0.0 rather
than propagating. The NaN case is ported because legacy hit it
(`reranking.py:318-324`): a NaN base sorts unpredictably and serialises to JSON
`null`, which breaks a client expecting a number.

### Date decoration

Ported from `reranking.py:272-286`, both decorations in legacy's order:
`context` is prefixed as `"{context}: {text}"`, then `occurred_start` — and
only `occurred_start`, not `scoring::effective_time`'s COALESCE — wraps the
whole thing as `"[Date: {%B %d, %Y} ({%Y-%m-%d})] "`. Both date styles are
emitted because the model has seen both in training, which legacy states
explicitly at `:279`.

One trap, commented in the code: legacy's own comment writes the example as
"June 5, 2022", but Python's `%d` is zero-padded and really produces
"June 05, 2022". jiff's `%d` is zero-padded too, so this matches
byte-for-byte — **the legacy comment is wrong, not the legacy code**, and the
note exists so nobody "fixes" this to `%-d` and silently diverges.

### `top_k` and what it structurally cannot do

Legacy truncates the candidate list to `rerank_limit` and cross-encodes what
survives, dropping the tail outright (`memory_engine.py:5266`). `top_k` is that
knob, and it is also therefore a cap on what recall returns whenever it is
below `[recall] limit`.

The consequence is worth stating loudly because it bounds every quality number
below: **a reranker can only reorder what retrieval already surfaced.** At
`top_k = 10` nothing below RRF rank 10 is reachable. The `#[ignore]`d
`live_rerank_recall` test pins this as a *property*, not a footnote: at
`top_k = 8` the cross-encoder lifts the on-topic fact from BM25 rank 4 to rank
1; at `top_k = 3` it cannot, because the answer is at RRF rank 4 and the model
never sees it. Both halves are asserted.

## Measured — recall quality

Against AX-2's harness, corpus `baee3f40…4bda868` (2718 nodes), `now =
1785715200000`, `limit = 20`, `max_tokens = 8192`, budget `mid`, all three
recall types — **the baseline's configuration exactly**, with `rerank` as the
only variable. The three runs are appended to `gold/results.jsonl`.

The disabled arm reproduces AX-2's recorded baseline **digit-for-digit on every
query and every stratum**, which is the parity claim measured at corpus scale
rather than on a fixture.

> **Re-baselined 2026-08-03 by `fix/ce-8-korean-absolute-dates`.** That PR made
> `8월 2일` parse, so q17 now exercises the temporal arm and its retrieval
> changed in all three arms. All three were re-run at this configuration
> exactly (`gold/results.jsonl` lines 5-7, commit `33d49519`); **only q17
> moved**, every other query reproducing digit-for-digit including its uuid
> list. Every table below carries the re-baselined figures. **This is not
> cosmetic: the reranker's aggregate nDCG@10 gain drops from +0.077 to +0.008
> and its temporal gain inverts from +0.251 to −0.192.** The decision —
> reranker off by default — is unchanged and, if anything, better supported;
> it was settled on latency unconditionally. The superseded figures are in
> `docs/design/ax-2-recall-quality.md` under *Superseded*.

**Every table below is 13 queries with the conclusion stratum excluded**, both
arms, no exceptions. That basis is stated once here because switching it
between tables is exactly how a reader gets misled — and an earlier draft of
this note did switch it, which is how `top_k = 20` came to be described as
"flat on nDCG" when on a consistent basis it is slightly ahead.

### Per stratum, nDCG@10 (the metric duplication does not deflate)

| stratum | queries | baseline | `top_k = 10` | Δ vs baseline |
|---|---|---|---|---|
| identifier / proper noun | 4 | 0.4163 | **0.4952** | **+0.079** |
| memcompare | 5 | 0.2032 | 0.2399 | +0.037 |
| graph | 2 | 0.5489 | 0.5462 | −0.003 |
| temporal | 2 | 0.3142 | 0.1220 | **−0.192** — a real regression now that q17 exercises the arm; see below |
| conclusion | 1 | — | — | **excluded — structurally unmeasurable, see below** |

MRR, same arms:

| stratum | baseline | `top_k = 10` | Δ |
|---|---|---|---|
| identifier | 0.6875 | **0.8750** | +0.188 |
| memcompare | 0.3833 | 0.6000 | +0.217 |
| graph | 0.7500 | 1.0000 | +0.250 |
| temporal | 0.5000 | 0.3056 | **−0.194** |

### Aggregate, conclusion stratum excluded (13 scored queries)

| metric | baseline | `top_k = 10` | Δ |
|---|---|---|---|
| recall@1 | 0.0414 | 0.0465 | +0.005 |
| recall@5 | 0.2546 | 0.2466 | −0.008 |
| recall@10 | 0.4026 | 0.3468 | **−0.056** |
| MRR | 0.5513 | **0.7009** | **+0.150** |
| nDCG@10 | 0.3390 | 0.3474 | +0.008 |

**Read against the re-baseline, the reranker's case is much narrower than this
note originally recorded.** It was `+0.257` MRR and `+0.077` nDCG@10; it is now
`+0.150` and `+0.008`, and recall@10 has gone from flat (+0.002) to a **0.056
loss**. The reason is entirely q17: a temporal constraint puts four relevant
nodes into the baseline's top 10, and the cross-encoder — which scores text
pairs and cannot see a date window — pushes three of them back out.

**MRR remains the honest headline**, for the reason in the tie-break section
below (top-of-block injection lives on rank 1), and it is still a large gain.
But an earlier reading of this note as "+0.077 nDCG@10" no longer holds, and
nothing here should be quoted as a general ranking-quality win.

For reference, the 14-query figure the harness prints (which folds the
conclusion stratum back in) is `0.038/0.236/0.388/0.522/0.323` →
`0.043/0.229/0.336/0.661/0.331`. It is printed by the tool, not used for any
judgement here.

**The conclusion stratum is excluded, not averaged in.** AX-2 established that
it is structurally unmeasurable against this corpus: three of its four queries
have no answer in the corpus at all, and q11 has 23 labels of which zero are
grade 2, because curated conclusions live in native `MEMORY.md` which this
export does not cover. Its `top_k = 10` value is identical to baseline (0.1131)
— but moving one query with no core answer in reach is noise with a decimal
point on it, so it is out of every table above **and out of the `top_k = 20`
comparison below**, where it would otherwise have decided the outcome.

### The temporal stratum: half of it is now valid, and on that half the reranker hurts

This section originally warned that the stratum's **+0.251** was meaningless
because *neither* query exercised the temporal arm. After the re-baseline the
caveat is narrower and the number has inverted, so both halves are restated.

* **q17 (`8월 2일`) now DOES exercise the temporal arm.** It previously
  extracted no constraint at all — `window` was `None`, the arm never ran, and
  `scores.temporal` was `NEUTRAL` for every candidate.
  `fix/ce-8-korean-absolute-dates` fixed that: it now resolves to a single-day
  2026-08-02 window and the arm runs. On the off arm q17 goes nDCG@10
  0.149 → 0.628, MRR 0.100 → 1.000, recall@10 0.250 → 1.000.
* **q15 (`지난주`) still does not**, and that is not fixable here. It resolves,
  with `now` pinned to Monday 2026-08-03, to a window containing 2697 of the
  corpus's 2718 facts. The arm fires and contributes a near-uniform fourth
  ranked list — effectively a no-op. **This is a property of a four-day
  corpus, not a defect in the window logic**, and AX-2 records it as
  explicitly not-a-bug. It needs a corpus spanning more calendar time.

**On the half that is now valid, the cross-encoder is a regression:**
temporal nDCG@10 **0.3142 → 0.1220** at `top_k = 10`, where this note
previously recorded 0.0745 → 0.3254. The mechanism is legible rather than
mysterious — the reranker scores query-document *text* pairs and knows nothing
about the date constraint that selected the candidates, so on a query whose
answer is defined by a date it demotes three of the four relevant nodes out of
the top 10.

Two caveats, both weakening the finding, both stated because the temptation is
to over-read a number that moved this far: the stratum is **two queries wide**
and q17 has only **four** relevant nodes; and one of those two queries is still
invalid. This is a signal to widen the gold set, not a proven property of
cross-encoders. It does not move CE-11's decision, which was settled on latency
unconditionally.

### The identifier guardrail: passed against baseline, traded against `top_k = 20`

The brief was explicit that a reranker which raises the aggregate while
regressing the proper-noun stratum is a failure, not a trade. Against the
baseline it does not regress: identifier nDCG@10 **0.4163 → 0.4952** and MRR
**0.6875 → 0.8750**. The evidence that the floorless hybrid was right —
jcode's argument that a hard cosine floor zeroes recall on identifier-heavy
agent memory — is *strengthened*. The lexical arm still surfaces the
proper-noun matches; the cross-encoder puts the right one first.

**And this is the column `top_k = 10` gives up to `top_k = 20`**, which is
stated here rather than left for the reader to derive from two tables that
never meet:

| identifier stratum (4 queries) | baseline | `top_k = 10` | `top_k = 20` |
|---|---|---|---|
| nDCG@10 | 0.4163 | 0.4952 | **0.5954** |
| recall@10 | 0.4183 | 0.4183 | **0.5318** |
| MRR | 0.6875 | 0.8750 | 0.8750 (tied) |

`top_k = 20` is **+0.100 nDCG@10 ahead of `top_k = 10` on the guardrail** —
larger than the +0.079 that `top_k = 10` gains over baseline. Choosing 10 is
knowingly declining that, for the reasons in the next section.

`recall@10` **loses 0.056** at `top_k = 10`. The mechanism was always expected
to hold it near flat — the reranker reorders the RRF top-10 and drops the tail,
so the *set* inside the measurement window should be close to the baseline's —
but it is not pinned at zero: the baseline's top-10 is ordered by
`passthrough_base × boosts`, so the ±21 % envelope can swap items across the
rank-10 boundary in either direction. It does, in both: the memcompare stratum
drops 0.3476 → 0.3276 and, post-re-baseline, the temporal stratum drops
0.5000 → 0.1875, which is where most of the aggregate loss comes from.
(Pre-re-baseline this line read "+0.002, nearly flat".) **Read MRR.**

### `top_k = 20`: the ranking evidence is split, and latency settles it

Same 13-query basis as everything above.

| metric (13q) | baseline | `top_k = 10` | `top_k = 20` | ranking winner |
|---|---|---|---|---|
| recall@1 | 0.0414 | **0.0465** | 0.0379 | 10 |
| recall@5 | 0.2546 | 0.2466 | **0.2555** | 20, but neither beats baseline by much |
| recall@10 | 0.4026 | 0.3468 | **0.4353** | 20 |
| MRR | 0.5513 | **0.7009** | 0.6187 | 10, decisively |
| nDCG@10 | 0.3390 | 0.3474 | **0.4121** | 20, no longer narrowly |
| identifier nDCG@10 | 0.4163 | 0.4952 | **0.5954** | 20 |
| graph nDCG@10 | 0.5489 | **0.5462** | 0.4966 | 10 |
| memcompare nDCG@10 | 0.2032 | 0.2399 | **0.2578** | 20 |
| temporal nDCG@10 (2q, half valid) | 0.3142 | 0.1220 | **0.3470** | 20 |

**`top_k = 20` wins nDCG@10, recall@5, recall@10, the identifier guardrail,
memcompare and temporal. `top_k = 10` wins MRR by 0.082, recall@1 and graph.**
On ranking alone this is not a clean call, and any sentence claiming it is
depends on which metric is quoted first.

**The re-baseline widened `top_k = 20`'s ranking lead**, which is worth stating
because it cuts against the configuration that shipped: nDCG@10 was `0.3787`
vs `0.3824` (+0.004 to 20) and is now `0.3474` vs `0.4121` (+0.065 to 20).
`top_k = 20` recovers q17's temporal nodes that `top_k = 10` drops. Latency
still decides, and latency still says 10 — but the ranking cost of that choice
is larger than this note first recorded.

**Latency settles it, and latency is unconditional.** `top_k = 20` doubles a
cost that already triples p50 against a 35 ms budget and already drags the
background ingest loop below 90 % of offered load (below). No stratum selection
is needed to reach that conclusion.

The ranking tie-break, given latency has already decided: MRR is the metric a
top-of-block injection lives on. Recalled memories are prepended to a prompt in
rank order and the client model reads down; the *first* relevant item arriving
at rank 1 rather than rank 3 is worth more than one extra relevant item
appearing at rank 9. 0.701 vs 0.619 is a real gap on exactly that — though it
is 0.082, not the 0.133 this note first recorded, so it carries less weight
than it did.

**What is being given up, stated plainly:** +0.100 identifier nDCG@10, +0.089
recall@10, +0.065 aggregate nDCG@10, +0.225 temporal nDCG@10. The guardrail
column is the real cost, and it is a cost, not a non-event. **The re-baseline
made this bill larger on every line**: the aggregate nDCG@10 concession was
+0.004 and is now +0.065. If a future caller's budget absorbs the latency,
`top_k = 20` is the better *ranking* configuration by a clearer margin than
this note first recorded, and it should not be read as saying otherwise.

One `top_k = 20` observation that is **not** admissible evidence and was cited
as such in an earlier draft: "q11 goes to zero" is the conclusion stratum,
excluded six paragraphs above. Not used here. The other — the temporal
stratum's apparent collapse — was inadmissible for a reason that has since been
half-repaired: it read `0.325 → 0.154` when *neither* query exercised the arm,
and post-re-baseline it reads `0.122 → 0.347`, i.e. `top_k = 20` now *wins* the
stratum. It is admitted to the table above with the two-query / four-label
caveat attached, and it does not decide anything on its own.

## Measured — latency

Interleaved-paired, per this project's standing rule that absolute
cross-session comparison is invalid on this machine (re-benching an identical
commit once returned +1.5 ms on identical bits). **One prebuilt binary**, the
arms alternated off/on within each pair, five pairs, `hybrid_recall_bench`
defaults (2500 nodes, 200 requests, 7 queries, both retrieval arms live, graph
and temporal arms exercised).

| | off (mean of 5) | on, `top_k = 10` (mean of 5) | mean paired Δ | Δ range across pairs | per candidate |
|---|---|---|---|---|---|
| p50 | 6.68 ms | 20.41 ms | **+13.73 ms** | +13.34 … +13.93 | 1.37 ms |
| p95 | 8.84 ms | 40.67 ms | **+31.83 ms** | +30.99 … +32.79 | 3.18 ms |
| p99 | 9.94 ms | 45.39 ms | +35.45 ms | +34.06 … +36.48 | 3.54 ms |

The paired deltas span 0.6 ms (p50) and 1.8 ms (p95) across five pairs, which
is why this discipline is worth the runs: the machine's own drift is larger
than several previously-reported "improvements".

**Both arms pass AC-2 idle.** On, `top_k = 10`: p50 20.4 ms against the 35 ms
gate, p95 40.7 ms against the 60 ms gate; 89.3 % of requests still finish under
35 ms. What changes is the headroom — p50 goes from 28 ms of slack to 15 ms,
p95 from 51 ms to 19 ms.

**The loaded arm could not be measured comparably, and that is the finding.**
`MEMGARDEN_BENCH_LOAD=1` runs a rate-paced background ingest (one batch of 8
every 12 ms) alongside the recall loop, and the harness refuses to report a run
whose loader fell more than 10 % behind its pacer — the gate CE-9b added after
an unpaced comparison flattered one arm by several milliseconds. With the
reranker off the loader hits 100 % of offered every time (1608 / 1600 / 1616
nodes). With `top_k = 10` it lands at **4160 of an offered 4626 (89.9 %)**,
reproducibly, on three consecutive attempts. The cross-encoder's four ONNX
threads take exactly the CPU the ingest path needs, so the two arms are no
longer offering the same load and no honest loaded p95 can be quoted from
them. Recorded rather than worked around: lowering the pacer to force
comparability would also make the run incomparable with CE-7's and CE-9b's
recorded loaded numbers, which is the trend line those gates exist to protect.

The AC-2 assertion is enforced only on the `rerank_top_k = 0` arm, deliberately:
a reranked run is an experiment on a non-default configuration, and turning
"the cross-encoder costs more than its budget" into a red test would destroy
the measurement instead of recording it.

Direct micro-measurement, warm, from `rerank::tests::live_rerank`:
`top_k = 10` → 10.4 ms/call, **1.04 ms/candidate** on short synthetic
documents. The in-situ 1.37 ms (p50) / 3.18 ms (p95) per candidate is the
honest number to quote — real fact text is longer than the micro-benchmark's,
and the recall path pays the ONNX mutex and the scheduler hop on top.

## Diverged from legacy

| # | Divergence | Legacy | Here | Why |
|---|---|---|---|---|
| 1 | **Reranker default off** | on, `cross-encoder/ms-marco-MiniLM-L-6-v2` loaded at startup | `enabled = false` | The *live* legacy daemon also runs it off (`HINDSIGHT_API_RERANKER_PROVIDER=rrf`), adopted as part of its 830 ms → 20 ms fix. Off is parity with the deployed reference; the shipped-default in the upstream source is not what anything actually runs. AC-2's 35 ms p50 is the second reason. |
| 2 | **`top_k = 10`** | `rerank_limit = thinking_budget * 2` = 600 at mid budget (`memory_engine.py:5266`) | 10 | 600 candidates × 1.0–3.2 ms is ~10–30 *seconds*: not supportable at any SLO, so legacy's depth is out regardless. The 10-vs-20 choice is a **latency** decision, not a ranking one — measured above, 20 wins nDCG@10, recall@10 and the identifier guardrail while 10 wins MRR decisively. |
| 3 | Calibrated-score passthrough branch not ported | `reranking.py:304-307` passes `[0, 1]` scores through un-sigmoided | always sigmoid | That branch exists for hosted rerankers (Cohere, Jina, llama.cpp) which MemGarden has no client for. Dead code here, and a dead branch in a scoring path is worse than an absent one. |
| 4 | `max_length` fixed at 512 | configurable | fixed | 512 is the model's own position-embedding limit; above it the model produces garbage rather than an error. A knob whose only legal value is its default is a trap. |
| 5 | Load failure is non-fatal and invisible to `/healthz` | n/a (legacy fails startup) | logs, leaves the slot empty, recall stays on the passthrough | The passthrough is the configuration every other Phase B number was measured against, and an optional ranking refinement being absent is not a degraded memory system. Reporting it as DEGRADED would train the operator to ignore DEGRADED. **`/metrics.json` carries `reranker_loaded`** so "configured on, silently running off" is still observable. |
| 6 | **`top_k` silently overrides `[recall] limit`** | legacy's `rerank_limit` (600) is far *above* `limit`, so it never binds | `top_k = 10` is *below* the default `limit = 20`, so an enabled reranker returns at most 10 results where the caller asked for 20 | Ported faithfully — legacy drops everything past the rerank depth (`memory_engine.py:5266`) and this does the same. But at legacy's depth the truncation is invisible and at ours it is the binding constraint, which makes it a **user-visible behaviour change** that a caller reading `[recall] limit` would not predict. Called out here rather than left as an emergent property of two configs. |
| 7 | **The model is an ONNX re-export, not legacy's checkpoint** | `cross-encoder/ms-marco-MiniLM-L-6-v2` (PyTorch, sentence-transformers) | `Xenova/ms-marco-MiniLM-L-6-v2` (community ONNX export of it) | Different artifact, different org, so "the exact model legacy loads" is an equivalence claim about a re-export. **Now measured rather than asserted** — see below. |
| 8 | Result count changes transiently while the model loads | n/a (legacy blocks startup on the model) | requests served between bind and load return the passthrough's up-to-`limit` results; requests after it return up-to-`top_k` | Follows from divergences 5 and 6 together. Bounded (one load, a few seconds) and it fails toward *more* results, but a caller diffing two responses across that boundary sees a count change with no config change. |
| 9 | AC-2 is not enforced on reranked bench runs | n/a | `hybrid_recall_bench` asserts the p50/p95 gates only when `MEMGARDEN_BENCH_RERANK` is unset | A reranked run is an experiment on a non-default config; turning "the cross-encoder costs more than its budget" into a red test destroys the measurement instead of recording it. The consequence is real and stated: **no automated gate protects the reranked path's latency.** Whoever enables it in production owns re-establishing one. |

### Divergence 7, measured

`Xenova/ms-marco-MiniLM-L-6-v2` is a community re-export, so the port-fidelity
claim needed a number rather than a sentence. Legacy's own stack was run on the
same three query-document pairs (`sentence_transformers.CrossEncoder(...,
device='cpu')`, `activation_fn=Identity`, then the same sigmoid):

| pair | legacy (PyTorch fp32) | MemGarden (fastembed/ONNX) | abs Δ |
|---|---|---|---|
| off-topic (banana) | 1.1982546262499065e-5 | 1.1982534835194829e-5 | 1.1e-11 |
| **on-topic** (wall clock) | 0.9998976474542983 | 0.9998976476495002 | 2.0e-10 |
| off-topic (FTS5) | 1.095726005926585e-5 | 1.0957291407939685e-5 | 3.1e-11 |

Agreement to ~7 significant figures — fp32 noise between two runtimes, the same
standard `embed.rs` holds the embedding model to. `rerank::tests::live_rerank`
now asserts against these recorded legacy values at 1e-6, sized to catch a
*different model* or a broken re-export rather than runtime drift. Reproduce
legacy's side with sentence-transformers on CPU and the raw logits
`[-11.332047462463379, 9.18698501586914, -11.421497344970703]`.

## Known limits and follow-ups

* **The reranker cannot rescue what retrieval missed.** At `top_k = 10`,
  anything RRF ranked 11th or lower is unreachable. Every quality number above
  is a reordering of the same ten candidates. The lever for the *set* is
  retrieval (the arms, the fusion, the over-fetch), not this.
* **The gold labels are still provisional** (`labels_status:
  provisional-pending-user-review`, carried into all three appended results
  records). The deltas are computed against the same labels on both arms, so
  a labelling error largely cancels — but the absolute figures inherit AX-2's
  caveat and should not be quoted without it.
* **Two strata are one or two queries wide, and the temporal one is half
  valid.** q17 now exercises the temporal arm (the Korean-date gap is closed);
  q15 still does not, because its `지난주` window covers 2697/2718 facts — a
  corpus property, not a bug. Its figure has flipped from +0.251 to **−0.192**
  on the strength of that one repaired query, which is a signal to widen the
  gold set rather than a proven property. The identifier stratum (4 queries)
  and memcompare (5) remain the ones to trust.
* **`rerank_latency` is a hard precondition on `enabled = true`, not a
  follow-up.** `/metrics.json` reports whole-recall latency only, so the
  cross-encoder's share is visible only by differencing two offline runs —
  which is exactly the position this project's standing rule about the embedder
  mutex exists to forbid. `enabled = false` makes it non-urgent; it does not
  make it optional. Whoever flips the default owes, in the same change: a
  `rerank_latency` histogram, **and** the same instrumentation on the embedder's
  mutex hold so the two ONNX sessions can be compared rather than guessed at.
* **One session, one mutex.** A concurrent recall waits out the ~15–26 ms of a
  `top_k = 10` batch (`ponytail:` marker at `Reranker::scores`). A second
  session would fix that and would also be a second copy of the model in RAM;
  RAM-first, so not now, and gated on the histogram above.
* **`recall@10` is now *negative* at `top_k = 10`** (−0.056 on the aggregate,
  and negative on memcompare and temporal individually). It was +0.002 before
  the re-baseline. The reranker is a reordering, not a retrieval improvement,
  and on this metric it is a small net loss — do not use recall@10 as this
  PR's success criterion in either direction.
* **The AX-2 baseline itself is `provisional-pending-user-review`**, and every
  post-CE-11 recall delta — this note's and any future one measured against
  these records — inherits that caveat until the corpus owner signs the labels
  off. With the harness now exercised by a real consumer, AX-3 and AX-4 are
  unblocked.

## Recommendation

**Keep the default off, at `top_k = 10`, and record what that costs.**

**Latency decides it, and latency is unconditional.** It needs no choice of
metric, no stratum selection and no aggregation basis:

* +13.73 ms p50 / +31.83 ms p95 on a hook that fires on *every prompt* — 3× the
  p50 and 4.6× the p95 of the passthrough.
* Idle, both arms pass AC-2 (on: p50 20.4 ms, p95 40.7 ms). What is consumed is
  the margin — 28/51 ms of slack becomes 15/19 ms — and that margin is what
  absorbs a slow disk, a concurrent retain, or a bank ten times this size.
* Under load the arms are not even comparable: the cross-encoder starves the
  ingest loop to 89.9 % of offered, reproducibly. A recall path that slows
  ingest is spending someone else's budget.
* The reference system AC-1 compares against runs the passthrough, so enabling
  this by default would make every A/B measure two changes at once.

**Quality says the cross-encoder is worth having as an opt-in, on a narrower
case than this note first recorded.** Post-re-baseline it is **+0.150 MRR** and
**+0.008 nDCG@10** over 13 queries, with the identifier guardrail up from 0.416
to 0.495 — against the +0.257 / +0.077 originally reported. The MRR gain is
still large and still the metric a top-of-block injection lives on, and the
guardrail column is untouched. But recall@10 is now a 0.056 *loss*, and the
aggregate nDCG@10 gain has all but vanished. It ships as a supported opt-in
rather than a rejected experiment; it does not ship as a general quality win,
and this paragraph should not be quoted as one.

**And here is what choosing `top_k = 10` gives up, stated rather than
presented around.** On a consistent 13-query basis the ranking evidence is
*split*, and the re-baseline moved it further toward 20: `top_k = 20` wins
nDCG@10 (0.4121 vs 0.3474), recall@10 (0.435 vs 0.347), recall@5, memcompare,
temporal, and the identifier guardrail by **+0.100 nDCG@10** — more than the
+0.079 that `top_k = 10` gains over baseline on that same column. `top_k = 10`
wins MRR by 0.082, recall@1 and graph. Two earlier drafts of this note have now
been wrong about this comparison in the same direction — first by mixing a
13-query table with a 14-query one and citing the excluded conclusion stratum,
now by resting on a baseline in which q17's temporal constraint never fired.
The claim that survives both corrections is the unconditional one: **latency
chooses 10**, and the ranking bill for that is real and has grown.

10 is still right, because latency already settled it and MRR is the better
tie-break for this consumer: recalled memories are prepended in rank order and
the client model reads down, so the first relevant item at rank 1 instead of
rank 3 is worth more than an extra relevant item at rank 9. But the guardrail
column is a real cost, knowingly paid. **If a future caller's budget absorbs
+14 ms p50 / +32 ms p95, `top_k = 20` is the better ranking configuration and
this note should not be read as saying otherwise.**

So: off in `config.example.toml` and in `Config::defaults`, `top_k = 10` as the
supported depth with the latency table in the config comments, `rerank_latency`
instrumentation as a hard precondition on ever flipping the default, and a
latency-tolerant caller (batch consolidation recall, a future `/reflect`) as
the natural first place to turn it on — a per-call-site decision Phase C can
now make from numbers instead of taste.
