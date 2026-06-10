//! Raw-store schema for the CardDAV (contacts) provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! Contacts-specific notes:
//!
//! - **Not event-shaped.** A contact is a thing, not an event, so
//!   nothing here declares a `when_ts` column. Per
//!   [`docs/data_architecture_ingestion.md`] §"Entities without a time-shape",
//!   the translate side either leaves `GridRow.when_ts` empty or
//!   sources it from the vCard `REV:` field (`revision` column) when
//!   needed.
//!
//! - **Inline photos, no `blob_refs`.** Per
//!   [`docs/data_architecture_ingestion.md`] §"Why contacts doesn't
//!   participate", vCard `PHOTO` bytes ride inline (base64) inside the
//!   payload column rather than being lifted into the shared CAS.
//!   There is no contacts-specific blob table declared here, and the
//!   shared `blob_refs` table created by `doltlite_raw::open` stays
//!   empty for this provider.
//!
//! - **PKs are server-derived, not UUIDv5.** `accounts.id` is the URL
//!   host; `addressbooks.id` is `"<account_id>!<href>"`; `contacts.id`
//!   is `"<addressbook_id>#<UID>"` where UID is the RFC 6350 vCard
//!   `UID:` field. The recipes live next to the upsert code in
//!   [`super::db`] (`addressbook_pk`, `contact_pk`).
//!
//! - **Payload is JSON-wrapped vCard text.** The `contacts.payload`
//!   column stores `{"vcard": "<raw text>"}` rather than a parsed
//!   object, so JSONB normalization at the dolt layer leaves the
//!   bytes alone and `dolt diff` reflects exactly the wire change.

use frankweiler_etl::doltlite_raw as dr;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["accounts", "addressbooks", "contacts"];

/// `accounts` — one row per configured CardDAV server.
///
/// We expect one row per `<name>.doltlite_db` in practice (the data
/// root maps 1:1 with a file), but model as a table for symmetry with
/// other providers and so a future "merge two address-book backends
/// into one file" path stays trivial.
///
/// Columns:
/// - `id` — the URL host (`contacts.icloud.com`,
///   `carddav.fastmail.com`, …). Primary key. One latchkey credential
///   entry keys per host, so this is the natural account key.
/// - `server_url` — the root URL of the CardDAV server, recorded for
///   provenance.
/// - `principal_href` — PROPFIND-derived `current-user-principal`
///   URL. NULL until discovery completes; back-filled later.
/// - `addressbook_home_set` — PROPFIND-derived addressbook home-set
///   URL. Same NULL-then-fill story as `principal_href`.
/// - `payload` — JSON object holding the same fields plus anything
///   else surfaced by discovery (JSONB-encoded on disk).
pub const ACCOUNTS_DDL: &str = "CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    server_url TEXT NULL,
    principal_href TEXT NULL,
    addressbook_home_set TEXT NULL,
    payload TEXT NULL
)";

/// `addressbooks` — one row per CardDAV addressbook collection
/// discovered under an account's home-set.
///
/// Columns:
/// - `id` — `"<account_id>!<href>"`. Primary key. The CardDAV href
///   (e.g. `/dav/addressbooks/user/default/`) is stable per server
///   and known before the first detail fetch, satisfying the
///   `doltlite_raw` PK guide.
/// - `account_id` — promoted FK into [`ACCOUNTS_DDL`].
/// - `href` — server-assigned URL slot for the addressbook.
/// - `display_name` — PROPFIND `displayname` value, promoted for
///   cheap predicate / filter queries.
/// - `description` — PROPFIND `addressbook-description`, when
///   provided by the server.
/// - `ctag` — the "has anything changed" fallback cursor when the
///   server doesn't honor sync tokens.
/// - `sync_token` — the `<sync-token>` returned by the previous
///   `sync-collection` REPORT. On next sync we hand it back and the
///   server replies with deltas only. NULL until the first
///   sync-collection cycle completes.
/// - `payload` — JSON object of the same fields plus anything else
///   surfaced by PROPFIND (JSONB-encoded on disk).
pub const ADDRESSBOOKS_DDL: &str = "CREATE TABLE IF NOT EXISTS addressbooks (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    href TEXT NOT NULL,
    display_name TEXT NULL,
    description TEXT NULL,
    ctag TEXT NULL,
    sync_token TEXT NULL,
    payload TEXT NULL
)";

/// Index on `addressbooks.account_id` — supports the per-account
/// listing query in [`super::db::RawDb::addressbooks_for_fetch`]
/// without a full scan.
pub const ADDRESSBOOKS_BY_ACCOUNT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS addressbooks_by_account ON addressbooks(account_id)";

/// `contacts` — one row per vCard.
///
/// Columns:
/// - `id` — `"<addressbook_id>#<UID>"`, where UID is the vCard `UID:`
///   field (RFC 6350 mandates non-empty). If a server ever emits a
///   vCard without a UID, the fetch path falls back to a UUIDv5
///   derived from `(addressbook_id, href)` and logs a warning.
///   Primary key.
/// - `addressbook_id` — promoted FK into [`ADDRESSBOOKS_DDL`].
/// - `uid` — the raw vCard `UID:` value, kept separately so callers
///   can recover the PK recipe without parsing.
/// - `href` — server-assigned URL slot for this vCard; the CardDAV
///   handle the server gives us alongside `etag`.
/// - `etag` — opaque version string the server stamps on each
///   resource. Compared to the listing-side etag to decide whether a
///   detail fetch is needed.
/// - `display_name` — denormalized from the vCard `FN:` field for
///   cheap listing queries; payload remains authoritative.
/// - `revision` — denormalized from the vCard `REV:` field. The
///   closest thing this provider has to an event-shaped timestamp;
///   translate may use it for `GridRow.when_ts` when an event-shape
///   is wanted, but contacts are fundamentally not event-shaped (see
///   module rustdoc).
/// - `payload` — JSON envelope `{"vcard": "<raw text>"}` carrying
///   the raw vCard bytes verbatim. JSONB-encoded on disk; the
///   envelope keeps the column valid JSONB while the inner string
///   stays byte-for-byte identical to what the server returned.
pub const CONTACTS_DDL: &str = "CREATE TABLE IF NOT EXISTS contacts (
    id TEXT PRIMARY KEY,
    addressbook_id TEXT NOT NULL,
    uid TEXT NULL,
    href TEXT NOT NULL,
    etag TEXT NULL,
    display_name TEXT NULL,
    revision TEXT NULL,
    payload TEXT NULL
)";

/// Index on `contacts.addressbook_id` — supports the per-addressbook
/// scans that drive sync (etag map, deletes, etc.).
pub const CONTACTS_BY_ADDRESSBOOK_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS contacts_by_addressbook ON contacts(addressbook_id)";

/// Index on `contacts(addressbook_id, href)` — supports the
/// href-keyed lookups used by the delete path and the etag-walk
/// fallback (which both arrive with `(book_id, href)` in hand and
/// need to resolve to a row).
pub const CONTACTS_BY_HREF_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS contacts_by_href ON contacts(addressbook_id, href)";

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
        ACCOUNTS_DDL.to_string(),
        ADDRESSBOOKS_DDL.to_string(),
        ADDRESSBOOKS_BY_ACCOUNT_INDEX_DDL.to_string(),
        CONTACTS_DDL.to_string(),
        CONTACTS_BY_ADDRESSBOOK_INDEX_DDL.to_string(),
        CONTACTS_BY_HREF_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
