# `edges` — directed links between source and destination anchors

`edges` stores directed links between documents, or between spans
inside documents. The schema is the `EdgeRow` struct in
`datalib/backend/schema/src/edges.rs` (DDL via
`#[derive(PortableTable)]`). A renderer returns a document's outgoing
edges on `RenderedMarkdown::edges`; they are stored in the source's
render store beside its rows, and the `grid_index` step copies them into
`<root>/unified_index/grid_index/db.doltlite_db` beside `grid_rows` and
`markdowns` (`init_schema` in
`datalib/backend/etl/render/src/grid_index.rs` creates the table).

## Data model

One row =
`(src_markdown_uuid, src_anchor_uuid?, dst_markdown_uuid, dst_anchor_uuid?, label?)`.
The src and dst sides are symmetric: each can be a whole document
(anchor is NULL) or a span inside one (anchor is the value the renderer
baked into the body as `data-section-uuid`). The primary key
(`edge_uuid`) is `datalib_id::edge_id` over that tuple, so re-rendering
is idempotent. A document owns the edges whose `src_markdown_uuid` is
its own: the `grid_index` step deletes them and inserts the new set
each time it loads that document. The id carries the stamp of its
source end; see [`entity_ids.md` § "The layout"](entity_ids.md#the-layout-the-stamp-first-then-the-hash).

## Producers

Only **Perseus** (`datalib/backend/etl/providers/perseus_render/`,
`chapter_edges`) writes edges, and only for an edition named in an
`alignment_pairs` entry. Per chapter document it emits:

- one doc-level edge to the same chapter in each counterpart edition.
  Its `label` is the counterpart edition's short id, which the UI shows
  as the link text (see "Label conventions").
- one `bilingual-alignment` edge per aligned sentence pair, from the
  sentence span on this side to the sentence span on the other.

Every other provider leaves the table empty.

### Label conventions

The destinations list at the top of a document uses `label` as the link
text when present, then the destination's title, then its bare uuid.
When both label and title are set and differ, the title follows in
parentheses; the title is also the link's hover tooltip. So set `label`
to what a person should read in that list — a short handle, not a
taxonomy tag.

Span-source edges (`src_anchor_uuid` set) are not listed; they show as
clickable spans in the body, so their `label` can be metadata
(`bilingual-alignment`).

## Consumers

- `GET /applet/unified_index/chat/{markdown_uuid}` returns
  `outgoing_edges`, each joined with the destination's title
  (`dst_title`).
- `DocCard.ce.vue` lists the whole-doc outgoing edges at the top of the
  preview.
- `ChatBody.ce.vue` marks every `[data-section-uuid]` that matches an
  edge's `src_anchor_uuid` with `.edge-source`. A click opens the
  destination card with `dst_anchor_uuid` as the scroll-and-highlight
  target; hovering lights the destination span in any open card that
  shows it.

## Limitations

1. **Overlapping span sources are not handled.** Two decorated spans
   whose text overlaps are each decorated on their own, and the nested
   styling may look odd. Producers should avoid overlap.
2. **One edge per source span.** `ChatBody` keeps the first edge per
   `src_anchor_uuid` in `outgoing_edges` order, and the query has no
   `ORDER BY`, so which one wins is unspecified. A span with several
   outgoing edges links to one of them.
3. **`label` drives nothing but the link text.** No filtering, grouping
   or icon is keyed off it.
4. **No incoming-edges view.** The data is there
   (`WHERE dst_markdown_uuid = ?`), but no endpoint or card reads it.

Update this list when you lift one of these.
