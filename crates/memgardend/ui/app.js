// E1 — the explorer shell.
//
// No build step and no framework: this file is served as-is. It talks to the
// daemon that served it, so `check_host` passes on the Host header a browser
// sends anyway and no token is needed (middleware.rs:34 — the token travels
// on responses, not requests).
//
// Two calls do everything here:
//   POST /v1/banks/{bank}/recall     what would be injected, with arm scores
//   GET  /v1/banks/{bank}/nodes/{id} one memory in full

const $ = (sel) => document.querySelector(sel);
const bankSel = $("#bank");
const results = $("#results");
const statusLine = $("#search-status");
const detailPanel = $("#detail-panel");
const detail = $("#detail");

/** Everything user-supplied goes through here. No innerHTML with data in it. */
const el = (tag, props = {}, kids = []) => {
  const node = Object.assign(document.createElement(tag), props);
  for (const kid of [].concat(kids)) {
    node.append(kid?.nodeType ? kid : document.createTextNode(kid));
  }
  return node;
};

async function api(path, init) {
  const res = await fetch(path, {
    headers: { "content-type": "application/json" },
    ...init,
  });
  if (!res.ok) {
    throw new Error(`${res.status} ${(await res.text()).slice(0, 200)}`);
  }
  return res.json();
}

const bank = () => bankSel.value;

// --- search ---------------------------------------------------------------

async function search(query) {
  statusLine.textContent = "…";
  results.replaceChildren();
  detailPanel.hidden = true;
  try {
    const out = await api(`/v1/banks/${encodeURIComponent(bank())}/recall`, {
      method: "POST",
      body: JSON.stringify({ query, limit: 20 }),
    });
    render(out);
  } catch (e) {
    statusLine.textContent = e.message;
    statusLine.className = "error";
  }
}

function render(out) {
  const rows = out.results ?? [];
  const c = out.counts ?? {};
  statusLine.className = "";
  statusLine.textContent = rows.length
    ? `${rows.length} of ${c.candidates ?? "?"} candidates · ${c.tokens ?? "?"} tokens`
    : "Nothing recalled for that.";

  results.replaceChildren(
    ...rows.map((r, i) => {
      const s = r.scores ?? {};
      const li = el("li", { tabIndex: 0 }, [
        el("div", { className: "row-text" }, r.text ?? ""),
        el("div", { className: "row-meta" }, [
          el("span", { className: `type ${r.type ?? ""}` }, r.type ?? "?"),
          // The two retrieval arms, then the fused score. `semantic` and
          // `keyword` are null when that arm did not return the node at all,
          // which is a different thing from returning it with a low score.
          arm("kw", s.keyword),
          arm("sem", s.semantic),
          el("span", { title: scoreTooltip(s) }, `final ${fmt(s.final)}`),
        ]),
      ]);
      const open = () => {
        for (const other of results.children) other.removeAttribute("aria-current");
        li.setAttribute("aria-current", "true");
        showNode({ id: r.id });
      };
      li.addEventListener("click", open);
      li.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); }
      });
      li.dataset.rank = String(i + 1);
      return li;
    }),
  );
}

/** An arm that did not fire is shown faded rather than omitted: "keyword only"
 *  and "both arms agreed" are different facts about why a memory surfaced.
 *
 *  The two numbers are **not on the same scale** and must not be read against
 *  each other — `keyword` is a raw BM25 score (negative, and its magnitude
 *  depends on the query's term statistics) while `semantic` is a cosine
 *  similarity in [0,1]. Only `final` orders the list. Said in a tooltip
 *  rather than by rescaling, because a rescaled number would be one this
 *  system never computed. */
const ARM_SCALE = {
  kw: "raw BM25 — negative, scale depends on the query. Not comparable to sem.",
  sem: "cosine similarity, 0–1.",
};
const arm = (name, score) =>
  el("span", {
    className: score == null ? "arm absent" : "arm",
    title: score == null
      ? `this arm did not return the memory at all — ${ARM_SCALE[name]}`
      : ARM_SCALE[name],
  }, `${name} ${score == null ? "—" : fmt(score)}`);

const fmt = (n) => (typeof n === "number" ? n.toFixed(3).replace(/^0/, "") : "—");

/** The rest of the breakdown, on hover rather than in the row: `rrf` is the
 *  fusion of the two arms and the others are multipliers applied after it. */
