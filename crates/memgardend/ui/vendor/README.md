# Vendored browser libraries

Committed as built files rather than fetched at runtime. A local-first memory
system that needs the network to draw its own graph is a contradiction, and
`cargo install memgardend` has to yield a working UI with no further steps —
the same reasoning that put the rest of the UI behind `include_str!`
(`routes/ui.rs`).

No bundler and no `package.json` follow from this: these are UMD builds loaded
by `<script>`, and `cargo build` remains the whole build.

| file | package | version | license | sha256 |
|---|---|---|---|---|
| `sigma-3.0.3.min.js` | [sigma](https://www.sigmajs.org/) | 3.0.3 | MIT | `58e30383ab428f83…` |
| `graphology-0.26.0.umd.min.js` | [graphology](https://graphology.github.io/) | 0.26.0 | MIT | `dc337efa23903f61…` |

Fetched 2026-08-11 from `https://unpkg.com/<package>@<version>/dist/…`.
License texts are beside them as `LICENSE-sigma.txt` and
`LICENSE-graphology.txt`; both are MIT and both require the notice to travel
with the code, which is why they are committed rather than linked.

Globals: `Sigma` and `graphology`. Both UMD bundles are self-contained — sigma's
factory takes no arguments, so it does not reach for `graphology` as an external
even though it renders a graphology `Graph`.

## Why two files rather than one

sigma renders; graphology is the graph data structure it renders. They are
separate packages and sigma's dist does not bundle the latter.

## Why this and not d3-force

Measured before choosing, minified: sigma + graphology is **261 KB** against
d3-force plus its three dependencies at **17 KB**. d3-force computes positions
and nothing else, which would have been enough for E3's filtered
neighbourhoods drawn as SVG — the explorer already draws SVG well.

What buys the difference is WebGL rendering, and it was chosen because E3 is
allowed to drop its filters and draw a whole bank. The live one is 5,414 nodes
with over 90,000 semantic edges, and SVG does not stay interactive at that
size. If that capability is ever dropped from E3, this vendor directory should
be revisited rather than inherited.

## Updating

Replace the file, update the version and sha256 in the table above, re-fetch
the license text, and check the globals still resolve. There is no lockfile to
do it for you — that is the cost of having no package manager, and it is
deliberate.
