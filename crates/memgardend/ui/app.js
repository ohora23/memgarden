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
    drawEgo(n);
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

// --- E2: the ego-graph ----------------------------------------------------
//
// SVG, and no library. `e1-memory-explorer.md` §Order planned sigma.js here,
// and it is still the right answer for E3's progressive expansion, where the
// node count leaves one screen and a force layout starts earning its keep. An
// ego-graph does not: one centre and one ring, every edge already in the
// `get_node` response this drew the panel from, and angles that are
// arithmetic. Seventy lines against a 200 KB vendored build — the library
// waits for the problem it is good at.
//
// `#canvas` stays `aria-hidden`: every node here is also a button in the
// detail panel, which is the keyboard path and the accessible one. The graph
// is a second view of the same data, not a second source of it.

const canvas = $("#canvas");
const SVG_NS = "http://www.w3.org/2000/svg";

const svg = (tag, attrs = {}, kids = []) => {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v != null) node.setAttribute(k, v);
  }
  node.append(...kids);
  return node;
};

/** Edge colour by link type. Nodes keep their fact_type colour, so the two
 *  axes never share a hue and a reader can hold both at once. */
const EDGE = {
  semantic: "#7fd1ae",
  temporal: "#7aa2f7",
  caused_by: "#f7768e",
  "built from": "#e0af68",
  "cited by": "#bb9af7",
};

/** Groups in a fixed order, so redrawing the same node lands identically and
 *  a neighbour does not move under the cursor between visits. */
function egoGroups(n) {
  const out = Object.entries(n.neighbors ?? {}).sort(([a], [b]) =>
    a < b ? -1 : a > b ? 1 : 0);
  if (n.sources?.length) out.push(["built from", n.sources]);
  if (n.cited_by?.length) out.push(["cited by", n.cited_by]);
  return out.filter(([, list]) => list?.length);
}

const R_NEAR = 130;
const R_FAR = 300;

/** Weight becomes distance: a 1.0 neighbour sits at `R_NEAR`, the 0.7
 *  `SEMANTIC_LINK_MIN_SIMILARITY` floor at `R_FAR`. Provenance edges carry no
 *  weight and sit midway rather than claiming a similarity they do not have. */
const radiusFor = (w) =>
  w == null
    ? (R_NEAR + R_FAR) / 2
    : R_FAR - Math.min(Math.max((w - 0.7) / 0.3, 0), 1) * (R_FAR - R_NEAR);

/** Hover reads, click travels.
 *
 *  This was an SVG `<title>`, which is the browser's native tooltip: it waits
 *  about a second before appearing and cannot be styled. The effect was that
 *  hovering appeared to do nothing, so the only way to learn what a dot was
 *  became clicking it — and clicking is the *navigation* gesture, so reading
 *  the neighbourhood meant walking it. A peek that appears on the same frame
 *  as the cursor separates the two: hover to read, click to go.
 *
 *  It shows the label the response already carries (truncated to 160
 *  characters by `/graph`'s `MAX_LABEL_LEN`), so peeking costs no request. */
const peek = $("#peek");

function bindPeek(node, text, factType, linkType, weight) {
  node.addEventListener("pointerenter", (e) => {
    peek.replaceChildren(
      el("div", { className: "peek-text" }, text),
      el("div", { className: "peek-meta" }, [
        el("span", { className: `type ${factType}` }, factType),
        el("span", {}, linkType),
        ...(weight == null ? [] : [el("span", {}, `w ${fmt(weight)}`)]),
      ]),
    );
    peek.hidden = false;
    movePeek(e);
  });
  node.addEventListener("pointermove", movePeek);
  node.addEventListener("pointerleave", hidePeek);
}

/** Follows the cursor, and flips to the other side rather than leaving the
 *  viewport — the outer ring reaches the edges of the canvas, so a peek that
 *  only ever opened down-right would be clipped for every node on the right
 *  half. */
function movePeek(e) {
  const gap = 14;
  const w = peek.offsetWidth;
  const h = peek.offsetHeight;
  let x = e.clientX + gap;
  let y = e.clientY + gap;
  if (x + w > window.innerWidth - 8) x = e.clientX - w - gap;
  if (y + h > window.innerHeight - 8) y = e.clientY - h - gap;
  peek.style.left = `${Math.max(8, x)}px`;
  peek.style.top = `${Math.max(8, y)}px`;
}

function hidePeek() {
  peek.hidden = true;
}

function drawEgo(n) {
  const groups = egoGroups(n);
  const total = groups.reduce((sum, [, list]) => sum + list.length, 0);
  if (!total) {
    canvas.replaceChildren(el("p", { className: "empty" },
      "This memory has no neighbours yet."));
    return;
  }

  const root = svg("svg", {
    class: "ego",
    viewBox: "-430 -390 860 780",
    preserveAspectRatio: "xMidYMid meet",
  });
  const edges = svg("g");
  const labels = svg("g");
  const dots = svg("g");

  // Each group gets arc proportional to its size, minus a gap so the sectors
  // read as sectors rather than as one undifferentiated ring.
  const GAP = 0.06;
  let start = -Math.PI / 2;
  for (const [type, list] of groups) {
    const span = (2 * Math.PI * list.length) / total;
    const inner = Math.max(span - GAP, span * 0.5);
    const step = inner / list.length;
    const colour = EDGE[type] ?? "#8b93a7";

    list.forEach((r, i) => {
      const angle = start + (span - inner) / 2 + step * (i + 0.5);
      const rad = radiusFor(r.weight);
      const x = +(Math.cos(angle) * rad).toFixed(1);
      const y = +(Math.sin(angle) * rad).toFixed(1);
      edges.append(svg("line", {
        x1: 0, y1: 0, x2: x, y2: y,
        stroke: colour,
        "stroke-width": r.weight == null ? 1 : 1 + r.weight * 1.5,
        opacity: 0.4,
      }));
      const dot = svg("circle", { cx: x, cy: y, r: 7, class: `ego-node ${r.type}` });
      bindPeek(dot, r.label, r.type, type, r.weight);
      dot.addEventListener("click", () => showNode({ id: r.id }));
      dots.append(dot);
    });

    // The group's name where its sector points — the legend, without a legend.
    const mid = start + span / 2;
    const cos = Math.cos(mid);
    labels.append(svg("text", {
      x: +(cos * (R_FAR + 44)).toFixed(1),
      y: +(Math.sin(mid) * (R_FAR + 44)).toFixed(1),
      class: "ego-label",
      fill: colour,
      "text-anchor": cos > 0.3 ? "start" : cos < -0.3 ? "end" : "middle",
    }, [document.createTextNode(`${type} (${list.length})`)]));
    start += span;
  }

  // Centre last so it sits over the edges leaving it.
  const centre = svg("circle", {
    cx: 0, cy: 0, r: 13, class: `ego-node ego-centre ${n.type}`,
  });
  bindPeek(centre, n.text, n.type, "selected", null);
  root.append(edges, labels, dots, centre);
  hidePeek();
  canvas.replaceChildren(root);
}

// --- boot -----------------------------------------------------------------

$("#detail-close").addEventListener("click", () => { detailPanel.hidden = true; });
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
