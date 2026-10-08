//! Raw-store schema for the Notion provider.
//!
//! There is no `blocks` table. A page's body is fetched already rendered
//! from `GET /v1/pages/{id}/markdown` and stored whole in
//! `page_markdown`, so the block tree is never mirrored.
//!
//! What a table holds, and at what version, is `held_version` in its
//! `_bookkeeping` sidecar (`datalib_etl_web::owed`): a page at its
//! `last_edited_time`, its body and its comments listing at the
//! `last_edited_time` they were read for, a user or a commented block at
//! no version (read once). No table carries a stamp of its own.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, Migration};
use datalib_etl_macros::CasEdgeRow;

pub const PAGES: &str = "pages";
pub const PAGE_MARKDOWN: &str = "page_markdown";
pub const PAGE_COMMENTS: &str = "page_comments";
pub const COMMENTS: &str = "comments";
pub const COMMENT_ANCHORS: &str = "comment_anchors";
pub const USERS: &str = "users";
pub const ATTACHMENTS: &str = "notion_attachments";

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &[
    PAGES,
    PAGE_MARKDOWN,
    PAGE_COMMENTS,
    COMMENTS,
    COMMENT_ANCHORS,
    USERS,
    ATTACHMENTS,
];

/// `pages` — one row per Notion page UUID, holding the page *object*:
/// properties, parent, icon, cover, timestamps. For a database row —
/// most of a typical workspace — this is the whole record, because such
/// a page usually has no body at all. It is also the listing every
/// other table is owed against: a body, a comments listing and the
/// users a page names are all owed by the page's `last_edited_time`.
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
    unresolved_block_ids TEXT NULL
)";

/// `page_comments` — one row per page whose comments have been listed,
/// keyed by the page. The row is only its id: what the listing is held
/// at, the page's `last_edited_time` it was read for, is `held_version`
/// in its sidecar, and a listing that failed is an attempt there. Its
/// own row rather than the page's, so that the page being written again
/// does not clear a listing that failed.
pub const PAGE_COMMENTS_DDL: &str =
    "CREATE TABLE IF NOT EXISTS page_comments (id TEXT PRIMARY KEY)";

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

/// `users` — one row per Notion user a page or a comment names.
///
/// Filled one `GET /v1/users/{id}` at a time, once per id: `GET
/// /v1/users` (list all) is not available to personal access tokens,
/// which is what this provider authenticates with. Comment authors do
/// not depend on this — Notion resolves those on the comment itself —
/// but a page's `created_by` carries only an id. A user Notion answers
/// 404 for keeps an id-only row, so it is not asked for again.
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
/// (11 across 25 pages in a measured workspace). A block Notion answers
/// 404 for keeps an id-only row.
///
/// PK choice: the block UUID the comment is parented by.
pub const COMMENT_ANCHORS_DDL: &str = "CREATE TABLE IF NOT EXISTS comment_anchors (
    id TEXT PRIMARY KEY,
    page_id TEXT NULL,
    block_type TEXT NULL,
    plain_text TEXT NULL
)";

/// `notion_attachments` — N:M edge between a page and a `cas_objects`
/// blob, written with the body that names the file.
///
/// `ref_id` is the attachment's **slot**: the unsigned URL, query
/// string discarded. That is the one identifier upstream keeps stable —
/// the signature rotates hourly and the bytes can be replaced in place
/// — so it is what both the stored markdown and this edge are keyed on.
/// `blake3` is null until the CAS write lands; an edge without bytes is
/// what the attachment loop owes.
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

/// The page and the slot of an attachment edge's key.
pub fn split_edge_key(key: &str) -> Option<(&str, &str)> {
    key.split_once('#')
}

/// The raw store's migration ladder (etl/README.md §"The migration
/// ladder").
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "what each table holds is its sidecar's held_version",
    apply: |conn| Box::pin(held_into_the_sidecar(conn)),
}];

