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
