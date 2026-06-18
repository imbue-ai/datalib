// Bridge: Frankweiler `grid_rows` (the denormalized union table served by
// `/api/search`) -> DACTAL datasets.
//
// Frankweiler deliberately denormalizes everything onto one row per
// displayable thing (see docs/dev/grid_rows.md). DACTAL, by contrast,
// shines when entities cross-reference each other by id: given a dataset
// named `author` whose ids are author names, DACTAL's `autoresolve`
// feature turns `rows.author` into a JOIN to the author entity, so you can
// write `rows.author.team` or `rows.conversation.channel`.
//
// So this bridge does two things:
//   1. Loads the flat rows as the `rows` dataset (1:1 with grid_rows).
//   2. Re-normalizes a handful of facet columns back into entity datasets
//      (author, channel, source, account, project, conversation, org) whose
//      id == the facet value. That re-lights DACTAL's relational joins on
//      top of a table that was flattened for Frankweiler's own SQL path.
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
// groupable property. We map Frankweiler's `uuid` -> id and pick a sensible
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

// Fetch from Frankweiler's HTTP API. In dev this is proxied to the Rust
// backend; in Tauri/openhost the same relative path is served by the
// embedded backend. Mirrors `fetchSearch` in frankweiler/ui/src/api.ts.
export async function fetchSearch(q, limit = 500) {
  const params = new URLSearchParams({ q, limit: String(limit) });
  const r = await fetch(`/api/search?${params.toString()}`);
  if (!r.ok) throw new Error(`/api/search -> ${r.status}: ${await r.text()}`);
  return r.json();
}
