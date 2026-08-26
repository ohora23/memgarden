# What the bank holds that disk does not — the answer is "almost nothing"

Run 2026-08-27 against the rules committed in
[`bank-uniqueness-criteria.md`](bank-uniqueness-criteria.md) before the sample
was drawn.

**Headline: 0 of 60 sampled memories state something that is not already on
disk.** By the rule of three that puts the memory-only share of this bank at
**under ~5%** with 95% confidence, with a point estimate of zero.

That is a real answer to the question [MX-3](mx-3-result.md) could not sample,
and it points the same way MX-3's result did.

## The counts

| verdict | n | |
|---|---|---|
| **on-disk** — a distinctive term appears in the repo, git log, vault or curated memory | **42** | 70% |
| **no distinctive terms** — the mechanical test could not run | **16** | 27% |
| **memory-only candidate** | **2** | 3% |
| **unique knowledge, after reading every candidate** | **0** | **0%** |

Both candidates and all sixteen untested nodes were read and checked by hand
against the sources. Every one had its subject matter on disk:

| node | claim | found on disk |
|---|---|---|
| 7147 | the `regex-automata` Lazy DFA panic text | `regex-automata`, `lazy dfa`, `unsafe precondition` |
| 7213 | recall fans out to semantic / bm25 / graph / temporal, RRF-merged then reranked | `rrf`, `bm25`, `temporal`, `재랭킹` |
| 7165 | "leading untested lead is thermal" | `thermal`, `온도` |
| 7775 | end status enumerated as strings twice; ask the type instead | `end_reason`, `jobstatus` |
| 7049 | 27 test binaries at 32 threads reproduces it | `27 test binaries`, `32 threads` |
| 8199 | the 9800X3D's CPU 3 and its SMT sibling CPU 11 | `9800x3d`, `cpu 3`, `cpu11`, `smt` |

## The instrument was wrong once, in the flattering direction

The first run returned **3** candidates. One of them, node 7388, cites
`docs/evidence/ac-1-criteria.md` — a file that exists in this repository. The
term extractor had taken the whole backtick-quoted path as a single token, and
that exact literal does not appear in the sources even though `ac-1-criteria`
does.

So the extractor now also emits sub-tokens split on `/`, `.` and whitespace.
Re-run on the **identical seeded draw**, one node moved — 7388, from
memory-only to on-disk — and nothing else.

Recording it because the bug ran **toward** the more interesting answer. An
instrument that inflates the number its author would rather see is the exact
failure this repository has retracted four times, and it was caught here only
because the one suspicious candidate was checked against the repository instead
of being counted.

## The gap the criteria did not anticipate

**27% of the sample carried no distinctive terms at all.** The extractor keys on
numbers, identifiers, `snake_case`, `CamelCase` and ALLCAPS — all of which are
English-and-code shapes. Korean prose statements like *"단순 작업 지시형 프롬프트
40건이 회수 품질이 실제로 갈림"* yield nothing for it to search.

The criteria said such nodes are reported as their own bucket and excluded from
both. That is honoured for the mechanical counts, and then all sixteen were read
anyway, because discarding a quarter of a 60-node sample would be discarding
whatever it was that made them unlike the rest. None turned out to be unique.

**So the mechanical test covered 44 of 60 nodes and a human covered the other
16.** The headline is a hand-verified zero, not a machine-verified one.

## What this means, with the other measurements beside it

Three independent measurements now agree, and the agreement is more informative
than any of them alone:

| measurement | says |
|---|---|
| [MX-3](mx-3-result.md), Layer 3 substitution | on in-repo questions the memory arm was **11–7 worse** on a blind panel and spent **+5% tokens** — but **−25% wall clock** |
| this, corpus census | **~0%** of the bank is knowledge that exists nowhere else |
| the session of 2026-08-26 | the memory that broke a month-old investigation open was a fact **already in `book/src/roadmap.md`** — retrieved from a bank, not from a file anyone would have grepped |

**MemGarden on this machine is an index, not an archive.** It does not hold
things the disk lost. It surfaces things the disk has and nobody would think to
look for, and it does that about seven milliseconds after you press enter.

That is a narrower claim than "a memory system remembers what you would forget",
and it is the one the evidence supports. It also explains MX-3's shape exactly:
if the answer is on disk either way, memory cannot beat reading on *quality* —
only on *time to find it*, which is where the 25% went.

## What this does not say

1. **Not "the bank is worthless".** An index over 7,172 facts that answers in
   7ms is worth having; this measures what kind of value it is, not whether
   there is any.
2. **Not "nothing is ever memory-only".** 0 of 60 bounds the share, it does not
   prove the set is empty. A decision made aloud and never written down would
   land in exactly that bucket, and this operator writes almost everything down
   — [the vault, the docs and the commit messages](command-log-pollution.md) are
   the reason the number is what it is.
3. **Not generalisable.** One bank of 1,562 nodes, one project, one operator who
   keeps an Obsidian vault. A team that decides things in chat and documents
   little would likely measure differently, and that is the interesting
   follow-up rather than a caveat to wave away.
4. **"On disk" is not "would have been found."** A fact in commit 40 of a
   history nobody greps counts as on-disk here. That is the whole point of the
   third row in the table above: on-disk and findable are different properties,
   and the gap between them is where this system earns its keep.
