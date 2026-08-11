// E5 — the dashboard (DB-1, MX-2, AC-5).
//
// memdash's successor, reading four endpoints every 10 seconds:
//
//   GET /healthz               the HEALTHY/DEGRADED/UNHEALTHY verdict
//   GET /metrics.json          counters and latency histograms
//   GET /v1/stats              what each bank holds
//   GET /v1/metrics/history    those counters as they were, for the trend
//   GET /v1/ledger             the benefit ledger (MX-2)
//
// Two rules the explorer does not have to care about:
//
// **A failed poll must not blank the screen.** Each section renders from its
// own response and keeps the last good one when its fetch fails, because a
// dashboard whose whole purpose is to show you a sick daemon is useless if a
// sick daemon empties it. Only the verdict changes to say so.
//
// **`/healthz` answers 503 when the news is bad**, which is the one place the
// shared `api()` helper is wrong for this page: it throws on a non-2xx, and
// the body of that 503 is exactly the UNHEALTHY report AC-5 asks for. So this
// file reads that response itself.

import { $, api, date, el } from "/ui/common.js";

const REFRESH_MS = 10_000;

// --- formatting ------------------------------------------------------------

const n = (v) => (typeof v === "number" ? v.toLocaleString("en-US") : "—");

/** Microseconds as milliseconds — every latency in the API is in µs. */
const ms = (us) =>
  typeof us === "number" ? `${(us / 1000).toFixed(1)} ms` : "—";

const pct = (part, whole) =>
  whole > 0 ? `${((part / whole) * 100).toFixed(1)}%` : "—";

const bytes = (b) => {
  if (typeof b !== "number") return "—";
  const mb = b / 1024 / 1024;
  return mb >= 1024 ? `${(mb / 1024).toFixed(2)} GB` : `${mb.toFixed(1)} MB`;
};

