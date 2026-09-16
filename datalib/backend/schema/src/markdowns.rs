// Per-rendered-markdown metadata + render bookkeeping. One row per
// `.md` file in `<root>/render_markdown/`. Owns the file's identity (UUID +
// title + provenance) and the `renderer_version` that produced it, which
// is how a renderer bump is noticed. `grid_rows.markdown_uuid` is the FK
// pointing here;
// many grid rows can share one markdown file. Note that a single
// 'conversation' upstream can shard into many markdowns when a provider
// renders one file per period (beeper) — the `markdowns` table is keyed
// on the rendered file, not the abstract conversation.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One row in the `markdowns` table: one rendered `.md` file and the
/// facts about it the grid and the index read. Nothing here is stamped
/// per run: a re-render that produces the same document writes the same
/// row, and doltlite's content-addressed tables then carry no diff for
/// it, which is the whole of how "unchanged" is decided downstream.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "markdowns", primary_key = "markdown_uuid")]
pub struct MarkdownRow {
    /// Stable identifier for one rendered `.md` file. For providers
    /// whose native id maps 1:1 to a rendered file (Anthropic
    /// conversation_uuid, Notion page_id) we reuse it verbatim; for
    /// sharded renders (Beeper per-period files) or ts-keyed providers
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
    /// per-row); this is the shape of the rendered file.
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
    /// Path to the rendered markdown file, relative to the **data
    /// root** — `<stanza>/render_markdown/...`, derived by
    /// `grid_index::apply_one` stripping the data root off the absolute
    /// path the renderer wrote. NULL until the renderer has produced
    /// output. The backend's `/api/chat/{markdown_uuid}` endpoint
    /// resolves this column to find the file to serve.
    #[col(sql = "VARCHAR(1024)")]
    pub md_path: Option<String>,
    /// Optional provider-defined cheap-probe value, consulted *before*
    /// loading payloads to decide whether a markdown has changed.
    /// Slack stamps each thread's `MAX(fetched_at_utc)` here so the next run
    /// can skip untouched threads without reading them. NULL for
    /// providers with no such signal.
    #[col(sql = "VARCHAR(64)")]
    pub upstream_cursor: Option<String>,
    /// Opaque version string for the renderer that produced `md_path`.
    /// Bumping this value (typically when the markdown layout or
    /// templating changes) forces a global re-render on the next run.
    #[col(sql = "VARCHAR(32)")]
    pub renderer_version: Option<String>,
    /// The bucket this document was rendered from — the unit the
    /// provider loads, a conversation or a thread or a page — so a
    /// bucket that re-renders to fewer documents can drop the extras.
    /// NULL from a renderer that has not been ported to declare buckets.
    #[col(sql = "VARCHAR(256)")]
    pub bucket_key: Option<String>,
}
