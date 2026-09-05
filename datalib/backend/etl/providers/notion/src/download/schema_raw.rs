//! Raw-store schema for the Notion provider.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw as dr;
use datalib_etl_macros::CasEdgeRow;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &[
    "pages",
    "blocks",
    "databases",
    "users",
    "comments",
    "notion_image_attachments",
];

/// `pages` — one row per Notion page UUID.
///
/// PK choice: upstream Notion page UUID (dashed v4 form normalized by
/// `download::format_uuid`).
pub const PAGES_DDL: &str = "CREATE TABLE IF NOT EXISTS pages (
    id TEXT PRIMARY KEY,
    parent_id TEXT NULL,
    last_edited_time TEXT NULL,
    payload TEXT NULL
)";

/// Index on `pages.last_edited_time` — supports the listing-derived
/// skip-check and any future "pages edited since X" cursor work.
pub const PAGES_LAST_EDITED_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS pages_last_edited ON pages(last_edited_time)";

/// `blocks` — one row per Notion block UUID.
///
/// PK choice: upstream Notion block UUID. `page_order` is local layout
/// metadata, not part of the PK; the same block may re-arrange upstream
/// and we want the row at the same UUID with the column updating.
pub const BLOCKS_DDL: &str = "CREATE TABLE IF NOT EXISTS blocks (
    id TEXT PRIMARY KEY,
    parent_id TEXT NULL,
    page_id TEXT NULL,
    page_order INTEGER NULL,
    last_edited_time TEXT NULL,
    payload TEXT NULL
)";

/// Index on `blocks(page_id, page_order)` — supports the per-page child
/// join in BFS / render order without a full-table sort.
pub const BLOCKS_PAGE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS blocks_page ON blocks(page_id, page_order)";

/// `databases` — one row per Notion database UUID.
///
/// PK choice: upstream Notion database UUID.
pub const DATABASES_DDL: &str = "CREATE TABLE IF NOT EXISTS databases (
    id TEXT PRIMARY KEY,
    parent_id TEXT NULL,
    last_edited_time TEXT NULL,
    payload TEXT NULL
)";

/// `users` — one row per Notion user UUID surfaced anywhere in the
/// mirror (page author, comment author, mention, …).
///
/// PK choice: upstream Notion user UUID.
pub const USERS_DDL: &str = "CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    payload TEXT NULL
)";

/// `comments` — one row per Notion page / block comment UUID.
///
/// PK choice: upstream Notion comment UUID.
pub const COMMENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS comments (
    id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL,
    page_id TEXT NULL,
    payload TEXT NULL
)";

/// Index on `comments.page_id` — supports the per-page child join.
pub const COMMENTS_PAGE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS comments_page ON comments(page_id)";

/// `notion_image_attachments` — N:M edge between one Notion image
/// block and a `cas_objects` blob. Replaces this provider's use of
/// the shared `blob_refs` table. Universal CAS-edge shape:
/// `id` (synth `"{block_id}#{ref_id}"`), owning FK (`block_id`,
/// indexed so per-page loads on the render side stay cheap), upstream
/// ref (`ref_id`, also indexed for the `blake3 IS NOT NULL`
/// skip-check), `blake3` (null until the CAS write lands). See
/// [`datalib_etl::blob_cas::CasEdgeRow`].
///
/// PK choice: the four-field synthesized PK every CAS edge table
/// uses (`{owning_id}#{ref_id}`). For Notion the only attachment
/// shape today is image blocks, where `ref_id = "{block_uuid}:image"`
/// — one image per block, so `(block_id, ref_id)` is effectively
/// 1:1 in practice. The two-column edge shape stays for symmetry
/// with the other providers; a future audio/video block would
/// slot in as a different `ref_id` suffix without re-shaping.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "notion_image_attachments")]
pub struct NotionImageAttachmentRow {
    pub id: String,
    pub block_id: String,
    pub ref_id: String,
    pub blake3: Option<String>,
}

/// Compose the full DDL list passed to
/// [`datalib_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, the
/// [`NotionImageAttachmentRow`] edge-table DDLs, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        PAGES_DDL.to_string(),
        PAGES_LAST_EDITED_INDEX_DDL.to_string(),
        BLOCKS_DDL.to_string(),
        BLOCKS_PAGE_INDEX_DDL.to_string(),
        DATABASES_DDL.to_string(),
        USERS_DDL.to_string(),
        COMMENTS_DDL.to_string(),
        COMMENTS_PAGE_INDEX_DDL.to_string(),
    ];
    out.extend(NotionImageAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
