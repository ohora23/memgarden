# AC-4 — the render benchmark, finally taken

AC-4 asks that **≥2,500 nodes** stay smooth under pan, zoom, drag and hover.
Every *feature* it names shipped in E1–E4 — filters, progressive expansion,
SSE inside 5 s — but the number itself was never measured, so the box stayed
empty through Phase E and into Phase F. This is the measurement.

Run 2026-08-19 against the live daemon on `:9100`, the largest live bank.

## What was measured

| | |
|---|---|
| graph | **3,200 nodes · 57,890 edges** (world 1,992 · observation 1,178 · experience 30) |
| layout | d3-force settled, 224 ticks — the state a user actually pans, not the seeded one |
| GPU | NVIDIA RTX 5080 via ANGLE, Chrome 151, `devicePixelRatio` 1, 1920×927 |

Reaching 3,200 needed one step outside the UI: the server caps `limit` at
`MAX_LIMIT = 2000` (`routes/graph.rs:18`), so the explorer's filter alone
cannot exceed 2,000 and only double-click expansion gets past it. The
remaining 1,200 nodes of the same bank were fetched through the same
`GET /v1/banks/{id}/graph?ids=…` the explorer uses for exactly this, given the
attribute shape the app had already applied to the first 2,000, and laid out
by the same force configuration (`app.js:572-581`). It is the app's data, the
app's edges and the app's layout — assembled by hand because the UI has no
control that asks for all of it at once.

## Frame cost

Per-frame **draw** cost, camera moved every frame, `gl.finish()` before the
clock stops so the GPU is actually done:

| gesture | nodes | p50 | p95 | max |
|---|---|---|---|---|
| pan | 2,000 | 2.1 ms | 3.6 ms | 19.1 ms¹ |
| zoom | 2,000 | 2.2 ms | 4.4 ms | 4.8 ms |
| pan + zoom | 2,000 | 2.2 ms | 4.3 ms | 4.5 ms |
| **pan** | **3,200** | **2.9 ms** | **4.6 ms** | 5.6 ms |
| **zoom** | **3,200** | **2.9 ms** | **4.7 ms** | 5.8 ms |
| **pan + zoom** | **3,200** | **3.6 ms** | **5.3 ms** | 5.6 ms |

¹ first frame of the run, before the buffers are warm; it does not recur.

**A 60 fps budget is 16.7 ms.** The worst p95 at 3,200 nodes is 5.3 ms — a
**3.2× margin**, and the cost scales with edges roughly linearly (37k → 58k
edges moves p50 2.2 → 3.6 ms).

Hover is the CPU half — sigma hit-tests every `mousemove`. Over 120 moves
scattered across the graph at 3,200 nodes: **p50 0.10 ms, p95 0.40 ms, max
2.6 ms**, and the peek panel opened on 13 of them, so the hit-testing was
doing real work rather than missing everything.

"Drag" in AC-4 is camera drag: the app implements no node dragging
(`app.js` binds `clickNode` and `doubleClickNode` only), so dragging renders
through the same path as pan and is covered by the pan row.

## The honest caveat

The tab was **hidden** for the whole run — this environment gives no focused
window, and `requestAnimationFrame` throttles to roughly 1 fps in a hidden
tab. The first attempt measured that throttle and reported single-digit fps;
those numbers were discarded, not published.

So what the table reports is the renderer's **synchronous draw cost with the
GPU forced to finish**, not observed frame cadence. It excludes compositing
and vsync. That is the right quantity for the criterion — AC-4 was already
reframed in `docs/design/e1-memory-explorer.md` §Decisions 3 as "the number
the renderer must sustain" — but it is not the same as watching it, and a
number this far inside budget should be read as "the renderer is not the
constraint" rather than as a measured frame rate.

## What is actually slow, and it is not the renderer

The layout is d3-force on the main thread:

| | 3,200 nodes / 57,890 edges |
|---|---|
| one tick | **13 ms** p50, 92 ms on the first (link initialisation) |
| settle | **224 ticks, 6.1 s** at `alpha 0.9`, `alphaDecay 0.03` |
| through the UI at 2,000 nodes | **1.99 s** from *Apply* to settled |

While it settles, every tick blocks the main thread for ~13 ms against a
16.7 ms frame — so during those seconds the *layout*, not the draw, is what a
gesture competes with. It is bounded: the simulation stops, and afterwards a
gesture costs the 3.6 ms in the table above.

**This is the ceiling worth knowing.** The renderer has 3× headroom at 3,200
nodes; the layout has none to spare at that size and is on the same thread as
the interaction. If AC-4 is ever pushed to five figures, the work is moving
d3-force to a worker, not replacing sigma.

## Verdict

**AC-4's rendering benchmark is met at 3,200 nodes**, 28% above the 2,500 it
asks for, on the largest bank that exists rather than a synthetic graph. The
criterion's other clauses (retain visible ≤5 s, session/bank/type filters)
shipped in E3 and E4.
