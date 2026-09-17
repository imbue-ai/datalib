// Bridge: Datalib `grid_rows` (the denormalized union table served by
// `/applet/unified_index/search`) -> DACTAL datasets, and the one channel
// this page has to the outside world.
//
// The page runs in a sandboxed iframe with an opaque origin: no cookie,
// no same-origin `fetch`, no `/api/*`. Rows arrive from the host card
// over `postMessage` (`fetchSearch` below); the host does the fetching
// with the session it holds. The card side is
// `datalib/ui/src/cards/libs/dactalView.ts`, which owns the other half
// of every message shape here.
//
// After loading you MUST call `dactal.survey()` — DACTAL caches the set of
// known dataset names ("destinations") and only refreshes it in survey().
// Without it, autoresolve never fires and `rows.author` stays a bare string.

// Facet columns we re-normalize into their own id-keyed entity datasets.
// `field` is the grid_rows column; `dataset` is the DACTAL dataset name a
// row's value will autoresolve into when you follow that property.
const FACETS = [
  { field: "author", dataset: "author" },
  { field: "channel", dataset: "channel" },
  { field: "source", dataset: "source" },
  { field: "account", dataset: "account" },
  { field: "project", dataset: "project" },
  { field: "org_name", dataset: "org" },
  // conversation: id is the uuid, label is the human name.
  { field: "conversation_uuid", dataset: "conversation", nameField: "conversation_name" },
];

// Turn one SearchRow into a DACTAL item. DACTAL keys items by `id` and
// labels them by `name`; everything else is a followable/filterable/
// groupable property. We map Datalib's `uuid` -> id and pick a sensible
// display name. Empty strings are dropped so DACTAL doesn't create a bogus
// "" entity for every row missing a channel.
function rowToItem(r) {
  const item = { id: r.uuid };
  const name = r.snippet || r.conversation_name || r.uuid;
  if (name) item.name = name;
  for (const [k, v] of Object.entries(r)) {
    if (k === "uuid") continue;
    if (v === "" || v === null || v === undefined) continue;
    item[k] = v;
  }
  // Give conversation a stable, followable handle that matches the
  // `conversation` entity dataset id (uuid), not the display name.
  if (r.conversation_uuid) item.conversation = r.conversation_uuid;
  return item;
}

// Build the id-keyed entity datasets from the rows we already have. Each
// distinct facet value becomes one entity; we attach a `count` and, where
// available, a human `name`. These are what `rows.author`, `rows.channel`,
// etc. resolve to.
function deriveEntities(rows) {
  const out = {};
  for (const f of FACETS) {
    const byId = new Map();
    for (const r of rows) {
      const id = r[f.field];
      if (!id) continue;
      let e = byId.get(id);
      if (!e) {
        e = { id, count: 0 };
        const nm = f.nameField ? r[f.nameField] : id;
        if (nm) e.name = nm;
        byId.set(id, e);
      }
      e.count += 1;
    }
    out[f.dataset] = [...byId.values()];
  }
  return out;
}

// Load a SearchResponse (or a bare rows array) into a DACTAL instance and
// refresh its dataset catalog. Returns a small summary for the UI.
export function loadSearchIntoDactal(dactal, searchResponse) {
  const rows = Array.isArray(searchResponse)
    ? searchResponse
    : searchResponse.rows || [];
  const items = rows.map(rowToItem);
  dactal.load(items, "rows");
  const entities = deriveEntities(rows);
  for (const [name, list] of Object.entries(entities)) {
    if (list.length) dactal.load(list, name);
  }
  dactal.survey(); // CRITICAL: refreshes `destinations` so autoresolve works
  return {
    rows: items.length,
    entities: Object.fromEntries(
      Object.entries(entities).map(([k, v]) => [k, v.length]),
    ),
  };
}

// --- The host channel ------------------------------------------------------

// True when a card is hosting this page. Opened on its own — a top-level
// navigation, a stray link — there is nobody to ask for rows and nothing
// to do, and the page says so instead of evaluating whatever the URL held.
export const hosted = window.parent !== window;

// The target origin is "*" in both directions: this frame's origin is
// opaque, so the host cannot name it, and the host's origin is not
// something a sandboxed page can read. Trust rests on `event.source`
// instead — only the parent window is listened to here, and the host only
// answers its own frame.
function post(msg) {
  window.parent.postMessage(msg, "*");
}

const pending = new Map();
let nextId = 1;
let onInit = null;

window.addEventListener("message", (e) => {
  if (e.source !== window.parent) return;
  const msg = e.data;
  if (!msg || typeof msg !== "object") return;
  if (msg.type === "dactal:init") {
    if (onInit) onInit(msg);
    return;
  }
  const waiting = pending.get(msg.id);
  if (!waiting) return;
  pending.delete(msg.id);
  if (msg.type === "dactal:rows") waiting.resolve({ rows: msg.rows || [] });
  else if (msg.type === "dactal:error")
    waiting.reject(new Error(String(msg.message || "search failed")));
});

// Ask the host for the card's arguments. Resolves with `{load, dq}` once
// the host answers the `ready` announcement; never resolves unhosted.
export function awaitInit() {
  return new Promise((resolve) => {
    onInit = (msg) => resolve({ load: msg.load || "", dq: msg.dq || "" });
    post({ type: "dactal:ready" });
  });
}

// The working set for a Datalib search, fetched by the host with its
// session. Same shape `fetchSearch` in datalib/ui/src/api.ts returns.
export function fetchSearch(q, limit = 500) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    post({ type: "dactal:search", id, q, limit });
  });
}
