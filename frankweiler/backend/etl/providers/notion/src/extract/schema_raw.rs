//! Raw-store schema for the Notion provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture.md`](../../../../../docs/data_architecture.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! Notion-specific notes:
//!
//! - **Notion is the doc's canonical pre-seed-before-fetch example.**
//!   See `docs/data_architecture.md` §"Retry and fetch durability":
//!   discovery (search / list / BFS-from-parent) surfaces a page id
//!   well before the detail GET runs, so the writer calls
//!   `dr::ensure_object_row(&mut tx, "pages", id)` on the bookkeeping
//!   sidecar to materialize the row with `attempt_count = 0` the moment
//!   the id is known. A later detail fetch turns into a payload-bearing
//!   UPSERT against the same PK, and a failed fetch turns into a
//!   `record_object_error` row — `--retry-failed` then finds it without
//!   anything ever going missing. Blocks and comments are detail-in-list
//!   (Notion's `/blocks/{id}/children` and `/comments` both return full
//!   bodies), so they go straight from upstream to a payload-bearing
//!   row without the pre-seed step.
//! - **Native UUIDs everywhere.** Notion supplies stable v4 UUIDs for
//!   pages, blocks, databases, users, and comments; no UUIDv5 recipe is
//!   needed. PKs are the upstream UUIDs verbatim.
//! - **Event-shaped, but only at the page/block grain.** `pages.payload`
//!   and `blocks.payload` both carry a `last_edited_time`, which is the
//!   source for `GridRow.when_ts` on the translate side. Sub-items
//!   without their own timestamp (e.g. comments, which have only a
//!   `created_time`) get a µs-bump off their parent per
//!   `docs/data_architecture.md` §"Object identity".
//! - **`page_order` is local ordering, not identity.** `blocks.page_order`
//!   is the 0-based index from the BFS walk in `extract::mod::walk_page_blocks`;
//!   render uses it to lay out sections / toggles. It is **not** part of
//!   the PK — upstream may re-arrange a block and we want the row at the
//!   same UUID with the column updating.

use frankweiler_etl::doltlite_raw as dr;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["pages", "blocks", "databases", "users", "comments"];

/// `pages` — one row per Notion page UUID.
///
/// Provenance: `GET https://api.notion.com/v1/pages/{id}` for detail,
/// with discovery via subtree seeds, BFS through `child_page` blocks,
/// and the unofficial `getNotificationLog` inbox walker. Discovery may
/// pre-seed a row (id only, no payload) before the detail GET — see the
/// module-level pre-seed note.
///
/// PK choice: upstream Notion page UUID (dashed v4 form normalized by
/// `extract::format_uuid`).
///
/// Columns:
/// - `id` — Notion page UUID. Primary key.
/// - `parent_id` — promoted from `payload.parent.page_id` /
///   `parent.block_id` / `parent.database_id` (or the literal
///   `"workspace"` sentinel) so BFS-from-parent and "show me children
///   of X" don't have to crack the payload.
/// - `last_edited_time` — upstream `payload.last_edited_time` ISO-8601
///   stamp. Drives the pre-detail skip check ("have we already mirrored
///   this page at this `last_edited_time`?") and is the source for
///   `GridRow.when_ts` on the translate side.
/// - `payload` — raw `/pages/{id}` JSON (JSONB-encoded on disk). May be
///   NULL for pre-seeded discovery rows whose detail fetch hasn't
///   landed yet.
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
/// Provenance: `GET /v1/blocks/{parent}/children` returns full block
/// bodies, so blocks are detail-in-list — no pre-seed step. Stored as
/// the BFS walk discovers them.
///
/// PK choice: upstream Notion block UUID. `page_order` is local layout
/// metadata, not part of the PK; the same block may re-arrange upstream
/// and we want the row at the same UUID with the column updating.
///
/// Columns:
/// - `id` — Notion block UUID. Primary key.
/// - `parent_id` — promoted from `payload.parent.block_id` /
///   `parent.page_id`.
/// - `page_id` — the owning page's UUID. Promoted so the
///   per-page child join (translate / render) avoids cracking the
///   payload.
/// - `page_order` — 0-based index of this block within the owning
///   page's BFS walk (see `extract::mod::walk_page_blocks`). Render
///   uses it to lay out sections / toggles deterministically,
///   independent of UUID ordering. Not part of identity.
/// - `last_edited_time` — upstream `payload.last_edited_time` ISO-8601
///   stamp. Source for `GridRow.when_ts` on the translate side.
/// - `payload` — raw block JSON (JSONB-encoded on disk).
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
/// Provenance: `GET /v1/databases/{id}` for the database object itself
/// (rows inside a database are pages, captured under `pages`).
///
/// PK choice: upstream Notion database UUID.
///
/// Columns:
/// - `id` — Notion database UUID. Primary key.
/// - `parent_id` — promoted from `payload.parent.page_id` /
///   `parent.workspace`.
/// - `last_edited_time` — upstream `payload.last_edited_time` ISO-8601
///   stamp. Source for `GridRow.when_ts` on the translate side.
/// - `payload` — raw database JSON (JSONB-encoded on disk).
pub const DATABASES_DDL: &str = "CREATE TABLE IF NOT EXISTS databases (
    id TEXT PRIMARY KEY,
    parent_id TEXT NULL,
    last_edited_time TEXT NULL,
    payload TEXT NULL
)";

/// `users` — one row per Notion user UUID surfaced anywhere in the
/// mirror (page author, comment author, mention, …).
///
/// Provenance: `GET /v1/users/{id}` when fetched explicitly; otherwise
/// inlined from any payload that references the user.
///
/// PK choice: upstream Notion user UUID.
///
/// Columns:
/// - `id` — Notion user UUID. Primary key.
/// - `payload` — raw user JSON (JSONB-encoded on disk).
///
/// Not event-shaped; no `when_ts` story.
pub const USERS_DDL: &str = "CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    payload TEXT NULL
)";

/// `comments` — one row per Notion page / block comment UUID.
///
/// Provenance: `GET /v1/comments?block_id={page_id}` returns full
/// comment bodies, so comments are detail-in-list — no pre-seed step.
///
/// PK choice: upstream Notion comment UUID.
///
/// Columns:
/// - `id` — Notion comment UUID. Primary key.
/// - `parent_id` — promoted from `payload.parent.block_id` /
///   `parent.page_id`. NOT NULL: comments must hang off something, and
///   the writer falls back to the page-being-fetched when upstream
///   omits a parent (see `extract::mod::mirror_page`).
/// - `page_id` — the owning page's UUID, propagated by the writer so
///   the per-page child join (translate / render) avoids cracking the
///   payload.
/// - `payload` — raw comment JSON (JSONB-encoded on disk).
///
/// Comments only have a `created_time` upstream; sub-items lacking their
/// own `last_edited_time` get a µs-bump off their parent on the
/// translate side per `docs/data_architecture.md` §"Object identity".
pub const COMMENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS comments (
    id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL,
    page_id TEXT NULL,
    payload TEXT NULL
)";

/// Index on `comments.page_id` — supports the per-page child join.
pub const COMMENTS_PAGE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS comments_page ON comments(page_id)";

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
///
/// Schema-local glue, kept here so the "what tables exist?" answer
/// is one function call from this file. Heavier composition (e.g. a
/// repo-wide bookkeeping macro) is deferred to P1.1.
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
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
