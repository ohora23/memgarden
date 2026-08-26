# Performance and recall quality — every measured number

Split out of the README on 2026-08-27. The README keeps a summary table
and links here; this file is the unabridged record.


Every number here is measured on this machine (Ryzen 7 9800X3D, 16 threads, release build) and traceable to a design note in `docs/design/`. Two caveats travel with them, because the notes insist on it: **absolute latencies on this box drift ±1.5ms between runs of identical bits**, so paired deltas are the trustworthy figures; and the recall-quality labels are **provisional on 19 of 20 gold queries**.

### Recall — the budget that matters

| | p50 | p95 | p99 | conditions |
|---|---|---|---|---|
| **idle** | **7.1ms** | **7.8ms** | 8.7ms | 3,000 nodes, 2,000 requests, all four arms (BM25 + vector + graph + temporal) |
| **under concurrent ingest** | 19.6ms | 48.8ms | — | same, while a background loader writes ~35,700 nodes into the same bank |
| budget (AC-2) | ≤35ms | ≤60ms | | 1,605/2,000 under 35ms and 1,997/2,000 under 60ms in the loaded run |

Per-arm, isolated: graph 0.29ms p50, temporal 0.13ms p50 (0.54ms worst case), mental-model KNN 0.20ms p50 at 1,000 models. The scaling ceiling is the brute-force vector scan — whole-recall p95 is 9.7ms at 3k nodes and 40.7ms at ~32k, which puts the upgrade point somewhere near 50k.

Through the hook, against a live daemon and the 2,718-fact gold bank: **p50 7.96ms / p95 8.91ms / p99 9.49ms** end to end, on a Korean query — 7× inside the 70ms gate.

### Hooks — 0.85ms of the 10ms allowance

Interleaved-paired against the same binary doing nothing, N=300 per arm:

| | `session-start` | `recall` | `retain` (gated turn) | `session-end` |
|---|---|---|---|---|
| p50 | 0.549ms | 0.465ms | 0.380ms | 0.361ms |
| p95 | 0.624ms | 0.526ms | 0.435ms | 0.416ms |
| paired p50 (own work) | 0.255ms | 0.183ms | 0.102ms | 0.084ms |

**A whole turn is `recall` + `retain` = 0.845ms p50 / 0.961ms p95.** The comparison that matters is not the budget but the system this replaces: the legacy Python hooks cost **33ms on their disabled path** — more to do nothing than these cost to work. An equivalent Python hook measured 24ms cold, so AC-2's <10ms was never reachable in that language.

Two declared exceptions: the **first retain of a session** sends the whole transcript (68.6ms on a 21.9MB file, once per session, which is why the `Stop` entry is `async`), and a **hung daemon** costs 1.5s on the first prompt before the circuit breaker takes over.

The binary is 1.58MB, links only glibc/libgcc/vdso, and its dependency closure is CI-enforced — no tokio, no SQLite, no ONNX in a process that runs thousands of times per session.

### Ingest and extraction

| | measured | conditions |
|---|---|---|
| embedding, single | 2.41ms p50 / 3.39ms p95 | bge-small, 384-dim, fp32 ONNX, CPU |
| embedding, corpus | 26.2s for 2,718 nodes | real drain worker including its KNN pass |
| legacy migration, whole corpus | 207s for 5,288 nodes | dev build; snapshot 1.6s, then documents, facts, entities, observations, links and re-embedding, four banks |
| transcript delta read | 0.46ms for a 200KB tail; 64.3ms to parse 106.9MB | byte-offset resume, so the common case never re-reads |
| input-cap savings | **−75.3%** live, **−86.9%** over a 5.8MB transcript | written to the benefit ledger on every retain |
| consolidation round | 151s for 50 facts | real Ollama qwen3-14b-nothink, ~50% duty cycle against a 300s interval |
| reflect | 1.70s warm, 6.21s cold | 3 memories in the payload |

Extraction wall time is **deliberately not quoted**: the one live measurement (564s for three chunks) ran on a GPU shared with the legacy daemon and against a pathological fixture. It needs re-measuring on an idle card before it means anything.

