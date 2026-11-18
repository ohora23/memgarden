# Why live recall latency doubled — it is the query, not the bank

2026-11-18. The whitepaper recorded live recall at ~26 ms p50 after 2026-10-16
against ~12.6 ms before, with two unmeasured candidates: banks growing, and the
cross-bank fallback (#76). Neither is it.

## Replay

127 real prompts from the last 21 days, each sent to the live daemon for its
own project bank (one warm-up, then once with `fallback: false` and once with
the fallback allowed). Client-side wall time, pinned to cores 4-7.

| | n | fallback off p50 / p95 | fallback allowed p50 / p95 |
|---|---|---|---|
| all | 127 | 24.6 / 46.7 ms | 25.4 / 49.8 ms |
| home bank returned nothing | 5 (4%) | 6.2 / 6.5 ms | 46.2 / 48.7 ms |
| home bank returned something | 122 | 25.7 / 46.7 ms | 24.5 / 48.2 ms |

The fallback runs on 4% of prompts and costs ~40 ms each time: about 1.6 ms
on the mean, nothing on the median.

## Bank size vs query length

Same daemon, 30 repetitions after 3 warm-ups:

| bank nodes | 13-char query p50 | 768-char query p50 |
|---|---|---|
| 1,131 | 9.4 ms | 56.7 ms |
| 4,836 | 8.9 ms | 54.0 ms |
| 6,925 | 9.9 ms | 53.7 ms |

At today's bank sizes the bank does not matter; the query does. Growing a
non-repeating query word by word on one bank:

| words | chars | p50 |
|---|---|---|
| 3 | 12 | 9.9 ms |
| 12 | 52 | 13.7 ms |
| 25 | 120 | 16.1 ms |
| 50 | 259 | 24.0 ms |
| 100 | 510 | 39.7 ms |
| 200 | 1,134 | 63.3 ms |
| 400 | 2,392 | 63.1 ms |

Linear at ~0.05 ms per character until it flattens near 1,100 characters,
which is where the embedder's 512-token window truncates the input. The lexical
arm caps its terms at 12, so the growth past a dozen words is the query
embedding.

## Cause

**Longer prompts.** Machine-sent prompts — background-task notifications and
peer-session reports, which became common in this period — are long: in the
replay their median was 26.9 ms against 14.1 ms for prompts a person typed.
The hook truncates every query at `recall_max_query_chars = 800`; by the
word-count table that is ~50 ms of embedding on top of ~10 ms of retrieval,
about 55–60 ms at the median — close to the 70 ms p95 gate rather than
comfortably inside it.

Two consequences:

- Skipping machine-sent prompts (`[hooks] skip_machine_prompts`) should remove
  the slower half of the median along with 40% of the injected tokens — an
  estimate from the replay's split, not a re-measurement after the change.
- If latency ever needs more, embedding only the first ~300 characters of the
  query would bound it near 25 ms. That changes what the semantic arm sees and
  is not done without a quality measurement.

The brute-force vector scan is still linear in bank size (~1.3 µs per node,
whitepaper §3.3); it becomes the dominant term only past ~20k nodes in one
bank.
