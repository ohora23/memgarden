// What the explorer and the dashboard both need. Extracted at E5 rather
// than invented for it: all three of these were already in `app.js` and the
// dashboard would otherwise have carried a second copy to drift from.

export const $ = (sel) => document.querySelector(sel);

/** Everything user-supplied goes through here. No innerHTML with data in it. */
export const el = (tag, props = {}, kids = []) => {
  const node = Object.assign(document.createElement(tag), props);
  for (const kid of [].concat(kids)) {
    node.append(kid?.nodeType ? kid : document.createTextNode(kid));
  }
  return node;
};

export async function api(path, init) {
  const res = await fetch(path, {
    headers: { "content-type": "application/json" },
    ...init,
  });
  if (!res.ok) {
    throw new Error(`${res.status} ${(await res.text()).slice(0, 200)}`);
  }
  return res.json();
}

export const date = (ms) =>
  ms == null ? null : new Date(ms).toISOString().slice(0, 16).replace("T", " ");
