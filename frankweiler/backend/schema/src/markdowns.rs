// Per-rendered-markdown metadata + render bookkeeping. One row per
// `.md` file in `<root>/rendered_md/`. Owns the file's identity (UUID +
// title + provenance) and the cache key (`row_set_hash` +
// `renderer_version`) used by incremental ingest to decide whether to
// re-emit the file. `grid_rows.markdown_uuid` is the FK pointing here;
// many grid rows can share one markdown file. Note that a single
// 'conversation' upstream can shard into many markdowns when a provider
// renders one file per period (beeper) — the `markdowns` table is keyed
// on the rendered file, not the abstract conversation.
//
// Hand-written row struct; the `CREATE TABLE` DDL + column metadata are
// derived from it by `#[derive(PortableTable)]`.

use frankweiler_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One row in the `markdowns` table. Source of truth for
/// `<root>/rendered_md/<...>.md` cache invalidation: ingest computes a
/// fresh `row_set_hash` from the canonical grid_row tuples for this
/// markdown file and compares it to the stored value; on mismatch the
/// renderer re-emits the file and bumps `rendered_at`. A bump to
/// `renderer_version` invalidates every cache entry at once.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
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
    /// `sources[].name` from `config.yaml` that produced this markdown.
    /// Lets the sync UI show per-source delta counts and the worker
    /// scope re-renders to a single source.
    #[col(sql = "VARCHAR(64)")]
    pub source_name: String,
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
    #[col(sql = "VARCHAR(512)")]
    pub title: Option<String>,
    /// Earliest authored timestamp for content in this markdown (ISO-8601
    /// with explicit offset, per AGENTS.md). Sourced from the underlying
    /// provider — not when we ingested it.
    #[col(sql = "VARCHAR(40)")]
    pub created_at: Option<String>,
    /// Latest authored timestamp for content in this markdown (ISO-8601
    /// with explicit offset). Drives the sync page's `Last updated`
    /// column.
    #[col(sql = "VARCHAR(40)")]
    pub updated_at: Option<String>,
    /// Path to the rendered markdown file, relative to
    /// `<root>/rendered_md/`. NULL until the renderer has produced
    /// output. The backend's `/api/chat/{markdown_uuid}` endpoint
    /// resolves this column to find the file to serve.
    #[col(sql = "VARCHAR(1024)")]
    pub md_path: Option<String>,
    /// SHA-256 (hex) over the canonical tuple list of grid_rows that feed
    /// this markdown — message texts, authors, timestamps, attachments.
    /// Computed by ingest; if it matches the stored value and
    /// `renderer_version` is unchanged, the renderer skips this markdown.
    /// The canonical tuple definition is part of the renderer contract;
    /// bump `renderer_version` if you change it.
    #[col(sql = "CHAR(64)")]
    pub row_set_hash: String,
    /// Opaque version string for the renderer that produced `md_path`.
    /// Bumping this value (typically when the markdown layout or
    /// templating changes) invalidates every markdowns row's cache and
    /// forces a global re-render on the next ingest.
    #[col(sql = "VARCHAR(32)")]
    pub renderer_version: String,
    /// When `md_path` was last written (ISO-8601 with explicit local
    /// offset, per AGENTS.md). NULL before the first render.
    #[col(sql = "VARCHAR(40)")]
    pub rendered_at: Option<String>,
}
