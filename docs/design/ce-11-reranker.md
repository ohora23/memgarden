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
| `crates/memgardend/src/rerank.rs` | `Reranker { inner: Mutex<TextRerank> }`, `sigmoid`, `decorate`, `top_k_warning`, `load_at_startup`. |
| `crates/memgarden-core/src/config.rs` | `[reranker]` — `enabled`, `model`, `top_k`, `threads`, `batch_size`, with validation. |
| `crates/memgardend/src/recall/mod.rs` | The one branch: with the cross-encoder on, its sigmoid-normalised logit replaces `scoring::passthrough_base`. |
| `crates/memgardend/src/bin/recall_bench.rs` | `rerank=<top_k>` — the only measurement knob the harness exposes. |
| `crates/memgardend/tests/recall_api.rs` | `MEMGARDEN_BENCH_RERANK=<top_k>` on the latency bench; the parity test; the live end-to-end test. |
| `gold/results.jsonl` | Three appended runs: the reproduced baseline, `top_k = 10`, `top_k = 20`. |

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

### Per stratum, nDCG@10 (the metric duplication does not deflate)

| stratum | queries | baseline | `top_k = 10` | Δ |
|---|---|---|---|---|
| identifier / proper noun | 4 | 0.416 | **0.495** | **+0.079** |
| memcompare | 5 | 0.203 | 0.240 | +0.037 |
| graph | 2 | 0.549 | 0.546 | −0.003 |
| temporal | 2 | 0.074 | 0.325 | +0.251 |
| conclusion | 1 | — | — | **excluded, see below** |

MRR, same arms:

| stratum | baseline | `top_k = 10` | Δ |
|---|---|---|---|
| identifier | 0.688 | **0.875** | +0.187 |
| memcompare | 0.383 | 0.600 | +0.217 |
| graph | 0.750 | 1.000 | +0.250 |
| temporal | 0.050 | 0.556 | +0.506 |

### Aggregate, conclusion stratum excluded (13 scored queries)

| metric | baseline | `top_k = 10` | Δ |
|---|---|---|---|
| recall@1 | 0.022 | 0.066 | +0.044 |
| recall@5 | 0.197 | 0.247 | +0.050 |
| recall@10 | 0.345 | 0.347 | +0.002 |
| MRR | 0.482 | **0.739** | **+0.257** |
| nDCG@10 | 0.302 | **0.379** | **+0.077** |

For reference, the 14-query figure the harness prints (which folds the
conclusion stratum in) is `0.021/0.183/0.335/0.458/0.289` → `0.061/0.229/0.336/0.697/0.360`.

**The conclusion stratum is excluded, not averaged in.** AX-2 established that
it is structurally unmeasurable against this corpus: three of its four queries
have no answer in the corpus at all, and q11 has 23 labels of which zero are
grade 2, because curated conclusions live in native `MEMORY.md` which this
export does not cover. Its number happens to be identical in both arms (0.113),
so nothing is being hidden — but moving one query with no core answer in reach
is noise with a decimal point on it, and it does not belong in a delta.

### The identifier guardrail: passed, and it is the strongest column

The instruction was explicit that a reranker which raises the aggregate while
regressing the proper-noun stratum is a failure, not a trade. It does not
regress: identifier nDCG@10 goes **0.416 → 0.495** and MRR **0.688 → 0.875**,
the second-largest gain of any stratum. The evidence that the floorless hybrid
was right — jcode's argument that a hard cosine floor zeroes recall on
identifier-heavy agent memory — is *strengthened*, not traded away. The lexical
arm still surfaces the proper-noun matches; the cross-encoder then puts the
right one first.

`recall@10` is essentially flat (+0.002) and that is expected, not a
disappointment: at `top_k = 10` the reranker reorders the RRF top-10 and drops
the rest, so the *set* inside the measurement window is nearly the baseline's.
(It moves at all only because the baseline's top-10 is ordered by
`passthrough_base × boosts`, not by raw RRF, so the ±21 % boost envelope can
swap an item across the rank-10 boundary.) **Read MRR and nDCG@10.** Those are
where a pure reordering can show up, and they move a lot.

### `top_k = 20`, a secondary data point

