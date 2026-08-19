# AC-1 — memcompare, 20 queries to both systems

Run 2026-08-12 against the live databases: MemGarden on `:9100`, legacy
hindsight on `:9077`, same query, same knobs (`budget=low`,
`max_tokens=512`, all three fact types — what both hooks send under the
`coding` profile).

Criteria were fixed and committed **before** any query was sent
(`ac-1-criteria.md`, commit `57f3ff9`). Raw responses and per-item reasoning
are kept out of the repository under the data policy; the verdict and its
one-line reason for all 20 are below.

## Result

| | |
|---|---|
| better | **6** |
| equivalent | 2 |
| worse | **5** |
| unjudgeable (both scored 0 hits) | 7 |

**Judgeable: 13. The gate condition (`worse ≤ better`) holds, 6 to 5.** It
holds by one query, and that is the honest headline: this is not a
comfortable margin.

Latency, same 20 queries: **MemGarden p50 11.5 ms, legacy p50 51.0 ms.**

### By query source

Queries came from three sources so that the judge did not pick the topics.

| source | better | equiv | worse | unjudgeable |
|---|---|---|---|---|
| A/B log (the 5 distinct queries from `memory_comparison_log.md`) | 2 | 0 | 2 | 1 |
| shadow prompts (6 real user questions, verbatim) | 0 | 0 | 1 | **5** |
| entity-derived (9, from the bank's most-mentioned entities, one template) | 4 | 2 | 2 | 1 |

**The shadow-prompt source produced almost nothing.** Five of six were
unjudgeable because a real prompt refers to the conversation around it —
"각 옵션의 장단점을 설명해줘", "자동화가 안된다는게 무슨말이야", "이게
맞나?" have no referent when replayed as a standalone query. That is a
finding about the method, not about either system: **recall records lifted
from a live session are not a query set.**

## The five `worse` queries, quoted as the criteria require

1. **"agentmemory 감사에서 발견된 진짜 원인이 뭐였지?"** — hits 2 to 2, but
   MemGarden spent four slots on `systemd` service configuration, a file
   search, and a `doctor --dry-run` invocation. Legacy spent one.
2. **"업스트림 PR은 최종적으로 어떻게 처리했나"** — legacy 3 hits to 1. It
   had the PR's actual state and the fork-sync policy; MemGarden returned its
   own project's CE-1..MX-1 PR stack instead.
3. **"개인 정보 관련된 부분은 지금 처리해줄 수 있지 않나?"** — legacy found
   that the repository was deleted and recreated to strip personal data
   before publication. All seven of MemGarden's items were about the graph
   UI, 3D and `RecallItem`.
4. **"vectorize-io/hindsight 관련해서 어떤 문제가 있었고…"** — both drowned
   in PR-monitoring log lines; legacy carried one item of substance more.
5. **"agentmemory 관련해서 어떤 문제가 있었고…"** — legacy 6 hits to 3, and
   six of MemGarden's twelve items were records of commands having been run.

**The pattern is one thing, not five.** MemGarden ranks *"a command was
executed"* records too highly. Every `worse` verdict except #2 turns on
that, and #2 turns on topic drift toward this project's own history. Legacy
wins where the answer is an operational fact buried among tool-call records,
because it surfaces fewer of those records.

That is a ranking problem with a specific shape, and it is actionable:
these items are recognisable (they are `world` facts whose text describes an
assistant action rather than a fact about the system). Nothing in this run
suggests a retrieval-mechanism defect.

## What went the other way

The six `better` verdicts were not close. MemGarden alone found: Ollama's
`/api/chat` ignoring the JSON schema and the retry defence built for it; the
embedding query going 0.38 s → 4.5 ms on a forced CPU switch; the reason the
upstream PR was withdrawn (20 hours of maintainer silence) where legacy
returned the same monitoring sentence six times; `sqlite-vec` pinned to 0.1.9
for a broken 0.1.10 package.

## Duplication, again

`#1` returned the same sentence four times from MemGarden, and `#19` returned
one legacy sentence six times. Neither is the `world`/`observation`
restatement the recall dedupe now removes — these are **independent nodes
carrying near-identical text**, from repeated retains of a recurring event.
The provenance-based dedupe cannot see them, by design: nothing recorded that
one was built from the other. Both systems have this; it is not a parity gap
and it is not fixed here.

## Limits — read these with the result

1. **The judge built the system.** The PRD assigns AC-1 to the user; the user
   delegated it on 2026-08-12. Delegation does not remove the conflict, so
   this is a **recommendation** and the gate signature remains the user's.
   The criteria were published first and every verdict carries its reason so
   a sampled re-read can contradict it.
2. **13 judgeable queries is a thin sample** for a 6-to-5 margin. One query
   re-read differently flips the verdict.
3. **The corpus-scope question from `ac-1-shadow.md` is still open** and this
   run does not touch it: conclusion-type questions are answered in the
   curated `MEMORY.md`, which neither system's corpus contains.

## Recommendation

Do not sign AC-1 on this run alone. Two things would make the margin mean
something, in order of cost:

* **Fix the ranking of action records.** It is the single cause behind four
  of the five `worse` verdicts, and it is not a rewrite.
* **Then re-run these 20 queries.** The set, the criteria and the harness are
  fixed, so the re-run is a comparison rather than a new judgement.