const scoreTooltip = (s) =>
  ["rrf", "recency", "temporal", "proof"]
    .map((k) => `${k} ${fmt(s[k])}`)
    .join("   ");

// --- detail ---------------------------------------------------------------

async function showNode(ref) {
  if (ref.id == null) {
    detail.replaceChildren(el("p", { className: "empty" },
      "This result carries no node id."));
    detailPanel.hidden = false;
    return;
  }
  detailPanel.hidden = false;
  detail.replaceChildren(el("p", { className: "empty" }, "…"));
  try {
    const n = await api(
      `/v1/banks/${encodeURIComponent(bank())}/nodes/${ref.id}`);
    detail.replaceChildren(...detailView(n));
    detail.parentElement.scrollTop = 0;
    // Selecting a node already on the graph must not rearrange it — that was
    // E2's behaviour and it threw away the walk. A result from the search
    // list has nothing to preserve, so that seeds a fresh ego view.
    if (graph.hasNode(String(n.id))) {
      select(String(n.id));
    } else {
      seedEgo(n);
    }
  } catch (e) {
    detail.replaceChildren(el("p", { className: "error" }, e.message));
  }
}

function detailView(n) {
  const out = [
    el("h2", {}, [el("span", { className: `type ${n.type}` }, n.type)]),
    el("p", { className: "full-text" }, n.text),
  ];

  const meta = el("dl", { className: "meta" });
  const pair = (k, v) => {
    if (v == null || v === "") return;
    meta.append(el("dt", {}, k), el("dd", {}, v));
  };
  pair("id", String(n.id));
  pair("context", n.context);
  pair("event date", date(n.event_date));
  pair("created", date(n.created_at));
  // proof_count is derived from node_sources; showing both next to each other
  // is what makes a disagreement visible without writing SQL.
  pair("proof count", `${n.proof_count} (sources: ${n.sources.length})`);
  if (meta.childElementCount) out.push(meta);

  out.push(...chips("Entities", n.entities), ...chips("Tags", n.tags));
  out.push(...related("Built from", n.sources));
  out.push(...related("Cited by", n.cited_by));

  for (const [type, list] of Object.entries(n.neighbors ?? {})) {
    out.push(...related(`${type} neighbours`, list, PER_TYPE_CAP));
  }
  return out;
}

/** `graph::MAX_NEIGHBORS_PER_TYPE`. A list landing exactly on it is probably
 *  truncated, and "20" reading as the whole neighbourhood when it is really
 *  the ceiling is the kind of quiet wrongness this project keeps finding. */
const PER_TYPE_CAP = 20;

const chips = (title, items) =>
  !items?.length ? [] : [
    el("h2", {}, title),
    el("div", { className: "chips" },
       items.map((t) => el("span", { className: "chip" }, t))),
  ];

const related = (title, items, cap) =>
  !items?.length ? [] : [
    el("h2", { title: items.length === cap ? `capped at ${cap}` : "" },
       `${title} (${items.length}${items.length === cap ? "+" : ""})`),
    el("ul", { className: "related" }, items.map((r) => {
      const btn = el("button", { type: "button" }, [
        el("div", { className: "row-text" }, r.label),
        el("div", { className: "row-meta" }, [
          el("span", { className: `type ${r.type}` }, r.type),
          ...(r.weight == null ? [] : [el("span", {}, `w ${fmt(r.weight)}`)]),
        ]),
      ]);
      // Navigating from a neighbour to its own detail is the exploration loop.
      btn.addEventListener("click", () => showNode({ id: r.id }));
      return el("li", {}, btn);
    })),
  ];

const date = (ms) =>
  ms == null ? null : new Date(ms).toISOString().slice(0, 16).replace("T", " ");
// --- E3: the graph ---------------------------------------------------------
//
// sigma renders and handles the pointer; graphology is the structure it
// renders; d3-force computes coordinates. sigma carries no layout, which is
// the one thing `e1-memory-explorer.md` §Decisions 2 got wrong about it —
// `vendor/README.md` has the sizes that decided the split.
//
// Two gestures, and keeping them apart is the whole interaction design:
//
//   hover        read      the peek panel, no request
//   click        select    the detail panel, graph untouched
//   double       expand    that node's neighbours join the graph
//
// E2 conflated the last two — click both selected *and* replaced the graph —
// so following an edge threw away where you had been. Expansion is additive
// now, which is what makes a walk through the graph accumulate into a view of
// a neighbourhood rather than a sequence of unrelated stars.