| | recall@1 | recall@5 | recall@10 | MRR | nDCG@10 |
|---|---|---|---|---|---|
| baseline (14q) | 0.021 | 0.183 | 0.335 | 0.458 | 0.289 |
| `top_k = 10` (14q) | 0.061 | 0.229 | 0.336 | **0.697** | **0.360** |
| `top_k = 20` (14q) | 0.035 | 0.219 | 0.368 | 0.563 | 0.355 |

Deeper is not better. `top_k = 20` buys real recall@10 (+0.033 over `top_k =
10`, since twice as many candidates are reachable) but loses MRR (0.697 →
0.563) and is flat on nDCG, because the extra candidates give the cross-encoder
twenty near-duplicates to be confidently wrong about — the temporal stratum
collapses from 0.325 to 0.154 and q11 goes to zero. At double the latency, that
is a bad trade. **`top_k = 10` stays the supported depth.**

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
| 2 | **`top_k = 10`** | `rerank_limit = thinking_budget * 2` = 600 at mid budget (`memory_engine.py:5266`) | 10 | 600 candidates × 1.5–2.6 ms is ~15–25 *seconds*. Not a supportable number on this hardware at any SLO. Measured above: 20 is already worse on MRR and nDCG than 10, so this is not merely a latency compromise — 10 is also the better ranking. |
| 3 | Calibrated-score passthrough branch not ported | `reranking.py:304-307` passes `[0, 1]` scores through un-sigmoided | always sigmoid | That branch exists for hosted rerankers (Cohere, Jina, llama.cpp) which MemGarden has no client for. Dead code here, and a dead branch in a scoring path is worse than an absent one. |
| 4 | `max_length` fixed at 512 | configurable | fixed | 512 is the model's own position-embedding limit; above it the model produces garbage rather than an error. A knob whose only legal value is its default is a trap. |
| 5 | Load failure is non-fatal and invisible to `/healthz` | n/a (legacy fails startup) | logs, leaves the slot empty, recall stays on the passthrough | The passthrough is the configuration every other Phase B number was measured against, and an optional ranking refinement being absent is not a degraded memory system. Reporting it as DEGRADED would train the operator to ignore DEGRADED. |

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
* **Two strata are one or two queries wide.** The temporal stratum's +0.251 is
  two queries, one of which (q17) went from 0.149 to 0.515 on its own. It is
  the largest-looking gain in the table and the least robust. The identifier
  stratum (4 queries) and memcompare (5) are the ones to trust.
* **No metric for rerank cost.** `/metrics.json` reports whole-recall latency;
  the cross-encoder's share is only visible by differencing two runs. If the
  reranker is ever enabled in production, a `rerank_latency` histogram should
  land with it.
* **One session, one mutex.** A concurrent recall waits out the ~15–26 ms of a
  `top_k = 10` batch. A second session would fix that and would also be a
  second copy of the model in RAM; RAM-first, so not now.
* **`recall@10` cannot be moved by this PR** and should not be used as its
  success criterion by a future reader skimming the table.

## Recommendation

**Keep the default off, and record that the opt-in now has evidence behind it.**

The measurement says the cross-encoder earns its cost on quality — +0.077
nDCG@10 and +0.257 MRR over 13 queries, with the identifier guardrail moving
*up* rather than being traded away. That is a substantial reordering win and
it is not one the boosts or the fusion could have produced.

It does not earn it on latency for the default caller. Idle, it fits — p50
20.4 ms and p95 40.7 ms are both inside AC-2 — but it triples the p50 and
quadruples the p95 of a hook that fires on *every prompt*, and it consumes
enough CPU that the machine can no longer sustain the background ingest rate
the passthrough sustains (89.9 % of offered load, reproducibly). The AC-2
margin that absorbs a slow disk, a concurrent retain or a busier bank is where
that 32 ms comes from. And the reference system AC-1 compares against does not
pay this cost either, so enabling it by default would make every A/B measure
two changes at once.

So: off in `config.example.toml` and in `Config::defaults`, documented as a
supported opt-in with both tables in the config comments, and `top_k = 10`
fixed as the depth — because 20 is worse on ranking *and* twice the cost. A
caller that is latency-tolerant (a batch consolidation recall, a future
`/reflect`) is the natural first place to turn it on, and that is a
per-call-site decision Phase C can make with these numbers in hand.
