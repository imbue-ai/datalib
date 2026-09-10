//! Raw-store schema for the Notion provider.
//!
//! There is no `blocks` table. A page's body is fetched already rendered
//! from `GET /v1/pages/{id}/markdown` and stored whole in
//! `page_markdown`, so the block tree is never mirrored.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw as dr;
use datalib_etl_macros::CasEdgeRow;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &[
    "pages",
    "page_markdown",
    "comments",
    "comment_anchors",
    "users",
    "notion_attachments",
];

/// `pages` — one row per Notion page UUID, holding the page *object*:
/// properties, parent, icon, cover, timestamps. For a database row —
/// most of a typical workspace — this is the whole record, because such
/// a page usually has no body at all.
///
/// PK choice: upstream Notion page UUID, dashed.
pub const PAGES_DDL: &str = "CREATE TABLE IF NOT EXISTS pages (
    id TEXT PRIMARY KEY,
    parent_type TEXT NULL,
    parent_id TEXT NULL,
    in_trash INTEGER NOT NULL DEFAULT 0,
    created_time TEXT NULL,
    last_edited_time TEXT NULL,
    url TEXT NULL,
    payload TEXT NULL
)";

pub const PAGES_LAST_EDITED_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS pages_last_edited ON pages(last_edited_time)";

pub const PAGES_PARENT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS pages_parent ON pages(parent_id)";

/// `page_markdown` — the page body, as Notion's own enhanced markdown.
///
/// Its own table rather than a column on `pages` for two reasons:
/// `dolt_diff_page_markdown` then means exactly "the body changed",
/// distinct from "a property changed"; and the body is large, so
/// keeping it out leaves the properties diff cheap.
///
/// **The stored markdown never contains a signed URL.** Notion mints a
/// fresh `?X-Amz-…` signature on every fetch, so storing what the API
/// returned would make an unchanged page differ from itself on every
/// run and defeat incremental render. Every file URL is rewritten to
/// its slot — scheme + host + path — before it lands here. See
/// `ingest::slots`.
///
/// PK choice: the page UUID this is the body of.
pub const PAGE_MARKDOWN_DDL: &str = "CREATE TABLE IF NOT EXISTS page_markdown (
    id TEXT PRIMARY KEY,
    markdown TEXT NULL,
    truncated INTEGER NOT NULL DEFAULT 0,
    unresolved_block_ids TEXT NULL,
    source_last_edited_time TEXT NULL
)";

/// `comments` — one row per Notion comment UUID.
///
/// One `GET /v1/comments?block_id={page_id}` returns a whole page's
/// discussions, including threads anchored to blocks inside it, so
/// `page_id` is the page the comment was collected for and `parent_id`
/// is the block (or page) it actually hangs off.
///
/// PK choice: upstream Notion comment UUID.
pub const COMMENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS comments (
    id TEXT PRIMARY KEY,
    discussion_id TEXT NULL,
    parent_type TEXT NULL,
    parent_id TEXT NULL,
    page_id TEXT NULL,
    created_time TEXT NULL,
    last_edited_time TEXT NULL,
    payload TEXT NULL
)";

pub const COMMENTS_PAGE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS comments_page ON comments(page_id)";

pub const COMMENTS_DISCUSSION_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS comments_discussion ON comments(discussion_id)";

/// `users` — one row per Notion user seen anywhere in the mirror.
///
/// Filled lazily, one `GET /v1/users/{id}` per id we have not seen
/// before. It has to be lazy: `GET /v1/users` (list all) is not
/// available to personal access tokens, which is what this provider
/// authenticates with. Comment authors do not depend on this — Notion
/// resolves those on the comment itself — but a page's `created_by`
/// carries only an id.
///
/// PK choice: upstream Notion user UUID.
pub const USERS_DDL: &str = "CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    name TEXT NULL,
    payload TEXT NULL
)";

/// `comment_anchors` — the text a block-anchored comment hangs off.
///
/// A comment names a `block_id` and carries no quoted text, so without
/// this a thread's anchor is an opaque uuid. Filled by fetching
/// `GET /v1/blocks/{id}` for **commented blocks only** — one request per
/// commented block, not per block, which is a small number in practice
/// (11 across 25 pages in a measured workspace).
///
/// PK choice: the block UUID the comment is parented by.
pub const COMMENT_ANCHORS_DDL: &str = "CREATE TABLE IF NOT EXISTS comment_anchors (
    id TEXT PRIMARY KEY,
    page_id TEXT NULL,
    block_type TEXT NULL,
    plain_text TEXT NULL
)";

/// `notion_attachments` — N:M edge between a page and a `cas_objects`
/// blob.
///
/// `ref_id` is the attachment's **slot**: the unsigned URL, query
/// string discarded. That is the one identifier upstream keeps stable —
/// the signature rotates hourly and the bytes can be replaced in place
/// — so it is what both the stored markdown and this edge are keyed on.
/// `blake3` is null until the CAS write lands, which is what lets a
/// failed blob fetch leave correct, stable markdown behind.
///
/// PK choice: the synthesized `{owning_id}#{ref_id}` every CAS edge
/// table uses.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "notion_attachments")]
pub struct NotionAttachmentRow {
    pub id: String,
    pub page_id: String,
    pub ref_id: String,
    pub blake3: Option<String>,
}

/// Compose the full DDL list passed to
/// [`datalib_etl::doltlite_raw::open`].
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        PAGES_DDL.to_string(),
        PAGES_LAST_EDITED_INDEX_DDL.to_string(),
        PAGES_PARENT_INDEX_DDL.to_string(),
        PAGE_MARKDOWN_DDL.to_string(),
        COMMENTS_DDL.to_string(),
        COMMENTS_PAGE_INDEX_DDL.to_string(),
        COMMENTS_DISCUSSION_INDEX_DDL.to_string(),
        COMMENT_ANCHORS_DDL.to_string(),
        USERS_DDL.to_string(),
    ];
    out.extend(NotionAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