/// Rung 1. A page object was held by having a payload, a body by a
/// `source_last_edited_time` column of its own, and a comments listing
/// by nothing at all: a failed one was a mark on the page's row. Each
/// becomes a `held_version` in its sidecar; a page stub whose object
/// never fetched goes; the search mark goes, so the next run lists the
/// workspace whole once (its listing requests only: what is held stays
/// held).
async fn held_into_the_sidecar(conn: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    let has = |table: &'static str| {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)",
        )
        .bind(table)
    };
    for ddl in [
        PAGE_COMMENTS_DDL.to_string(),
        dr::bookkeeping_ddl_for(PAGE_COMMENTS),
        datalib_etl_web::coverage::DDL.to_string(),
    ] {
        // Audited: this module's own DDL.
        sqlx::query(sqlx::AssertSqlSafe(ddl))
            .execute(&mut *conn)
            .await?;
    }
    for table in [PAGES, PAGE_MARKDOWN] {
        let has_column: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?) WHERE name = 'held_version')",
        )
        .bind(format!("{table}_bookkeeping"))
        .fetch_one(&mut *conn)
        .await?;
        if !has_column {
            // Audited: `table` is one of this module's constants.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE {table}_bookkeeping ADD COLUMN held_version TEXT NULL"
            )))
            .execute(&mut *conn)
            .await?;
        }
    }
    let problems = has("problems").fetch_one(&mut *conn).await?;
    let (now, _) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();

    // A stub without an object is nothing the new listing knows of.
    if problems {
        sqlx::query(
            "DELETE FROM problems WHERE scope_kind = ? \
             AND scope_key IN (SELECT 'pages:' || id FROM pages WHERE payload IS NULL)",
        )
        .bind(datalib_problems::ScopeKind::Entity.as_str())
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query(
        "DELETE FROM pages_bookkeeping WHERE id IN (SELECT id FROM pages WHERE payload IS NULL)",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM pages WHERE payload IS NULL")
        .execute(&mut *conn)
        .await?;

    // Comments were listed with the page, so a page is held for them
    // unless its row says the listing failed, or the credential could
    // not read comments at all.
    let forbidden = problems
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM problems WHERE scope_key = 'listing:comments')",
        )
        .fetch_one(&mut *conn)
        .await?;
    if !forbidden {
        sqlx::query(
            "INSERT INTO page_comments (id) \
             SELECT p.id FROM pages p LEFT JOIN pages_bookkeeping b ON b.id = p.id \
             WHERE b.last_error IS NULL OR b.last_error NOT LIKE 'comments: %'",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO page_comments_bookkeeping (id, attempt_count, fetched_at_utc, held_version) \
             SELECT p.id, 0, ?, p.last_edited_time FROM page_comments c JOIN pages p ON p.id = c.id",
        )
        .bind(&now)
        .execute(&mut *conn)
        .await?;
    }

    // Every page with an object is held at its last_edited_time, and
    // the failures its row carried are owed listings now.
    sqlx::query(
        "INSERT INTO pages_bookkeeping (id, attempt_count, fetched_at_utc, held_version) \
         SELECT id, 0, ?, last_edited_time FROM pages WHERE true \
         ON CONFLICT(id) DO UPDATE SET held_version = excluded.held_version, \
            fetched_at_utc = COALESCE(pages_bookkeeping.fetched_at_utc, excluded.fetched_at_utc), \
            last_error = NULL",
    )
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    if problems {
        sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND scope_key LIKE 'pages:%'")
            .bind(datalib_problems::ScopeKind::Entity.as_str())
            .execute(&mut *conn)
            .await?;
    }

    // A body is held at the stamp its row carried; a stub that never
    // fetched carried none and stays owed.
    sqlx::query(
        "INSERT INTO page_markdown_bookkeeping (id, attempt_count, fetched_at_utc, held_version) \
         SELECT id, 0, ?, source_last_edited_time FROM page_markdown \
         WHERE source_last_edited_time IS NOT NULL \
         ON CONFLICT(id) DO UPDATE SET held_version = excluded.held_version, \
            fetched_at_utc = COALESCE(page_markdown_bookkeeping.fetched_at_utc, excluded.fetched_at_utc)",
    )
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    sqlx::query("ALTER TABLE page_markdown DROP COLUMN source_last_edited_time")
        .execute(&mut *conn)
        .await?;

    for (table, sql) in [
        (
            "sync_scope_state",
            "DELETE FROM sync_scope_state WHERE scope = 'workspace'",
        ),
        (
            "sync_scope_config",
            "DELETE FROM sync_scope_config WHERE scope = 'notion:download'",
        ),
    ] {
        if has(table).fetch_one(&mut *conn).await? {
            sqlx::query(sql).execute(&mut *conn).await?;
        }
    }
    Ok(())
}

/// Compose the full DDL list passed to
/// [`datalib_etl::doltlite_raw::open`].
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        PAGES_DDL.to_string(),
        PAGES_LAST_EDITED_INDEX_DDL.to_string(),
        PAGES_PARENT_INDEX_DDL.to_string(),
        PAGE_MARKDOWN_DDL.to_string(),
        PAGE_COMMENTS_DDL.to_string(),
        COMMENTS_DDL.to_string(),
        COMMENTS_PAGE_INDEX_DDL.to_string(),
        COMMENTS_DISCUSSION_INDEX_DDL.to_string(),
        COMMENT_ANCHORS_DDL.to_string(),
        USERS_DDL.to_string(),
        datalib_etl_web::coverage::DDL.to_string(),
    ];
    out.extend(NotionAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl::bulk::BulkUpsertable;

    #[test]
    fn the_edge_table_is_named_once() {
        assert_eq!(NotionAttachmentRow::TABLE, ATTACHMENTS);
        assert_eq!(
            split_edge_key("page-1#https://files.notion.so/a.png"),
            Some(("page-1", "https://files.notion.so/a.png"))
        );
    }
}
