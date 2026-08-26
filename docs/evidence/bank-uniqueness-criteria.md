# What the bank holds that disk does not — rules, fixed before any node was drawn

Committed before the sample was drawn. The commit timestamp is what makes that
checkable, which is the only reason this is separate from the result.

## The question, and why it is this one

[MX-3](mx-3-result.md) asked whether injected memory substitutes for work, and
could not answer: its mechanical draw of real user prompts came out **24 of 27
in-repo**, so it measured "when the answer is written down anyway, does memory
help?" — and found it slightly worse and faster. The question it was built for,
*"when the answer exists only because someone said it, does the bank recover
it?"*, had a sample of **one**.

This measures the same thing from the corpus side instead of the query side.
Not "do the questions people ask need the bank", but **"does the bank contain
anything disk does not"**. A query sample can be unlucky; a corpus census
cannot be, because the corpus is the thing under test.

## Population and sample

* **Population**: the `claude-code::memgarden` bank, 1,562 nodes — the one bank
  whose subject matter has a repository to be compared against.
* **Sample**: 60 nodes, `random.seed(17)`, drawn before any is read.

## What counts as "on disk"

Four sources, all things a competent engineer would actually search:

1. the repository worktree at `HEAD` (tracked files only);
2. `git log --all --format=%B` — every commit message in the history;
3. the Obsidian vault's MemGarden project folder;
4. the native `MEMORY.md` store under `~/.claude/projects/<project>/memory/` — the
   curated native memory.

**Raw session transcripts are deliberately excluded.** They are MemGarden's
input, not a place anyone reads: the four of them in this project total ~33 MB
of JSONL. Counting them would make every memory trivially "on disk" and the
measurement meaningless.

## The test, and the direction its error runs

From each node's text, distinctive terms are extracted:

* numbers of 3+ digits or containing a decimal point;
* `` `backtick-quoted` `` identifiers;
* `snake_case` / `CamelCase` tokens of 6+ characters;
* ALLCAPS tokens of 4+ characters.

A node is **on-disk** if **any single one** of its distinctive terms appears in
any source. A node is a **memory-only candidate** only if **none** do.

That test is deliberately lopsided. One shared identifier — `retain`, `9100`,
`CE-7` — marks a node as on-disk even when the *claim* it makes appears
nowhere. So the on-disk count is inflated and the memory-only count is a
**lower bound**. If this measurement is going to be wrong, it will be wrong
against the conclusion its author would prefer.

Nodes yielding **no** distinctive terms are reported as their own bucket and
excluded from both, rather than silently counted as memory-only.

## Then it is read

Every memory-only candidate is read and classified:

| label | meaning |
|---|---|
| **unique knowledge** | states something a reader could not get from the four sources |
| **trivial** | true but carries no usable content |
| **misclassified** | the claim *is* on disk; the term extractor simply missed it |

The headline is the **unique knowledge** count, not the candidate count.

## What would decide it

* **A material share is unique knowledge** → the bank holds things nothing else
  does, which is the value MX-3 could not sample, and it is worth saying so.
* **Almost none is** → MemGarden's value on this machine is speed and recall
  quality, not access, exactly as MX-3's result suggested. That is a real
  finding too and gets reported the same way.

There is no outcome this file prefers.

## Limits, stated in advance

1. **"On disk" is not "would have been found".** A fact in commit 40 of a
   history nobody greps is on disk and practically invisible. This bounds the
   claim; it does not model retrieval.
2. **One bank, one project, one operator.** 1,562 nodes of 7,172.
3. **n = 60.** A 10% result carries roughly ±8 points at 95%, and the number is
   reported with that width rather than as a point estimate.
4. **The author built the system.** The sample is seeded and drawn first, the
   test is mechanical and biased against the interesting answer, and the
   classification of every candidate is published. That is the mitigation, not
   a proof of neutrality.
