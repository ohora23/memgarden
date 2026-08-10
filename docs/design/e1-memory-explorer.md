# E1 — the memory explorer (Phase E, first slice)

What Phase E builds first, why the shape is what it is, and the two PRD
statements it deliberately contradicts.

Phase E's scope in the PRD is "DB-1 dashboard → GV-1 graph API → GV-2 WebGL
viewer → GV-3 realtime + filters → MX-2 metrics views". This document reorders
it: **exploration first, dashboard last**. The reasoning is in §Order.

---

## What is already there, and what is missing

`GET /v1/banks/{bank_id}/graph` exists (CE-7, PR B5) and returns the newest N
nodes plus the links **between those nodes** — an induced subgraph, not an
ego-network. Node text arrives truncated to a 160-character label, with a
comment promising that "the viewer draws a label and fetches the full text on
click".

That endpoint does not exist. Neither does anything that answers "what is
adjacent to *this* node", which is what progressive loading needs.

| need | endpoint | state |
|---|---|---|
| search | `POST /v1/banks/{bank_id}/recall` | ✅ exists, and returns per-arm scores |
| full text of one node | `GET .../nodes/{id}` | ✅ E1 |
| provenance | same | ✅ E1 |
| neighbours of one node | same | ✅ E1, and it is what E3 expands with |
| a filtered set to start from | `GET .../graph?types=&session=&since=&until=&limit=` | ✅ `since`/`until` added by E3 |
| realtime | SSE | ❌ deferred to GV-3 |

---

## Decisions

### 1. The daemon serves the UI. This contradicts R2.

The PRD's round-2 decision is *"배포: `memgardend`(데몬, REST) + 웹UI 서버 분리,
독립 재시작 (R2)"*. We are not doing that: `memgardend` gets a static route and
serves the UI from `/ui/*` on its own port.

**Why the reversal.** R2 was decided before the rest of the system had a
character, and that character turned out to be *one binary, one file, zero
external processes* — the first bullet of the README's "what makes it
different". A second process to run the UI reintroduces exactly the "process
sprawl" §Why rebuild lists as the legacy system's original sin. It also buys
CORS configuration, a second port to document, and a second thing to be down.

**What R2 was protecting** was the ability to restart the UI without restarting
the daemon. In practice the UI is static files: changing it means changing files
on disk, and nothing needs restarting at all. The independence R2 wanted is
free, by a different route than R2 imagined.

**What this costs.** A UI bug that panics a request handler is now in the same
process as retain and recall. Mitigated by the static route doing nothing but
read bytes off disk — it has no database access and no fallible logic beyond
"file not found".

PRD §Constraints is amended to match, rather than left to disagree with the
code. The original wording survives in git history.

### 2. Vanilla JavaScript, no build step, with sigma.js vendored for the graph

No bundler, no framework, no `package.json`. `cargo build` remains the whole
build, and CI keeps its single toolchain.