const canvas = $("#canvas");
const canvasEmpty = $("#canvas-empty");
const graphStatus = $("#graph-status");

/** Node colour is fact_type, edge colour is link type — the two axes never
 *  share a hue, so both can be read at once. Kept in step with `style.css`
 *  by hand; there is no build step to share them through. */
const TYPE_COLOR = {
  world: "#7aa2f7",
  observation: "#bb9af7",
  experience: "#e0af68",
};
const EDGE_COLOR = {
  semantic: "#7fd1ae",
  temporal: "#7aa2f7",
  caused_by: "#f7768e",
  "built from": "#e0af68",
  "cited by": "#bb9af7",
};
const DIM = "#8b93a7";

const graph = new graphology.UndirectedGraph();
let renderer = null;
/** The node whose detail panel is open. Drawn larger, and kept out of the
 *  force layout's way when it is the centre of a fresh ego view. */
let selected = null;

function sigmaRenderer() {
  if (renderer) return renderer;
  renderer = new Sigma(graph, canvas, {
    renderEdgeLabels: false,
    defaultEdgeColor: DIM,
    // Labels are off: at expansion sizes they overlap into noise, and the
    // peek already answers "what is this" without costing screen.
    renderLabels: false,
    zIndex: true,
  });

  renderer.on("enterNode", ({ node, event }) => {
    showPeek(event.original, graph.getNodeAttributes(node));
  });
  renderer.on("leaveNode", hidePeek);
  renderer.on("clickNode", ({ node }) => {
    hidePeek();
    showNode({ id: Number(node) });
  });
  renderer.on("doubleClickNode", (e) => {
    // Without this sigma also zooms, which fights the expansion that is
    // about to move everything.
    e.preventSigmaDefault();
    hidePeek();
    expand(Number(e.node));
  });
  return renderer;
}

// --- the peek --------------------------------------------------------------
//
// This was an SVG `<title>`, the browser's native tooltip: it waits about a
// second and cannot be styled, so hovering appeared to do nothing and the only
// way to learn what a dot was became clicking it — which is the *navigation*
// gesture. A peek on the same frame as the cursor separates reading from
// travelling.

const peek = $("#peek");

function showPeek(mouseEvent, attrs) {
  peek.replaceChildren(
    el("div", { className: "peek-text" }, attrs.text || attrs.label || ""),
    el("div", { className: "peek-meta" }, [
      el("span", { className: `type ${attrs.factType}` }, attrs.factType),
      ...(attrs.via ? [el("span", {}, attrs.via)] : []),
      ...(attrs.weight == null ? [] : [el("span", {}, `w ${fmt(attrs.weight)}`)]),
      el("span", { className: "peek-hint" }, "double-click to expand"),
    ]),
  );
  peek.hidden = false;
  movePeek(mouseEvent);
}

/** Follows the cursor and flips rather than leaving the viewport: the graph
 *  reaches every edge of the screen, so a peek that only opened down-right
 *  would be clipped for everything on the right half. */
function movePeek(e) {
  const gap = 14;
  let x = e.clientX + gap;
  let y = e.clientY + gap;
  if (x + peek.offsetWidth > window.innerWidth - 8) x = e.clientX - peek.offsetWidth - gap;
  if (y + peek.offsetHeight > window.innerHeight - 8) y = e.clientY - peek.offsetHeight - gap;
  peek.style.left = `${Math.max(8, x)}px`;
  peek.style.top = `${Math.max(8, y)}px`;
}

function hidePeek() {
  peek.hidden = true;
}

// --- adding to the graph ---------------------------------------------------

const nodeSize = (id) => (id === selected ? 11 : 6);

/** Marks a node as the open one without moving anything. */
function select(key) {
  const previous = selected;
  selected = key;
  for (const id of [previous, key]) {
    if (id && graph.hasNode(id)) graph.setNodeAttribute(id, "size", nodeSize(id));
  }
  sigmaRenderer().refresh();
}

/** Idempotent: expanding two neighbours that share a third must not duplicate
 *  it, and re-adding a node already on screen must not move it. */
function addNode(id, attrs) {
  if (graph.hasNode(String(id))) {
    graph.mergeNodeAttributes(String(id), attrs);
    return false;
  }
  graph.addNode(String(id), {
    size: nodeSize(String(id)),
    color: TYPE_COLOR[attrs.factType] || DIM,
    ...attrs,
  });
  return true;
}

