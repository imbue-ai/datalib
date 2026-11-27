// One row per rendered document: its identity (uuid, title, provenance)
// and the `renderer_version` that produced it, which is how a renderer
// bump is noticed. `grid_rows.markdown_uuid` points here; many grid rows
// share one document.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One row in the `markdowns` table: one rendered document and the
/// facts about it the grid and the index read. Nothing here is stamped
/// per run: a re-render that produces the same document writes the same
/// row, and doltlite's content-addressed tables then carry no diff for
/// it, which is the whole of how "unchanged" is decided downstream.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "markdowns", primary_key = "markdown_uuid")]
pub struct MarkdownRow {
    /// Stable identifier for one rendered document. For providers
    /// whose native id maps 1:1 to a rendered document (Anthropic
    /// conversation_uuid, Notion page_id) we reuse it verbatim; for
    /// sharded renders (Beeper per-period documents) or ts-keyed providers
    /// (Slack threads) we synthesize a UUIDv5 from the canonical tuple
    /// `grid_rows.uuid` uses. Must be deterministic so re-ingest is
    /// idempotent.
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    /// The id of the source that produced this markdown — its group,
    /// which is the first segment of the producing step's artifact
    /// paths. Never the display name a person gave that group; a name
    /// is mutable and two groups may share one.
    #[col(sql = "VARCHAR(64)")]
    pub source_id: String,
    /// Denormalized provider tag, matches `grid_rows.provider` for the
    /// rows that point at this markdown. Stored here so the markdowns
    /// table is queryable without a join when filtering the sync page.
    #[col(sql = "VARCHAR(32)")]
    pub provider: String,
    /// Markdown-level category. Distinct from `grid_rows.kind` (which is
    /// per-row); this is the shape of the rendered document.
    #[col(sql = "VARCHAR(32)")]
    pub kind: String,
    /// Human-readable title — same value the renderer puts in the
    /// markdown frontmatter / page header. Nullable for sources whose
    /// entities don't have an authored title (e.g. early Slack threads);
    /// the renderer falls back to a snippet of the first message.
    #[col(sql = "TEXT")]
    pub title: Option<String>,
    /// The document row's `grid_rows.created_at`, copied here so the
    /// table answers without a join: when the document came into being,
    /// as the source wrote it (ISO-8601 with explicit offset, per
    /// AGENTS.md) — not when we ingested it.
    #[col(sql = "VARCHAR(40)")]
    pub created_at: Option<String>,
    /// The document row's `grid_rows.modified_at`, likewise: when it last
    /// changed, as the source wrote it.
    #[col(sql = "VARCHAR(40)")]
    pub modified_at: Option<String>,
    /// The document row's `grid_rows.item_count`, likewise, so what a
    /// whole store holds is a sum over this table rather than a scan of
    /// every row. Never NULL from a render: the render store refuses a
    /// document that does not count what it holds.
    #[col(sql = "BIGINT")]
    pub item_count: Option<i64>,
    /// Optional provider-defined cheap-probe value, consulted *before*
    /// loading payloads to decide whether a markdown has changed.
    /// Slack stamps each thread's `MAX(fetched_at_utc)` here so the next run
    /// can skip untouched threads without reading them. NULL for
    /// providers with no such signal.
    #[col(sql = "VARCHAR(64)")]
    pub upstream_cursor: Option<String>,
    /// Opaque version string for the renderer that produced the document.
    /// Bumping this value (typically when the markdown layout or
    /// templating changes) forces a global re-render on the next run.
    #[col(sql = "VARCHAR(32)")]
    pub renderer_version: Option<String>,
    /// The render unit this document was rendered from — the unit the
    /// provider loads, a conversation or a thread or a page — so a
    /// render unit that re-renders to fewer documents can drop the extras.
    /// Every provider's renderer sets it; NULL only on the storage report
    /// (`datalib_step::introspect::plan`), which no render unit produces.
    #[col(sql = "VARCHAR(256)")]
    pub render_unit_key: Option<String>,
    /// blake3 of the document's text, its sections end to end, as hex: what
    /// the search index compares to learn whether it holds this rendering.
    /// NULL from a renderer that declares no sections.
    #[col(sql = "VARCHAR(64)")]
    pub content_hash: Option<String>,
    /// The document's front matter as its renderer handed it, as JSON:
    /// its keys in order, each with its value
    /// (`datalib_etl_render::front_matter::Front`). NULL from a store an
    /// older build wrote.
    #[col(sql = "TEXT")]
    pub front: Option<String>,
    /// The search indexes this document: false for one filed for the grid
    /// alone (Notion's table of contents), which the keyword pass and the
    /// embeddings leave out.
    #[col(sql = "INTEGER")]
    pub searchable: bool,
}
