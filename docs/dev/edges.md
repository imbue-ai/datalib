# `edges` — directed links between source and destination anchors

`edges` stores directed links between documents, or between spans
inside documents. A renderer returns a document's outgoing edges on
`RenderedMarkdown::edges`; they are stored in the source's render store
beside its rows, and the `unified_grid_and_search_index` step copies
them into the grid index,
`<root>/unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite`,
beside `grid_rows` and `markdowns`. The row is the `EdgeRow` struct in
`datalib/backend/schema/src/edges.rs`.

## Data model

One row =
`(src_markdown_uuid, src_anchor_uuid?, dst_markdown_uuid, dst_anchor_uuid?, label?, kind)`.
The src and dst sides are symmetric: each can be a whole document
(anchor is NULL) or a span inside one (anchor is the value the renderer
baked into the body as `data-section-uuid`). The primary key,
`edge_uuid`, is `datalib_id::edge_id` over that tuple less `kind`, so
re-rendering is idempotent. Its leading bits are its source end's
stamp (the anchor's, else the document's), so an edge is stored beside
the row it leaves from. A document owns the edges whose
`src_markdown_uuid` is its own: each time the document is stored, its
old set is replaced.

`kind` says what the edge means, an `EdgeKind` stored as its number:
`Link` (0), a link the page draws, or `ReplyTo` (1), an
item that answers another. A number is never reused.

## Producers

**chat-common** writes a `ReplyTo` edge from each message that answers
another in the same document (`NormalizedChatItem::reply_to`): a
quote-reply, an email's `In-Reply-To`. Both anchors are messages.

**calendar-common** links a recurring series to each changed
occurrence, and the occurrence back to its series (`Link`, labelled
with the occurrence's or series' title).

**Perseus** (`datalib/backend/etl/providers/perseus_render/`,
`chapter_edges`) writes `Link` edges for an edition named in an
`alignment_pairs` entry. Per chapter document it emits:

- one doc-level edge to the same chapter in each counterpart edition.
  Its `label` is the counterpart edition's short id, which the UI shows
  as the link text (see "Label conventions").
- one `bilingual-alignment` edge per aligned sentence pair, from the
  sentence span on this side to the sentence span on the other.

### Label conventions

The list of destinations at the top of a document shows `label` as the
link text, else the destination's title, else its bare uuid; when label
and title are both set and differ, the title follows in parentheses,
and it is the hover too. So set `label` to what a person should read
there: a short handle, not a taxonomy tag.

Span-source edges (`src_anchor_uuid` set) are not listed; they show as
clickable spans in the body, so their `label` can be metadata
(`bilingual-alignment`).

## Consumers

- `GET /applet/unified_grid_and_search_index/chat/{markdown_uuid}` returns
  the document's `Link` edges as `outgoing_edges`, each with the
  destination's title (`dst_title`). `ReplyTo` edges are not returned.
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