function addEdge(a, b, linkType, weight) {
  const [x, y] = [String(a), String(b)];
  if (x === y || !graph.hasNode(x) || !graph.hasNode(y) || graph.hasEdge(x, y)) return;
  graph.addEdge(x, y, {
    color: EDGE_COLOR[linkType] || DIM,
    size: weight == null ? 1 : 0.5 + weight * 2,
    linkType,
    weight,
  });
}

/** Every group a node detail carries, in a fixed order so the same node lays
 *  out the same way twice. */
function detailGroups(n) {
  const out = Object.entries(n.neighbors ?? {}).sort(([a], [b]) =>
    a < b ? -1 : a > b ? 1 : 0);
  if (n.sources?.length) out.push(["built from", n.sources]);
  if (n.cited_by?.length) out.push(["cited by", n.cited_by]);
  return out.filter(([, list]) => list?.length);
}

// --- the ego seed ----------------------------------------------------------

const R_NEAR = 3;
const R_FAR = 9;

/** Weight becomes distance: a 1.0 neighbour sits at `R_NEAR`, the 0.7
 *  `SEMANTIC_LINK_MIN_SIMILARITY` floor at `R_FAR`. Provenance carries no
 *  weight and sits midway rather than claiming a similarity it does not have. */
const radiusFor = (w) =>
  w == null
    ? (R_NEAR + R_FAR) / 2
    : R_FAR - Math.min(Math.max((w - 0.7) / 0.3, 0), 1) * (R_FAR - R_NEAR);

/** A fresh graph around one memory, laid out by meaning rather than by
 *  physics: one sector per link type, angle inside it arbitrary but stable,
 *  radius from the weight. The force layout takes over on the first
 *  expansion — at which point the sectors stop meaning anything, which is the
 *  honest trade for being able to grow.  */
function seedEgo(n) {
  graph.clear();
  selected = String(n.id);
  addNode(n.id, { x: 0, y: 0, factType: n.type, text: n.text, label: n.text });

  const groups = detailGroups(n);
  const total = groups.reduce((sum, [, list]) => sum + list.length, 0);
  const GAP = 0.06;
  let start = -Math.PI / 2;
  for (const [linkType, list] of groups) {
    const span = (2 * Math.PI * list.length) / total;
    const inner = Math.max(span - GAP, span * 0.5);
    const step = inner / list.length;
    list.forEach((r, i) => {
      const angle = start + (span - inner) / 2 + step * (i + 0.5);
      const rad = radiusFor(r.weight);
      addNode(r.id, {
        x: Math.cos(angle) * rad,
        y: Math.sin(angle) * rad,
        factType: r.type,
        text: r.label,
        label: r.label,
        via: linkType,
        weight: r.weight,
      });
      addEdge(n.id, r.id, linkType, r.weight);
    });
    start += span;
  }
  sigmaRenderer().refresh();
  fitView();
  reportGraph(`${total} neighbours of the selected memory`);
}

// --- expansion -------------------------------------------------------------

/** Additive, and that is the point: the graph you built by walking stays.
 *  New nodes start on top of the node they came from so the force layout
 *  pushes them outward from the right place instead of flying in from 0,0. */
async function expand(id) {
  const key = String(id);
  const seed = graph.hasNode(key)
    ? { x: graph.getNodeAttribute(key, "x"), y: graph.getNodeAttribute(key, "y") }
    : { x: 0, y: 0 };
  reportGraph("expanding…");
  try {
    const n = await api(`/v1/banks/${encodeURIComponent(bank())}/nodes/${id}`);
    addNode(n.id, { x: seed.x, y: seed.y, factType: n.type, text: n.text, label: n.text });
    let added = 0;
    for (const [linkType, list] of detailGroups(n)) {
      for (const r of list) {
        const jitter = () => (Math.random() - 0.5) * 2;
        if (addNode(r.id, {
          x: seed.x + jitter(),
          y: seed.y + jitter(),
          factType: r.type,
          text: r.label,
          label: r.label,
          via: linkType,
          weight: r.weight,
        })) added++;
        addEdge(n.id, r.id, linkType, r.weight);
      }
    }
    relayout();
    reportGraph(added ? `+${added} new` : "no new neighbours");
  } catch (e) {
    reportGraph(`expand failed: ${e.message}`);
  }
}

// --- the force layout ------------------------------------------------------

let sim = null;

