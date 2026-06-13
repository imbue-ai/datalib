//! Raw-store schema for the CardDAV (contacts) provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §"Schema first" for the conventions every `schema_raw.rs` follows.
//!
//! Contacts-specific notes:
//!
//! - **Not event-shaped.** A contact is a thing, not an event, so
//!   nothing here declares a `when_ts` column. Translate either leaves
//!   `GridRow.when_ts` empty or sources it from the vCard `REV:` field
//!   (the `revision` column) when an event-shape is wanted.
//!
//! - **Inline photos, no CAS edge.** vCard `PHOTO` bytes ride inline
//!   (base64) inside the vCard text rather than being lifted into a
//!   sibling CAS file. There is no `*_attachments` edge table here,
//!   and the CAS pool stays unopened for this provider.
//!
//! - **The vCard text IS the canonical wire data.** Unlike beeper /
//!   whatsapp, the source isn't a local file we can re-extract from
//!   — it lives on a CardDAV server that may have deleted the row
//!   between syncs. So `contacts.payload` stays around (wrapped as
//!   `{"vcard": "<raw text>"}` so JSONB normalization leaves the
//!   bytes alone). `accounts.payload` / `addressbooks.payload`
//!   carried nothing the typed columns didn't already cover, so
//!   they're dropped per "the schema diff IS the design review."
//!
//! - **PKs are server-derived, not UUIDv5.** `accounts.id` is the URL
//!   host; `addressbooks.id` is `"<account_id>!<href>"`; `contacts.id`
//!   is `"<addressbook_id>#<UID>"` where UID is the RFC 6350 vCard
//!   `UID:` field. The recipes live with the row structs below so
//!   the writer and reader paths agree on format.

use frankweiler_etl::bulk::BulkUpsertable;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::WirePayloadRow;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

pub const DATA_TABLES: &[&str] = &["accounts", "addressbooks", "contacts"];

// ─────────────────────────────────────────────────────────────────────
// accounts
// ─────────────────────────────────────────────────────────────────────

/// `accounts` — one row per configured CardDAV server.
///
/// We expect one row per `<name>.doltlite_db` in practice (the data
/// root maps 1:1 with a file), but model as a table for symmetry with
/// other providers and so a future "merge two address-book backends
/// into one file" path stays trivial.
///
/// Columns:
/// - `id` — the URL host (`contacts.icloud.com`,
///   `carddav.fastmail.com`, …). Primary key.
/// - `server_url` — the root URL of the CardDAV server.
/// - `principal_href` — PROPFIND-derived `current-user-principal`
///   URL. NULL until discovery completes.
/// - `addressbook_home_set` — PROPFIND-derived addressbook home-set
///   URL. Same NULL-then-fill story as `principal_href`.
pub const ACCOUNTS_DDL: &str = "CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    server_url TEXT NULL,
    principal_href TEXT NULL,
    addressbook_home_set TEXT NULL
)";

#[derive(Debug, Clone, Default)]
pub struct AccountRow {
    pub id: String,
    pub server_url: Option<String>,
    pub principal_href: Option<String>,
    pub addressbook_home_set: Option<String>,
}

impl BulkUpsertable for AccountRow {
    const TABLE: &'static str = "accounts";
    const TYPED_COLUMNS: &'static [&'static str] =
        &["server_url", "principal_href", "addressbook_home_set"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(self.server_url.as_deref())
            .bind(self.principal_href.as_deref())
            .bind(self.addressbook_home_set.as_deref())
    }
}

// ─────────────────────────────────────────────────────────────────────
// addressbooks
// ─────────────────────────────────────────────────────────────────────

/// `addressbooks` — one row per CardDAV addressbook collection
/// discovered under an account's home-set.
///
/// PK choice: `"<account_id>!<href>"`. The CardDAV href (e.g.
/// `/dav/addressbooks/user/default/`) is stable per server and known
/// before the first detail fetch.
///
/// Columns:
/// - `id` — synthesized PK (see [`addressbook_pk`]). Primary key.
/// - `account_id` — promoted FK into [`ACCOUNTS_DDL`].
/// - `href` — server-assigned URL slot for the addressbook.
/// - `display_name` — PROPFIND `displayname` value.
/// - `description` — PROPFIND `addressbook-description`.
/// - `ctag` — the "has anything changed" fallback cursor when the
///   server doesn't honor sync tokens.
/// - `sync_token` — the `<sync-token>` returned by the previous
///   `sync-collection` REPORT; NULL until the first cycle completes.
pub const ADDRESSBOOKS_DDL: &str = "CREATE TABLE IF NOT EXISTS addressbooks (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    href TEXT NOT NULL,
    display_name TEXT NULL,
    description TEXT NULL,
    ctag TEXT NULL,
    sync_token TEXT NULL
)";

