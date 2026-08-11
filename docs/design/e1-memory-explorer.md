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
| the links *between* nodes already on screen | `GET .../graph?ids=` | ✅ E3 — see below |
| realtime | `GET .../events` (SSE) | ✅ E4 |
| what each bank holds | `GET /v1/stats` | ✅ E5 |
| the counters as they were | `GET /v1/metrics/history` | ✅ E5 — the rows `metrics_task` had been writing since MX-1 with no reader |

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

> **Retracted at E6 — the sentence about occlusion was never measured, and it
> is false for this data.** The separate-screen argument survives; the third
> dimension does not. §E6 has the experiment. The survey view exists, and it
> is a panel of numbers.

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

**`nodes/{id}` alone draws a star, and that is why `?ids=` exists.** It
answers "what is adjacent to *this* one" and nothing else, so two neighbours
that are linked to each other get no edge between them. A graph assembled by
walking then shows the path taken rather than the fabric around it — measured
on the live bank, one node's ego view drew 4 edges where the same node set
actually holds 10, so half the picture was missing. The explorer sends the ids
it has on screen to `/graph?ids=` after every seed and expansion and fills in
the rest. Best-effort: a failure there leaves a graph thinner than the truth,
not a broken one.

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
| **E4** | SSE, so a retain appears without a reload (GV-3, AC-4's ≤5 s) — as a badge, never as a graph that moves itself |
| **E5** | the dashboard and the ledger (DB-1, MX-2, AC-5) — a second page at `/ui/dashboard`, see below |
| **E6** | the survey view — **measured, not drawn**. The 3D plan was tested and dropped; see §E6 |
**Why the dashboard is last** rather than first as the PRD orders it: the
exploration view is the one that pays immediately. It is the instrument AC-1's
shadow evidence gets reviewed with, and it makes `proof_count`, entity
resolution and the CE-7 link density visible — all three of which produced
defects in the last three days that were found by writing throwaway SQL.

The dashboard's value is operational monitoring, which matters most once the
system is the only one running. That is Phase F.

---

## E5 — the dashboard

`/ui/dashboard`, polling five endpoints every 10 seconds: `/healthz` for the
verdict, `/metrics.json` for the counters, `/v1/stats`, `/v1/metrics/history`
and `/v1/ledger`. memdash's successor, with the two things AC-5 adds to it —
a HEALTHY/DEGRADED/UNHEALTHY judgement and the 10-second refresh.

**A second page, not a mode.** It shares `style.css` and the `$`/`el`/`api`/
`date` helpers, now in `common.js`, and nothing else. The explorer is a graph
filling a fixed viewport; the dashboard is a scrolling document on a timer.
Merging them would give every control two meanings — the same argument
§Decisions 6 makes about E6, arrived at for the same reason.

**The verdict is rendered, not recomputed.** `routes/health.rs` already
decides HEALTHY vs DEGRADED vs UNHEALTHY, and two definitions of "healthy"
that can disagree is how a dashboard starts lying. The one verdict the page
adds is `UNREACHABLE`, which is the one a daemon cannot report about itself.
`/healthz` answers **503** when the news is bad, which is why this page reads
that response directly rather than through the shared `api()` helper — a
helper that throws on non-2xx would turn the report into an absence.

**A failed poll costs one panel, not the screen.** Each section renders from
its own response and keeps its last good value; only the verdict and the tick
line change to say a fetch failed. A dashboard whose whole purpose is to show
a sick daemon is useless if a sick daemon empties it.

**Two new read routes, outside the timing middleware.** `/v1/stats` is a
`GROUP BY` over `memory_nodes`, `documents` and a `links` join — 46 ms on the
live database, six times a minute — and `/v1/metrics/history` reads the
snapshot table. Both sit with `/metrics.json` outside `track_http` for the
reason that route already gives: an operator watching the numbers must not be
the reason the numbers move. `/healthz` stays measured; it predates the
dashboard and an external monitor is expected to call it.

`/v1/stats` is separate from `/v1/banks` rather than a field on it because the
explorer calls `/v1/banks` to fill a dropdown on every page load, and that
must not start paying for a 200,000-row join.

### Two things building the screen found

**The ledger API was deleting what the ledger collects.** `LedgerResponse`
flattened the `detail` column into the five fields of the *manual* case shape,
so a `retain_cap_saving` row — which records `{raw_tokens, capped_tokens,
saved, ratio}` and none of those five — came back as a row of nulls. Eight
automatic rows on the live database rendered as eight rows of `—`. AC-6's
whole claim is that the ledger collects itself; an endpoint that can only read
what a human typed into it defeats that. `detail` is now returned whole, and
`kind` says how to read it — strict about what is written, permissive about
what is read.

**Skipping hidden tabs made the page freeze while claiming to refresh.** The
poll originally ran only when `document.visibilityState === "visible"`, to
save what `/v1/stats` costs. Watched in a real browser with the daemon killed,
the page sat at HEALTHY for 25 seconds with "every 10s" in the corner: this is
a screen you leave open on a second monitor, so the one state it must never
be in is stale-but-confident. The interval now runs regardless — browsers
throttle background timers to about a minute on their own, which is the only
budget this needs — and `visibilitychange` still forces a refresh so returning
to the tab shows current numbers rather than whatever the throttle left.

---

## E6 — the survey, measured rather than drawn

`GET /v1/banks/{bank_id}/anatomy`, and a panel on the dashboard that runs it
on demand. It reports the node and link counts, the split by link type, the
degree distribution as five numbers, the connected components with what kinds
of memory each holds, and two counts that turned out to be the whole story:
links that cross a `fact_type` boundary, and `node_sources` provenance rows.

### The 3D plan was tested before it was built, and it failed

§Decisions 6 promised a WebGL survey on the argument that at this density the
extra axis relieves occlusion. That was an assertion, so E6 started by
measuring it: `3d-force-graph` (1.31 MB), the largest live bank (3,200 nodes,
118,937 links), the same 2,000-node slice the explorer draws.

| condition | edges | result | time |
|---|---|---|---|
| 2D (sigma, already vendored) | 37,432 | white hairball, opaque core | instant |
| **3D, everything** | 63,384 | featureless sphere | 18.6 s |
| **3D, `semantic` only** (−62% edges) | 24,196 | the same sphere | 21.4 s |
| 3D, camera inside | — | an even mesh in every direction | — |
| **three SQL statements** | — | components, the type split, degrees, edge mix | **~3 ms** |

The reason is the degree distribution, not the node count: p50 = 74 and
p90 = 101, so the bank has no hubs. A force layout separates what has an axis
to be separated along; a flat degree distribution offers none, and a third
dimension only makes a rounder ball. Rotation is 3D's real advantage and it
does not help — a sphere is a sphere from every angle, which is the "photograph
well and read badly" this document already warned about, arriving on the view
that was supposed to be the exception.

So 1.31 MB is not vendored, `vendor/` stays at 272 KB, and the cargo feature
flag that was going to hide the cost is not needed because there is no cost.

### What the measurement found

**The bank is two universes.** Links are only ever written between nodes of the
same `fact_type` (`links.rs:67` for temporal, `:142` for semantic — legacy
parity with `link_utils.py:394-395`). On the live bank exactly **one** of
118,937 links crosses a type boundary: a lone `caused_by` edge that attaches
the 30 `experience` nodes to the `world` component. The 1,177 `observation`
nodes share no link at all with the rest of the bank, so no amount of walking
the graph from a `world` node ever reaches one.

**What does join them is not drawn.** `node_sources` — the "built from"
relation between an observation and the facts it was consolidated from — holds
1,418 rows on that bank, 1,396 of them observation→world. They are the bank's
real connective tissue, they are already in the `nodes/{id}` response as
`sources` / `cited_by`, and the graph screen renders none of them because they
are not `links`. `provenance_edges` is on the panel to say how much of the
structure the explorer is not showing.

Neither fact came from a picture. Both are one `GROUP BY` away, and that is
the argument for this shape of survey.

### Cost

23–52 ms for the largest live bank (3,200 nodes, 118,937 links), against
21,439 ms for the 3D layout of a smaller slice. It reads every link in the
bank, so it is an on-demand route and deliberately not part of the dashboard's
10-second poll.

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
