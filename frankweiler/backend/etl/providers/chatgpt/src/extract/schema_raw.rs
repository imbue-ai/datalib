//! Raw-store schema for the ChatGPT provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! ChatGPT-specific notes: upstream supplies stable string ids for
//! every entity (no UUIDv5 recipe needed); `GridRow.when_ts` comes
//! from `conversations.update_time`; `last_listing_update_time` is a
//! demoted bookkeeping column — it used to be stuffed into the JSON
//! payload as a synthetic `_listing_update_time` key, but promoting
//! it out keeps the payload byte-for-byte identical to the live API
//! (see [`docs/data_architecture_ingestion.md`] §"Wire-fidelity of the raw
//! store").

use frankweiler_etl::doltlite_raw as dr;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["me", "conversations"];

/// `me` — the upstream `/backend-api/me` response.
///
/// One row per ChatGPT account. We keep `email` and `name` denormalized
/// for cheap predicate queries; the full response stays in `payload`.
///
/// Columns:
/// - `id` — upstream `/backend-api/me.id`. Primary key.
/// - `email` — denormalized from `payload.email`.
/// - `name` — denormalized from `payload.name`.
/// - `payload` — raw `/backend-api/me` response (JSONB on disk,
///   round-trips as text JSON in Rust).
pub const ME_DDL: &str = "CREATE TABLE IF NOT EXISTS me (
    id TEXT PRIMARY KEY,
    email TEXT NULL,
    name TEXT NULL,
    payload TEXT NULL
)";

/// `conversations` — one row per ChatGPT conversation id.
///
/// Stores the raw `/backend-api/conversation/{id}` response as
/// received from the live API.
///
/// Columns:
/// - `id` — upstream conversation id. Primary key.
/// - `title` — denormalized conversation title for cheap listing
///   queries; the payload remains authoritative.
/// - `update_time` — upstream `payload.update_time`. Used both as the
///   listing-derived skip-check cursor (extract side) and as the
///   source for `GridRow.when_ts` (translate side).
/// - `last_listing_update_time` — the most recent
///   `update_time` value we saw for this conversation in the
///   `/backend-api/conversations` listing pass. Promoted out of the
///   payload to keep wire-fidelity. Compared against
///   `payload.update_time` to decide whether the detail fetch is
///   stale. Stored as JSON because the upstream listing returns
///   varied types (string, number, sometimes null).
/// - `payload` — raw upstream conversation JSON (JSONB on disk).
pub const CONVERSATIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    title TEXT NULL,
    update_time TEXT NULL,
    last_listing_update_time TEXT NULL,
    payload TEXT NULL
)";

/// Index on `conversations.update_time` — supports the listing-derived
/// skip-check that asks "has this conversation changed since we last
/// fetched it?" without scanning the full table.
pub const CONVERSATIONS_UPDATE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_update ON conversations(update_time)";

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
        ME_DDL.to_string(),
        CONVERSATIONS_DDL.to_string(),
        CONVERSATIONS_UPDATE_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
