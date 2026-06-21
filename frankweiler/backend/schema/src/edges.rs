// Directed edges between source and destination anchors. An anchor is
// either a whole rendered markdown document (identified by
// markdown_uuid) or a span inside one (identified by markdown_uuid +
// anchor_uuid, where anchor_uuid is the same value emitted by the
// renderer as `data-section-uuid` on the wrapping element). Edges are
// how frankweiler records discovered linkages between documents — e.g.
// URL-in-slack-message → notion-page, or first-word-of-greek-section ↔
// first-word-of-english-section. The table is intentionally generic:
// src and dst are symmetric in shape (both can be whole docs or spans),
// even though current use cases all use span sources. New ingests are
// expected to populate this table; older ingests that pre-date the
// table simply leave it empty (the schema is purely additive).
//
// Hand-written row struct; the `CREATE TABLE` DDL + column metadata are
// derived from it by `#[derive(PortableTable)]`. This struct is the
// single source of truth for the column names, types, and shape.

use frankweiler_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One directed edge from a source anchor to a destination anchor. Both
/// anchors are described by (markdown_uuid, anchor_uuid?) where the
/// anchor_uuid is null for whole-document anchors. The optional `label`
/// carries a free-form discriminator (e.g. 'cross-language',
/// 'url-target') that producers may set; the current UI does not render
/// it.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "edges", primary_key = "edge_uuid")]
pub struct EdgeRow {
    /// Stable identifier for one edge. Producers SHOULD derive it as a
    /// UUIDv5 over the canonical tuple (src_markdown_uuid,
    /// src_anchor_uuid, dst_markdown_uuid, dst_anchor_uuid, label) so
    /// re-ingest is idempotent. The renderer bakes this value into the
    /// source markdown body as `data-edge-id` on the wrapping span (when
    /// src_anchor_uuid is set) so the UI's click handler can resolve it
    /// back to a destination.
    #[col(sql = "VARCHAR(96)")]
    pub edge_uuid: String,
    /// FK into `markdowns.markdown_uuid` — the rendered document the
    /// source anchor lives in. The Load step deletes-then-inserts edges
    /// keyed by this column when a markdown is re-rendered, so this
    /// column also drives cache invalidation.
    #[col(sql = "VARCHAR(96)")]
    pub src_markdown_uuid: String,
    /// Identifies the source anchor inside `src_markdown_uuid`. NULL
    /// means the source is the whole document. Otherwise this value MUST
    /// match a `data-section-uuid` attribute the renderer emits in the
    /// markdown body (either an existing grid_row section anchor, or a
    /// sub-section span the renderer wraps specifically for this edge).
    #[col(sql = "VARCHAR(96)")]
    pub src_anchor_uuid: Option<String>,
    /// FK into `markdowns.markdown_uuid` — the rendered document the
    /// destination anchor lives in. Cross-document edges have dst != src;
    /// same-document edges (none today, but legal) have dst == src.
    #[col(sql = "VARCHAR(96)")]
    pub dst_markdown_uuid: String,
    /// Identifies the destination anchor inside `dst_markdown_uuid`. NULL
    /// means the destination is the whole document and the UI just opens
    /// it. When set, the UI passes this value as `selectedSectionUuid` so
    /// ChatBody.vue scrolls to and highlights `[data-section-uuid=...]` —
    /// the same mechanism grid_row clicks already use.
    #[col(sql = "VARCHAR(96)")]
    pub dst_anchor_uuid: Option<String>,
    /// Optional free-form label describing the edge's semantics (e.g.
    /// 'cross-language', 'url-target', 'bilingual-alignment'). No use
    /// case requires a label today; the column exists so future
    /// consumers can filter or style by it.
    #[col(sql = "VARCHAR(64)")]
    pub label: Option<String>,
}