### Cost of measuring

One `record` call is **74.3ns**; the full set a `POST /recall` touches — four counters and two histograms — is **87.7ns per request**. That is 0.00025% of the 35ms SLO, which is what makes "zero added latency on the hot path" an acceptance criterion rather than a hope.

### Recall quality

Against a frozen 2,718-fact corpus with 20 graded queries and 331 judgments, macro-averaged, Burges/TREC nDCG. That corpus is a real memory bank and is not in this repository, so these are **our** before/after record rather than a benchmark to compare against — recall@k is a property of the corpus and its labels ([`gold/README.md`](../gold/README.md)):

| | recall@1 | recall@5 | recall@10 | MRR | nDCG@10 |
|---|---|---|---|---|---|
| shipped (RRF, no reranker) | 0.038 | 0.243 | **0.403** | 0.551 | 0.340 |
| with the reranker, top_k=10 | 0.047 | 0.258 | 0.358 | **0.701** | 0.352 |

The reranker wins ordering (+0.150 MRR) and loses coverage (−0.044 recall@10) for +13.7ms p50 / +31.8ms p95, and it drops background ingest to 89.9% of offered load. That is why it ships **off** — with a written re-entry criterion rather than a verdict.

**A caveat these numbers inherited, found during Phase D and since fixed.** Semantic links only
ever formed between nodes embedded in the *same* batch of 8: `embed_task.rs` built its
`fact_type` lookup from the just-embedded batch, so `semantic_links` dropped every neighbour
outside it and the KNN's other 99 candidates were discarded. The filter meant to select on
fact type was selecting on batch membership.

Fixed on 2026-08-09 — the lookup now covers the batch **and** its neighbours. Re-importing the
same corpus moves semantic edges **6,918 → 62,199** (0.11× → 0.96× of legacy's 65,149) and
out-degree from max 7, which was `batch_size - 1`, to max 20, which is `SEMANTIC_LINK_TOP_K`.

**Re-measured on the fixed graph, and the density bought nothing.** The gold corpus was rebuilt
through the same worker: **681 semantic edges → 43,830**, a 64× change, with out-degree going
from mean 1.24 / max 3 to mean 16.6 / max 20. Recall moved the wrong way:

| | recall@10 | MRR | nDCG@10 |
|---|---|---|---|
| thin graph (ledger line 8) | 0.3881 | 0.5221 | 0.3236 |
| 9× denser (line 11) | **0.3792** | **0.5162** | **0.3168** |
| relinked, +58% denser again (line 12) | 0.3792 | 0.5162 | 0.3168 |

Per stratum, nothing improved: `memcompare` recall@10 −0.025, `graph` nDCG −0.025, and
`identifier`, `conclusion` and `temporal` unmoved to three decimals. The ceiling is unchanged at
0.8588, as it must be — the labels never moved.

**Line 12 is not a rounding coincidence.** The fix only reaches nodes embedded after it, so
[`POST /v1/banks/{id}/relink`](design/ce-7-entity-graph.md) re-runs the pass over a settled
bank; on line 11's own database it added 25,250 edges in 2.4s (43,830 → 69,080, out-degree mean
16.61 → 25.53, max 20 → 40) and every aggregate came back **identical to the last floating-point
digit**. The only field that moved anywhere in the record is q05's retrieved list, which reordered
without changing a metric. Of 400 pooled candidates, 8 were replaced — and not because the new
edges are weak, since their mean weight is 0.781 against the existing 0.7669. They never reach the
fused top-20: the graph arm is already saturated against its 200-node expansion cap, so a denser
graph feeds it more of what it was already discarding.

The honest reading is *no measurable gain*, not *a regression*: −0.9 points of recall@10 over 14
scored queries is inside what a set this size can resolve, and the second experiment moved nothing
at all. Two independent density changes now agree, so the assumption that these numbers were held
down by the thin graph is retired. The fix and the repair stand on the code having done something
other than what it said, not on a recall win neither delivered.
