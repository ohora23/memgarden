# A multilingual cross-encoder: better ranking, ten times the latency

2026-11-18. After the switch to multilingual-e5-small, nDCG@10 rose and MRR
fell slightly ([multilingual-embeddings](multilingual-embeddings.md)). The
shipped reranker (`Xenova/ms-marco-MiniLM-L-6-v2`, CE-11) is English-only and
off. This note measures a multilingual one before deciding anything.

## Quality — frozen gold set, every top-10 result labelled

20 graded queries (14 answerable), production weights. Rerankers reorder the
RRF top-k. Reranking promotes results from ranks 11–20 that had never been
graded, so the 55 such results were graded on the existing 0/1/2 scale first
(local-model draft, every item reviewed by a second reader, 7 of 55 changed);
after that no arm has an unlabelled result in its top 10.

| arm | recall@10 | MRR | nDCG@10 |
|---|---|---|---|
| no reranker | 0.384 | 0.696 | 0.431 |
| ms-marco-MiniLM-L-6 (English), top 10 | 0.328 | 0.737 | 0.393 |
| ms-marco-MiniLM-L-6 (English), top 20 | 0.372 | 0.729 | 0.430 |
| mmarco-mMiniLMv2-L12-H384 (multilingual), top 10 | 0.328 | 0.735 | 0.403 |
| **mmarco-mMiniLMv2-L12-H384 (multilingual), top 20** | **0.435** | **0.788** | **0.473** |

Per query, the multilingual top-20 arm against no reranker: nDCG@10 better on
**13 of 14** (sign test p = 0.002), recall@10 9 better / 2 worse (p = 0.07),
MRR 5 / 1 (p = 0.22). The English model at top 20 is a wash (nDCG 7 / 6).

The top-10 arms lose recall@10 because a reranker given ten candidates
returns them in a new order, and the result list the harness scores is
re-fitted after it; only the wider pool lets the reranker *bring in* a
better result.

## Latency — `hybrid_recall_bench`, 5,000 nodes, 500 requests, cores 4-7

| arm | p50 | p95 |
|---|---|---|
| no reranker | 11.2 ms | 14.2 ms |
| English, top 20 | 56.1 ms | 102.5 ms |
| multilingual, top 20 | **102.8 ms** | **115.8 ms** |
| multilingual, top 10 | 56.3 ms | 65.3 ms |

## Decision

**Held for the hook path.** The configuration that improves ranking costs
~90 ms per prompt and breaks the 70 ms p95 gate the hook is built to; the one
that fits the gate does not improve ranking.

What would change it:

- a quantised or smaller multilingual cross-encoder measured at ≤ 25 ms for
  20 candidates on this CPU with the nDCG gain intact;
- running it on the GPU, which the local LLM currently owns;
- a recall path that is not on the prompt — the MCP/pull surface or
  `/reflect` — where 100 ms is affordable. That is the first place to use it.

The gold set is small (14 answerable queries), its labels are partly
provisional, and the new grades were not blind to which arm returned each
result; the 13-of-14 sign result is the robust part.