The one exception is the graph renderer. Writing a WebGL force-directed layout
by hand is the largest risk in Phase E and buys nothing that
[sigma.js](https://www.sigmajs.org/) (MIT) does not already do well. It is
**vendored** — the built file committed under `crates/memgardend/ui/vendor/`
with its version and license recorded — rather than fetched at runtime, because
a CDN dependency in a local-first memory system is a contradiction, and because
the daemon must work with no network.

> **E2 turned out not to need it, so it is not vendored yet.** The argument
> above is about a *force-directed layout over an open-ended graph*, which is
> what E3's progressive expansion produces. An ego-graph is not that: one
> centre and one ring, every edge already present in the `get_node` response
> the panel was just drawn from, and positions that are an angle and a radius
> rather than a simulation. Seventy lines of SVG against a 200 KB vendored
> build and a `vendor/` directory to keep current.
>
> Measured on the real shape rather than assumed: the densest node in the
> gold corpus returns 20 semantic plus 20 temporal neighbours, and 40 dots on
> a 300-unit ring are 47 units apart. The decision above stands unchanged for
> E3 — when the node count leaves one screen and edges arrive incrementally,
> a hand-written layout is exactly the risk this section describes.
>
> **Resolved for E3: vendor it, in 2D, with pan and zoom.** That is what this
> section originally decided and nothing has changed the reasoning; E2 only
> established that the ego-graph did not need it yet.
>
> **One correction to the sentence above: sigma does not carry a layout.** It
> renders and handles the pointer; positions come from somewhere else. The
> package that would have supplied them, `graphology-library`, is 168 KB and
> bundles metrics, generators and community detection to deliver one function,
> so the layout is `d3-force` instead — 17 KB with its three dependencies,
> computing coordinates and nothing more. sigma still earns its 261 KB,
> because the WebGL renderer is what lets E3 drop its filters and draw a whole
> bank. `vendor/README.md` records the measurement.

### 6. 3D is a separate screen, and E3 is not it

The question came up while looking at E2: should the graph be 3D?

**Not for exploration.** The ego-graph already spends both of its axes:
angle is the link type, distance is the weight. A third axis has nothing to
carry, and the costs are not hypothetical — nodes occlude each other, depth
is ambiguous enough that "which of these is closer" stops being readable, a
node behind the camera does not exist until you rotate, and small targets get
harder to hit. 3D graph views photograph well and read badly, and this screen
exists to be read.

**E3 is 2D force with pan and zoom.** Progressive expansion is a
*narrowing* gesture — filter, expand one node, follow an edge — and narrowing
wants legibility and precise hit targets, which is exactly where 2D wins.

**Where 3D does win is a different question, so it gets a different screen.**
Seeing the shape of a whole bank at once — the live one is 5,414 nodes with
over 90,000 semantic edges — is a *survey*, not a walk, and in a graph that
dense the extra dimension genuinely relieves occlusion. E6 builds that as its
own view. Merging the two into one canvas with a toggle would make both
mediocre: the survey wants the whole bank and no filters, the explorer wants
a filtered neighbourhood and precise clicking, and every control would have to
mean two things.

### 3. The viewer is filter-first and loads progressively

`MAX_LIMIT` is 2000 and AC-4 asks for 2,500 nodes. That conflict is not the
interesting one. The interesting one is that after CE-7's fix the live bank
holds **5,333 nodes and 167,398 links** — drawing all of it produces a hairball
that is unreadable long before it is slow.

So the viewer never tries. It opens on a filtered set (a recall result, a
session, a type), and clicking a node adds *its* neighbours. AC-4's "2,500 nodes
smooth" becomes a **rendering benchmark** — the number the renderer must sustain
when a user expands that far — rather than a description of the default screen.
AC-4 is amended to say so.

### 4. Search is `recall`, unchanged

The search box posts to the existing recall endpoint and shows what it returns,
including the per-arm scores (`keyword`, `semantic`, `final`) already in the
response. This makes the first screen answer *"what would MemGarden inject for
this prompt, and why"*, which is the AC-1 review question.

It does **not** answer "what is in this bank" — a memory that fails to be
recalled is invisible here. A browse/list view is a real gap and a later slice.

### 5. UI text is English

Every document, commit message and code comment in this repository is English;
the UI joins them. Memory *content* renders as stored, which is frequently
Korean.

---

## `GET /v1/banks/{bank_id}/nodes/{id}`

One round trip returns everything the detail panel shows.

```jsonc
{
  "id": 4821,
  "uuid": "…",
  "type": "observation",
  "text": "…full text, not truncated…",
  "context": "claude-code",
  "event_date": 1785715200000,
  "mentioned_at": 1785715200000,
  "occurred_start": null,
  "occurred_end": null,
  "proof_count": 3,
  "tags": ["session:…", "file:…"],
  "entities": ["ollama", "retain"],

  // node_sources.observation_id = id — the facts this observation was
  // consolidated from. Empty for a fact.
  "sources": [ { "id": 4102, "uuid": "…", "type": "world", "label": "…" } ],

  // node_sources.source_id = id — observations that cite this node. The
  // reverse direction is indexed (idx_node_sources_source), so it is as cheap
  // as the forward one, and it is what makes `proof_count` auditable from the
  // screen rather than from a query.
  "cited_by": [ { "id": 4821, "uuid": "…", "type": "observation", "label": "…" } ],

  "neighbors": {
    "semantic":  [ { "id": 4830, "label": "…", "weight": 0.83 } ],
    "temporal":  [ … ],
    "caused_by": [ … ]
  }
}
```

**Neighbours union both directions.** `links` is keyed
`(from_node_id, to_node_id, link_type, entity_id)` and the semantic pass writes
edges only *out of* the nodes it was just handed — so a pair embedded in one
batch has two rows and a pair spanning batches has one
(`graph_api.rs::a_semantic_link_reaches_a_node_embedded_in_an_earlier_batch`).
Reading one direction would make a node's neighbourhood depend on when it was
embedded. The endpoint reads `from_node_id = ?1 OR to_node_id = ?1` and reports
the *other* end.

**Bounded like `/graph` is.** Labels truncate at 160 characters, the same
constant. Each neighbour list is capped and ordered by `weight DESC` — a node
with 20 semantic and dozens of temporal edges must not return a megabyte.

**404 on a node in another bank.** The `bank_id` in the path is checked against
the node's, so a guessed id cannot read across banks.

---

## The screen

Graph fills the viewport; everything else floats over it.

```
┌────────────────────────────────────────────────┐
│  ┌──────────────────────┐            ●───○     │
│  │ search…      [bank ▾]│          ╱     ╲    │
│  ├──────────────────────┤        ○   ●──○     │
│  │ 1. fact…   kw .82    │                ●    │
│  │ 2. fact…   sem .91   │      ┌──────────┐   │
│  │ 3. fact…             │      │ detail   │   │
│  └──────────────────────┘      │ …full…   │   │
│                                │ sources  │   │
│                                │ neighbors│   │
│                                └──────────┘   │
└────────────────────────────────────────────────┘
```

**PR 1 ships this shell with the canvas empty**, carrying an empty state that
says selecting a result will draw its neighbourhood. The overlays are the
application in PR 1; PR 2 fills the space behind them. Nothing is built to be
thrown away.

---

## Order

| PR | scope |
|---|---|
| **E1** | `GET .../nodes/{id}`, static serving, the shell: search → results → detail |
| **E2** | ego-graph for the selected node, in SVG — sigma.js deferred to E3, see §Decisions 2 |
| **E3** | filters (type, session, date), progressive expansion, sigma + graphology + d3-force vendored — 2D, pan/zoom |
| E4 | SSE, so a retain appears without a reload (GV-3, AC-4's ≤5 s) |
| E5 | dashboard and ledger views (DB-1, MX-2, AC-5) |
| E6 | the 3D overview — a **separate screen**, not a mode of the explorer (see §Decisions 6) |

**Why the dashboard is last** rather than first as the PRD orders it: the
exploration view is the one that pays immediately. It is the instrument AC-1's
shadow evidence gets reviewed with, and it makes `proof_count`, entity
resolution and the CE-7 link density visible — all three of which produced
defects in the last three days that were found by writing throwaway SQL.

The dashboard's value is operational monitoring, which matters most once the
system is the only one running. That is Phase F.

---

## Deliberately not in this slice

* **Browse/list.** Search only shows what recall ranks; a memory that never
  surfaces is invisible. Real gap, named rather than hidden.
* **Editing.** The UI is read-only. Deleting or editing a memory from a browser
  is a different security posture and the daemon has no auth beyond loopback.
* **The document body.** A node links to its `document_id`, and those documents
  are transcripts running to hundreds of kilobytes. Out of scope.
* **Mental models and consolidation runs.** They have endpoints already; no
  screen yet.
