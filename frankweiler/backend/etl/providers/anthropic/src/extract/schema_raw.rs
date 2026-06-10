//! Raw-store schema for the Anthropic (Claude) provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture.md`](../../../../../docs/data_architecture.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! Anthropic-specific notes: upstream supplies stable UUIDs for every
//! entity (no UUIDv5 recipe needed); `GridRow.when_ts` comes from
//! `conversations.updated_at`; `conversations.payload` is the raw
//! `/api/...` response, not the pre-normalized export shape.

use frankweiler_etl::doltlite_raw as dr;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["users", "orgs", "conversations"];

/// `users` — one row per Anthropic user UUID.
///
/// Carries the `users.json` entries from a bulk export plus anything
/// synthesized from `/api/account` when no export is available.
///
/// Columns:
/// - `id` — Anthropic user UUID (upstream `uuid`). Primary key.
/// - `email` — denormalized from `payload.email_address` for quick
///   lookups; the payload remains authoritative.
/// - `full_name` — denormalized from `payload.full_name`, same
///   rationale.
/// - `payload` — raw user JSON object (JSONB-encoded on disk).
pub const USERS_DDL: &str = "CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    email TEXT NULL,
    full_name TEXT NULL,
    payload TEXT NULL
)";

/// `orgs` — one row per Anthropic organization UUID.
///
/// Columns:
/// - `id` — Anthropic org UUID (upstream `uuid`). Primary key.
/// - `name` — denormalized from `payload.name`.
/// - `payload` — raw `/api/organizations` entry (JSONB on disk).
pub const ORGS_DDL: &str = "CREATE TABLE IF NOT EXISTS orgs (
    id TEXT PRIMARY KEY,
    name TEXT NULL,
    payload TEXT NULL
)";

/// `conversations` — one row per Anthropic conversation UUID.
///
/// Stores the raw `/api/.../chat_conversations/{uuid}` payload as
/// received. The translate step applies `normalize_to_export_shape`
/// at read time.
///
/// Columns:
/// - `id` — Anthropic conversation UUID. Primary key.
/// - `org_uuid` — owning organization UUID. Needed at read time so
///   translate can rebuild the export-shape `_source` block and so
///   conversations sharing an account but living in different orgs
///   (e.g. personal Max plan vs. a Team-plan workspace) stay
///   disambiguated.
/// - `org_name` — human-readable org name, when available; comes
///   from the `_source.org_name` field at fetch time. Added by
///   [`MIGRATION_CONVERSATIONS_ADD_ORG_NAME`].
/// - `name` — denormalized conversation title from `payload.name`.
/// - `updated_at` — upstream `payload.updated_at` ISO-8601 stamp.
///   Used both as the listing-derived skip-check cursor (extract
///   side) and as the source for `GridRow.when_ts` (translate side).
/// - `payload` — raw upstream conversation JSON (JSONB on disk),
///   pre-normalization.
pub const CONVERSATIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    org_uuid TEXT NULL,
    org_name TEXT NULL,
    name TEXT NULL,
    updated_at TEXT NULL,
    payload TEXT NULL
)";

/// Index on `conversations.org_uuid` — supports the per-org filter in
/// translate, which builds one rendered tree per `(account, org)`
/// pair.
pub const CONVERSATIONS_ORG_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_org ON conversations(org_uuid)";

/// Index on `conversations.updated_at` — supports the listing-derived
/// skip-check that asks "have we seen this conversation at this
/// `updated_at` yet?" without scanning the full table.
pub const CONVERSATIONS_UPDATED_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_updated ON conversations(updated_at)";

/// Idempotent migration adding `conversations.org_name`. The
/// `CREATE TABLE IF NOT EXISTS` in [`CONVERSATIONS_DDL`] already
/// declares this column, so on fresh DBs the `ALTER` is a no-op
/// (SQLite returns "duplicate column" and we swallow it; see
/// [`crate::extract::db::RawDb::open`]).
///
/// **Safe to delete** once we are confident no production
/// `<data_root>/raw/<name>.doltlite_db` exists that was created
/// before `org_name` was added (i.e. when every deployed store has
/// been reopened at least once since the migration landed).
pub const MIGRATION_CONVERSATIONS_ADD_ORG_NAME: &str =
    "ALTER TABLE conversations ADD COLUMN org_name TEXT";

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
        USERS_DDL.to_string(),
        ORGS_DDL.to_string(),
        CONVERSATIONS_DDL.to_string(),
        CONVERSATIONS_ORG_INDEX_DDL.to_string(),
        CONVERSATIONS_UPDATED_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
