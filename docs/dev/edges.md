# `edges` — directed links between source and destination anchors

`edges` is an optional Dolt table that stores directed links between
documents (or spans inside documents) discovered during ingest. The
schema is the hand-written `EdgeRow` struct at
`frankweiler/backend/schema/src/edges.rs` (DDL via
`#[derive(PortableTable)]`); the table is created by
`init_schema` in `frankweiler/backend/etl/src/grid_index.rs` and persists in
`<root>/system/backend_index/db.doltlite_db` alongside `grid_rows` and
`markdowns`.

## Data model

One row =
`(src_markdown_uuid, src_anchor_uuid?, dst_markdown_uuid, dst_anchor_uuid?, label?)`.
The src and dst sides are symmetric: each can be either a whole
document (anchor is NULL) or a span inside one (anchor is the value
the renderer baked into the body as `data-section-uuid`). The PK
(`edge_uuid`) is a UUIDv5 over the canonical tuple so re-ingest is
idempotent — the grid_index step deletes-then-inserts every edge whose
`src_markdown_uuid` matches the doc being re-applied.

## Producers today

- **Perseus** (`frankweiler/backend/etl/providers/perseus/`) emits two
  edge flavors per chapter doc:
  - one doc-level edge to the matching chapter in the other language
    (replacing the old inline `*Other:* […]` markdown link). The
    `label` carries the destination's language name ("Greek" /
    "English") because the UI uses it verbatim as the link text — see
    "Label conventions" below.
  - one `bilingual-alignment` edge per bilingual section, anchored on
    the first-word span on each side — a stand-in for a future
    word-level alignment pass.

### Label conventions

The UI's outgoing-destinations list uses `label` as the link text
when present, falling back to the destination markdown's title.
Producers should therefore set `label` to whatever the user should
read in that list — a short human-readable handle, not an
edge-taxonomy tag. The destination doc title appears as a hover
tooltip so it stays discoverable.

Span-source edges (`src_anchor_uuid != null`) don't appear in the
list — they show as inline clickable spans inside the body — so
their `label` is free to be metadata (`bilingual-alignment` for
perseus today) without UI implications.

Other providers leave the table empty; sidecars without an `edges`
field load with zero edges (the serde `default` handles older
sidecars).

## Consumers today

- The backend includes `outgoing_edges` in every
  `GET /api/chat/{markdown_uuid}` response (joined with the
  destination markdown's title for direct rendering).
- `DocCard.ce.vue` shows whole-doc outgoing edges as a list at the top
  of the preview.
- `ChatBody.ce.vue` decorates every `[data-section-uuid]` whose value
  matches an edge's `src_anchor_uuid` with `.edge-source` (subtle
  background, deeper on hover) and a click handler that opens the
  destination column with `dst_anchor_uuid` seeded as the
  scroll-and-highlight target.

## Limitations (current)

These are knowingly punted for the proof of concept:

1. **Overlapping span sources are not specially handled.** If the
   renderer emits two `<span data-section-uuid="X">` and
   `<span data-section-uuid="Y">` whose text content overlaps, both
   get decorated independently — the resulting nested CSS may look
   odd. Producers (today: only perseus) are expected to avoid
   overlap; future producers should too.
2. **Multiple outgoing edges per source span: only the first is
   exposed.** `ChatBody`'s lookup is `(src_anchor_uuid → first
   matching EdgeOut)`. If a single span uuid carries two outgoing
   edges, the user sees a click handler for one of them. The picker
   is "first in `outgoing_edges` array order"; the backend orders by
   insertion, which for perseus is currently doc-level then
   bilingual-alignment.
3. **`label` is stored but not rendered.** The doc-level destination
   list shows it as a parenthetical when present, but there's no
   filtering, grouping, or icon-mapping behavior keyed off it yet.
4. **No incoming-edges view.** `outgoing_edges` is computed by
   `src_markdown_uuid`; we don't currently surface "who points at this
   doc" in the UI. The data is there — query
   `WHERE dst_markdown_uuid = ?` — but no consumer is wired up.

When extending this, please update the bullets above so the next
contributor can see what's still missing.