pub const ADDRESSBOOKS_BY_ACCOUNT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS addressbooks_by_account ON addressbooks(account_id)";

#[derive(Debug, Clone, Default)]
pub struct AddressbookRow {
    pub id: String,
    pub account_id: String,
    pub href: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub ctag: Option<String>,
}

impl BulkUpsertable for AddressbookRow {
    const TABLE: &'static str = "addressbooks";
    // `sync_token` is bumped separately via `set_sync_token` after a
    // successful sync-collection REPORT, so it stays out of the
    // promoted-column list (and bulk-upsert won't clobber it).
    const TYPED_COLUMNS: &'static [&'static str] =
        &["account_id", "href", "display_name", "description", "ctag"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.account_id)
            .bind(&self.href)
            .bind(self.display_name.as_deref())
            .bind(self.description.as_deref())
            .bind(self.ctag.as_deref())
    }
}

/// PK recipe: `"{account_id}!{href}"`.
pub fn addressbook_pk(account_id: &str, href: &str) -> String {
    format!("{account_id}!{href}")
}

// ─────────────────────────────────────────────────────────────────────
// contacts
// ─────────────────────────────────────────────────────────────────────

/// `contacts` — one row per vCard.
///
/// PK choice: `"<addressbook_id>#<UID>"` where UID is the vCard `UID:`
/// field (RFC 6350 mandates non-empty). If a server ever emits a
/// vCard without a UID the fetch path falls back to a UUIDv5 derived
/// from `(addressbook_id, href)` and logs a warning.
///
/// Columns:
/// - `id` — synthesized PK (see [`contact_pk`]). Primary key.
/// - `addressbook_id` — promoted FK into [`ADDRESSBOOKS_DDL`].
/// - `uid` — the raw vCard `UID:` value, kept separately so callers
///   can recover the PK recipe without parsing.
/// - `href` — server-assigned URL slot for this vCard.
/// - `etag` — opaque version string the server stamps on each
///   resource. Compared to the listing-side etag to decide whether a
///   detail fetch is needed.
/// - `display_name` — denormalized from the vCard `FN:` field.
/// - `revision` — denormalized from the vCard `REV:` field. The
///   closest thing this provider has to an event-shaped timestamp.
/// - `payload` — JSON envelope `{"vcard": "<raw text>"}` carrying the
///   raw vCard bytes verbatim. The envelope keeps the column valid
///   JSONB while the inner string stays byte-for-byte identical to
///   what the server returned. We retain this column because the
///   server-side data is the only place the vCard exists once the
///   server prunes it.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "contacts")]
pub struct ContactRow {
    pub id_and_payload: WirePayload,
    pub addressbook_id: String,
    pub uid: String,
    pub href: String,
    pub etag: Option<String>,
    pub display_name: Option<String>,
    pub revision: Option<String>,
}

impl ContactRow {
    /// Build from the upstream `(addressbook_id, raw vCard text)`
    /// pair plus the metadata harvested alongside. Wraps the vCard
    /// text in the standard `{"vcard": …}` JSON envelope.
    pub fn new(
        addressbook_id: String,
        uid: String,
        href: String,
        etag: Option<String>,
        display_name: Option<String>,
        revision: Option<String>,
        vcard_text: &str,
    ) -> Self {
        let envelope = serde_json::json!({ "vcard": vcard_text });
        Self {
            id_and_payload: WirePayload {
                id: contact_pk(&addressbook_id, &uid),
                payload: envelope.to_string(),
            },
            addressbook_id,
            uid,
            href,
            etag,
            display_name,
            revision,
        }
    }
}

/// PK recipe: `"{addressbook_id}#{uid}"`.
pub fn contact_pk(addressbook_id: &str, uid: &str) -> String {
    format!("{addressbook_id}#{uid}")
}

pub const CONTACTS_BY_ADDRESSBOOK_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS contacts_by_addressbook ON contacts(addressbook_id)";

pub const CONTACTS_BY_HREF_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS contacts_by_href ON contacts(addressbook_id, href)";

// ─────────────────────────────────────────────────────────────────────
// Composer
// ─────────────────────────────────────────────────────────────────────

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        ACCOUNTS_DDL.to_string(),
        ADDRESSBOOKS_DDL.to_string(),
        ADDRESSBOOKS_BY_ACCOUNT_INDEX_DDL.to_string(),
        ContactRow::ddl(),
        CONTACTS_BY_ADDRESSBOOK_INDEX_DDL.to_string(),
        CONTACTS_BY_HREF_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