/** Runs once the graph stops being an ego view. Link distance follows the
 *  weight so a strong edge still pulls its ends together — the one piece of
 *  the seed's meaning that survives into the physics. */
function relayout() {
  if (sim) sim.stop();
  const nodes = graph.nodes().map((id) => ({
    id,
    x: graph.getNodeAttribute(id, "x"),
    y: graph.getNodeAttribute(id, "y"),
  }));
  const links = graph.edges().map((e) => ({
    source: graph.source(e),
    target: graph.target(e),
    weight: graph.getEdgeAttribute(e, "weight"),
  }));

  sim = d3.forceSimulation(nodes)
    .force("charge", d3.forceManyBody().strength(-30))
    .force("link", d3.forceLink(links)
      .id((d) => d.id)
      .distance((l) => 2 + (1 - (l.weight ?? 0.5)) * 6)
      .strength(0.5))
    .force("center", d3.forceCenter(0, 0))
    .force("collide", d3.forceCollide(0.8))
    .alpha(0.9)
    .alphaDecay(0.03);

  sim.on("tick", () => {
    for (const n of nodes) {
      graph.setNodeAttribute(n.id, "x", n.x);
      graph.setNodeAttribute(n.id, "y", n.y);
    }
  });
  sim.on("end", fitView);
}

function fitView() {
  const cam = sigmaRenderer().getCamera();
  cam.animatedReset({ duration: 250 });
}

function reportGraph(note) {
  graphStatus.replaceChildren(
    el("span", {}, `${graph.order} nodes · ${graph.size} edges`),
    ...(note ? [el("span", { className: "dim" }, note)] : []),
  );
  canvasEmpty.hidden = graph.order > 0;
}

// --- filters ---------------------------------------------------------------

/** Draws a bank rather than a memory: whatever the filters select, straight
 *  from `/graph`, force-laid out because there is no centre to arrange around.
 *
 *  `since` / `until` go to the server. They have to: `/graph` returns the
 *  newest `limit` nodes, so a range applied here could only narrow that
 *  window and could never reach anything older — which is the entire reason
 *  to ask for a date. */
async function drawFiltered() {
  const types = [...document.querySelectorAll("#f-types input:checked")].map((i) => i.value);
  const p = new URLSearchParams();
  p.set("limit", $("#f-limit").value || "200");
  if (types.length && types.length < 3) p.set("types", types.join(","));
  const session = $("#f-session").value.trim();
  if (session) p.set("session", session);
  const ms = (v) => (v ? Date.parse(`${v}T00:00:00Z`) : null);
  const since = ms($("#f-since").value);
  const until = ms($("#f-until").value);
  if (since != null) p.set("since", String(since));
  // Inclusive to the end of the chosen day, which is what a date input means
  // to the person typing it.
  if (until != null) p.set("until", String(until + 86_399_999));

  reportGraph("drawing…");
  try {
    const out = await api(`/v1/banks/${encodeURIComponent(bank())}/graph?${p}`);
    graph.clear();
    selected = null;
    for (const n of out.nodes ?? []) {
      addNode(n.id, {
        x: (Math.random() - 0.5) * 10,
        y: (Math.random() - 0.5) * 10,
        factType: n.type,
        text: n.text,
        label: n.text,
      });
    }
    for (const l of out.links ?? []) addEdge(l.from, l.to, l.type, l.weight);
    sigmaRenderer().refresh();
    relayout();
    reportGraph(`${(out.nodes ?? []).length} drawn`);
  } catch (e) {
    reportGraph(`draw failed: ${e.message}`);
  }
}

// --- boot -----------------------------------------------------------------

$("#detail-close").addEventListener("click", () => { detailPanel.hidden = true; });
$("#f-apply").addEventListener("click", drawFiltered);
$("#search-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const q = $("#q").value.trim();
  if (q) search(q);
});

(async () => {
  try {
    // `/v1/banks` answers a bare array, not `{ banks: [...] }`.
    const banks = await api("/v1/banks");
    const usable = (Array.isArray(banks) ? banks : []).filter((b) => b.bank_id);
    bankSel.replaceChildren(
      ...usable.map((b) => el("option", { value: b.bank_id }, b.bank_id)),
    );
    if (!usable.length) statusLine.textContent = "No banks yet.";
  } catch (e) {
    statusLine.textContent = `Cannot reach the daemon: ${e.message}`;
    statusLine.className = "error";
  }
})();