const duration = (msTotal) => {
  if (typeof msTotal !== "number") return "—";
  const s = Math.floor(msTotal / 1000);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m}m`;
  return `${m}m ${s % 60}s`;
};

// --- the verdict -----------------------------------------------------------

const hero = $("#hero");

/**
 * `/healthz` decides HEALTHY vs DEGRADED vs UNHEALTHY server-side (see
 * routes/health.rs) and this renders that verdict rather than recomputing it
 * — two definitions of "healthy" that can disagree is how a dashboard starts
 * lying. The one verdict added here is `UNREACHABLE`, which the daemon
 * cannot report about itself.
 */
async function renderHealth() {
  let h;
  try {
    const res = await fetch("/healthz", {
      headers: { "content-type": "application/json" },
    });
    h = await res.json();
  } catch {
    hero.className = "verdict-unhealthy";
    hero.querySelector(".verdict").textContent = "UNREACHABLE";
    hero.querySelector(".hero-line").textContent =
      "No answer from the daemon on this origin.";
    hero.querySelector(".hero-sub").textContent = "";
    return null;
  }

  const status = h.status ?? "UNHEALTHY";
  hero.className = `verdict-${status.toLowerCase()}`;
  hero.querySelector(".verdict").textContent = status;
  hero.querySelector(".hero-line").textContent = [
    `${n(h.nodes)} nodes in ${n(h.banks)} banks`,
    `${bytes(h.db_size_bytes)} on disk`,
    `schema v${h.schema_version ?? "?"}`,
  ].join(" · ");
  hero.querySelector(".hero-sub").textContent = [
    `memgardend ${h.version}`,
    `up ${duration(h.uptime_ms)}`,
    `embedding ${h.embedding}`,
    `ollama ${h.ollama}`,
  ].join(" · ");
  return h;
}

// --- counters --------------------------------------------------------------

const cardsHost = $("#cards");

/** A definition list of `[label, value, className?]` rows. */
const pairs = (rows) =>
  el(
    "dl",
    { className: "meta" },
    rows.flatMap(([k, v, cls]) => [
      el("dt", {}, k),
      el("dd", { className: cls ?? "" }, v),
    ]),
  );

/** One card: a title and one of those lists. */
const card = (title, rows) =>
  el("article", { className: "card" }, [el("h2", {}, title), pairs(rows)]);

/**
 * AC-2's two recall SLOs, drawn as met or not. The histogram's `under_35ms`
 * and `under_60ms` are exact bucket counts rather than interpolations (the
 * bounds are chosen to land on those boundaries — see core/metrics.rs), so
 * this is a count, not an estimate.
 */
const slo = (hist, key, boundUs, label) => {
  if (!hist || !hist.count) return [label, "—"];
  const value = hist[key];
  const ok = value <= boundUs;
  return [label, `${ms(value)} ${ok ? "✓" : "✗"}`, ok ? "ok" : "bad"];
};

function renderMetrics(m) {
  const recall = m.recall_latency;
  const retain = m.retain_latency;
  const http = m.http_latency;

  cardsHost.replaceChildren(
    card("Recall", [
      ["requests", n(m.recall_requests)],
      ["errors", n(m.recall_errors), m.recall_errors > 0 ? "bad" : ""],
      slo(recall, "p50_us", 35_000, "p50 (AC-2 ≤35 ms)"),
      slo(recall, "p95_us", 60_000, "p95 (AC-2 ≤60 ms)"),
      ["p99", recall?.count ? ms(recall.p99_us) : "—"],
      [
        "within 35 ms",
        recall?.count ? pct(recall.under_35ms, recall.count) : "—",
      ],
      ["memories injected", n(m.recall_injected_memories)],
      ["tokens injected", n(m.recall_injected_tokens)],
      ["reranker", m.reranker_loaded ? "loaded" : "off"],
    ]),

    card("Retain", [
      ["requests", n(m.retain_requests)],
      ["errors", n(m.retain_errors), m.retain_errors > 0 ? "bad" : ""],
      [
        "jobs failed",
        n(m.retain_jobs_failed),
        m.retain_jobs_failed > 0 ? "bad" : "",
      ],
      [
        "chunks failed",
        n(m.retain_chunks_failed),
        m.retain_chunks_failed > 0 ? "warn" : "",
      ],
      ["p50", retain?.count ? ms(retain.p50_us) : "—"],
      ["p95", retain?.count ? ms(retain.p95_us) : "—"],
      ["nodes written", n(m.nodes_written)],
      ["links written", n(m.links_written)],
    ]),

    card("Cap saving", [
      ["tokens in", n(m.retain_tokens_raw)],
      ["after caps", n(m.retain_tokens_capped)],
      ["saved", n(m.retain_tokens_saved)],
      // Both derived fields are null until there has been a retain to derive
      // them from, and that must not render as 0% — "the caps saved nothing"
      // and "nothing has been retained yet" are opposite readings of the
      // same screen. The PRD's target band is a 55-87% reduction, judged by
      // eye rather than pass/fail: one small transcript can sit outside it
      // for reasons that are not a regression.
      [
        "ratio",
        m.retain_saving_ratio == null
          ? "—"
          : `${(100 * m.retain_saving_ratio).toFixed(1)}%`,
      ],
      ["ledger rows", n(m.retain_cap_savings)],
    ]),

    card("Process", [
      ["uptime", duration(m.uptime_ms)],
      ["http requests", n(m.http_requests)],
      ["http errors", n(m.http_errors), m.http_errors > 0 ? "bad" : ""],
      ["http p50", http?.count ? ms(http.p50_us) : "—"],
      ["http p95", http?.count ? ms(http.p95_us) : "—"],
      ["hook invocations", n(m.hook_invocations)],
    ]),
  );
}

// --- the trend -------------------------------------------------------------

const trend = $("#trend");
const TREND_W = 640;
const TREND_H = 90;

/**
 * Recall p50 over the stored snapshots, oldest on the left.
 *
 * Latency rather than a counter, deliberately: every counter in the snapshot
 * is process-lifetime and resets to zero on restart, so a line through them
 * plots restarts more than it plots work. A p50 stays comparable across one.
 *
 * Snapshots with no recall in them are gaps, not zeros — drawing an idle
 * period as 0 ms would read as the fastest the system has ever been.
 */
function renderTrend(rows) {
  const points = rows
    .slice()
    .reverse()
    .map((r) => ({
      at: r.created_at,
      us: r.payload?.recall_latency?.p50_us ?? null,
    }))
    .filter((p) => p.us != null);

  if (points.length < 2) {
    trend.replaceChildren(
      el("p", { className: "empty" }, "Not enough snapshots yet."),
    );
    return;
  }

  const max = Math.max(...points.map((p) => p.us));
  const min = Math.min(...points.map((p) => p.us));
  const span = max - min || 1;
  const x = (i) => (i / (points.length - 1)) * TREND_W;
  const y = (us) => TREND_H - ((us - min) / span) * (TREND_H - 8) - 4;

  const svgEl = (tag, attrs) => {
    const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
    return node;
  };

  const d = points.map((p, i) => `${i ? "L" : "M"}${x(i)} ${y(p.us)}`).join(" ");
  const svg = svgEl("svg", {
    viewBox: `0 0 ${TREND_W} ${TREND_H}`,
    preserveAspectRatio: "none",
    class: "spark",
    role: "img",
    "aria-label": `Recall p50 from ${ms(points[0].us)} to ${ms(points.at(-1).us)}`,
  });
  svg.append(svgEl("path", { d, class: "spark-line" }));
  trend.replaceChildren(
    svg,
    el("p", { className: "spark-scale" }, [
      `${date(points[0].at)} → ${date(points.at(-1).at)}`,
      el("span", {}, ` · ${ms(min)}–${ms(max)}`),
    ]),
  );
}

// --- tables ----------------------------------------------------------------

const table = (headers, rows) =>
  [
    el(
      "thead",
      {},
      el(
        "tr",
        {},
        headers.map((h) => el("th", {}, h)),
      ),
    ),
    el(
      "tbody",
      {},
      rows.length
        ? rows.map((cells) =>
            el(
              "tr",
              {},
              cells.map((c) =>
                el("td", { className: typeof c === "number" ? "num" : "" }, [
                  typeof c === "number" ? n(c) : c,
                ]),
              ),
            ),
          )
        : el(
            "tr",
            {},
            el("td", { colSpan: headers.length, className: "empty" }, "Nothing yet."),
          ),
    ),
  ];

const banksTable = $("#banks");

function renderBanks(stats) {
  banksTable.replaceChildren(
    ...table(
      ["bank", "nodes", "world", "observation", "experience", "unembedded", "docs", "links"],
      stats.map((s) => [
        // A link, because "this bank looks wrong" and "let me look at this
        // bank" are the same thought — and the explorer takes the bank in
        // its query string (E3).
        el("a", { href: `/ui/?bank=${encodeURIComponent(s.bank_id)}` }, s.bank_id),
        s.nodes,
        s.world,
        s.observation,
        s.experience,
        s.unembedded,
        s.documents,
        s.links,
      ]),
    ),
  );
}

const ledgerTable = $("#ledger");

/**
 * What one row *says*, in one column, because the three kinds record three
 * different things and a table with a column per key would be mostly empty:
 *
 *   retain_cap_saving   raw → capped tokens, and the reduction   (automatic)
 *   recall_substitution what an injection replaced               (manual, v1)
 *   manual              a case someone wrote down
 *
 * An unrecognised kind falls back to its raw JSON rather than to a blank —
 * a ledger that quietly omits what it does not recognise is how the flattened
 * response this replaced managed to show eight rows of nothing.
 */
function ledgerSummary(r) {
  const d = r.detail ?? {};
  if (r.kind === "retain_cap_saving" && d.raw_tokens != null) {
    return `${n(d.raw_tokens)} → ${n(d.capped_tokens)} tokens · ${(
      100 * (d.ratio ?? 0)
    ).toFixed(1)}% saved`;
  }
  if (d.case_text) {
    const extra = [
      d.injection_tokens != null && `${n(d.injection_tokens)} injected`,
      d.replaced_tokens_est != null && `${n(d.replaced_tokens_est)} replaced est.`,
    ].filter(Boolean);
    return extra.length ? `${d.case_text} (${extra.join(", ")})` : d.case_text;
  }
  return Object.keys(d).length ? JSON.stringify(d) : "—";
}

function renderLedger(rows) {
  ledgerTable.replaceChildren(
    ...table(
      ["when", "kind", "bank", "what it records", "session"],
      rows.map((r) => [
        date(r.created_at) ?? "—",
        r.kind,
        r.bank_id ?? "—",
        ledgerSummary(r),
        r.detail?.session_id ? r.detail.session_id.slice(0, 8) : "—",
      ]),
    ),
  );
}

// --- E6: the anatomy of a bank ---------------------------------------------
//
// The survey view Phase E planned as a 3D graph, arrived at as numbers.
// `docs/design/e1-memory-explorer.md` §E6 carries the experiment: the live
// bank rendered with a 1.31 MB WebGL renderer is a featureless sphere, and
// every structural fact it failed to show is on this panel.
//
// On demand, not on the 10 s poll — it reads every link in the bank.

const anatomyPick = $("#anatomy-bank");
const anatomyOut = $("#anatomy");
const anatomyStatus = $("#anatomy-status");

/** Keeps the picker in step with whatever `/v1/stats` last returned. */
function syncAnatomyBanks(stats) {
  const chosen = anatomyPick.value;
  anatomyPick.replaceChildren(
    ...stats.map((s) => el("option", { value: s.bank_id }, s.bank_id)),
  );
  if (stats.some((s) => s.bank_id === chosen)) anatomyPick.value = chosen;
}

function renderAnatomy(a) {
  const typeList = (types) =>
    types.map(([t, count]) => `${t} ${n(count)}`).join(" + ");

  // The headline, stated from the numbers rather than from a verdict.
  //
  // An earlier version asked "does every component hold exactly one
  // fact_type?" and answered "no" for the live bank — technically true, and
  // useless: the largest component mixes world and experience *because of a
  // single caused_by edge*. One edge out of 118,937 is not a bank whose types
  // are mixed. So the sentence quotes `cross_type_links` and lets the reader
  // see how thin the join is.
  const headline =
    a.component_count <= 1
      ? `One connected component: every node reaches every other.`
      : `${n(a.cross_type_links)} of ${n(a.links)} links cross a fact_type ` +
        `boundary, so the graph stands in ${n(a.component_count)} components. ` +
        `${n(a.provenance_edges)} provenance rows join them further — and the ` +
        `explorer does not draw those.`;

  anatomyOut.replaceChildren(
    el("div", { className: "anatomy-grid" }, [
      el("div", {}, [
        el("h3", {}, "Shape"),
        pairs([
          ["nodes", n(a.nodes)],
          ["links", n(a.links)],
          ["components", n(a.component_count)],
          ["isolated nodes", n(a.isolated), a.isolated > 0 ? "warn" : ""],
          ["degree p50", n(a.degree.p50)],
          ["degree p90", n(a.degree.p90)],
          ["degree max", n(a.degree.max)],
          ["degree mean", a.degree.mean.toFixed(1)],
        ]),
      ]),
      el("div", {}, [
        el("h3", {}, "Edges"),
        pairs([
          ...a.links_by_type.map(([t, count]) => [
            t,
            `${n(count)} · ${pct(count, a.links)}`,
          ]),
          // The number that says whether the graph is one thing or several.
          [
            "crossing fact_type",
            n(a.cross_type_links),
            a.cross_type_links === 0 ? "warn" : "",
          ],
          // Not links, so the explorer never draws them — which is the point
          // of showing the count here.
          ["provenance (not drawn)", n(a.provenance_edges)],
        ]),
      ]),
    ]),

    el("p", { className: "anatomy-note" }, headline),

    el("h3", {}, `Components (largest ${Math.min(a.components.length, a.component_count)} of ${n(a.component_count)})`),
    el(
      "ul",
      { className: "components" },
      a.components.map((c) =>
        el("li", {}, [
          el("span", { className: "comp-size" }, n(c.size)),
          el("span", { className: "comp-types" }, typeList(c.types)),
          // Width against the largest component, so the split is visible
          // without reading the numbers.
          el("span", {
            className: "comp-bar",
            style: `width:${(100 * c.size) / a.components[0].size}%`,
          }),
        ]),
      ),
    ),
  );
}

async function measureAnatomy() {
  const bank = anatomyPick.value;
  if (!bank) return;
  anatomyStatus.textContent = "measuring…";
  anatomyStatus.className = "";
  const t0 = performance.now();
  try {
    const a = await api(`/v1/banks/${encodeURIComponent(bank)}/anatomy`);
    renderAnatomy(a);
    anatomyStatus.textContent = `${Math.round(performance.now() - t0)} ms`;
  } catch (e) {
    anatomyStatus.textContent = e.message;
    anatomyStatus.className = "bad";
  }
}

$("#anatomy-run").addEventListener("click", measureAnatomy);

// --- the loop --------------------------------------------------------------

const tick = $("#tick");

/**
 * Each section is refreshed independently and a rejection is swallowed per
 * section, so one broken endpoint costs one panel rather than the page. The
 * tick line reports how many failed, because silently stale is the failure
 * mode this whole screen exists to prevent.
 */
async function refresh() {
  const attempts = [
    renderHealth(),
    api("/metrics.json").then(renderMetrics),
    api("/v1/stats").then((stats) => {
      renderBanks(stats);
      syncAnatomyBanks(stats);
    }),
    api("/v1/metrics/history?limit=60").then(renderTrend),
    api("/v1/ledger?limit=20").then(renderLedger),
  ];
  const settled = await Promise.allSettled(attempts);
  const failed = settled.filter((r) => r.status === "rejected").length;

  const at = new Date().toTimeString().slice(0, 8);
  tick.textContent = failed
    ? `updated ${at} · ${failed} of ${attempts.length} failed`
    : `updated ${at} · every ${REFRESH_MS / 1000}s`;
  tick.classList.toggle("bad", failed > 0);
}

refresh();

// A plain interval that runs whether or not the tab is in front.
//
// It skipped hidden tabs at first, to save the ~46 ms `/v1/stats` costs. That
// was measured against the wrong use: this is a screen you leave open on a
// second monitor, and skipping made it stop while still saying "every 10s" —
// a monitoring page that has quietly frozen is worse than one that costs
// 0.5% of a core. Browsers throttle background timers to about a minute on
// their own, which is the only budget this needs.
//
// `visibilitychange` stays, for exactly that throttle: coming back to the tab
// shows current numbers rather than however stale the throttled tick left
// them.
setInterval(refresh, REFRESH_MS);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") refresh();
});
