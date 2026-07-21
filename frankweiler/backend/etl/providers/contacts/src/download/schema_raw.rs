//! Raw-store schema for the CardDAV (contacts) provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
//! and [`docs/dev/archived/data_architecture_plan.md`](/docs/dev/archived/data_architecture_plan.md)
//! §"Schema first" for the conventions every `schema_raw.rs` follows.
//!
//! Contacts-specific notes:
//!
//! - **Not event-shaped.** A contact is a thing, not an event, so
//!   nothing here declares a `when_ts` column. Render either leaves
//!   `GridRow.when_ts` empty or sources it from the vCard `REV:` field
//!   (the `revision` column) when an event-shape is wanted.
//!
//! - **Inline photos, no CAS edge.** vCard `PHOTO` bytes ride inline
//!   (base64) inside the vCard text rather than being lifted into a
//!   sibling CAS file. There is no `*_attachments` edge table here,
//!   and the CAS pool stays unopened for this provider.
//!
//! - **The vCard text IS the canonical wire data.** Unlike beeper /
//!   whatsapp, the source isn't a local file we can re-download from
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

use std::sync::OnceLock;

use frankweiler_etl::bulk::BulkUpsertable;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::WirePayloadRow;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;
use uuid::Uuid;

pub const DATA_TABLES: &[&str] = &["accounts", "addressbooks", "contacts"];

// ─────────────────────────────────────────────────────────────────────
// accounts
// ─────────────────────────────────────────────────────────────────────

/// `accounts` — one row per configured CardDAV server.
///
/// We expect one row per `<name>/entities.doltlite_db` in practice (the
/// data root maps 1:1 with a file), but model as a table for symmetry with
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

/// Frozen UUIDv5 namespace for synthesized contacts identity. Changing
/// these bytes re-keys every UID-less contact we have ever ingested, so
/// the sequence is effectively immutable.
const CONTACTS_UUID_NS: Uuid = Uuid::from_bytes([
    0x2d, 0x9b, 0x4e, 0x7a, 0x1c, 0x44, 0x5f, 0x6d, 0x8b, 0x5a, 0x7c, 0x2b, 0x1d, 0x3e, 0x4f, 0x5a,
]);

/// Surrogate `uid` for a vCard that carries no RFC 6350 `UID:` — most
/// notably Google's vCard export, which omits it entirely. Derived from
/// the contact's first + last name so the *same person* collapses onto
/// one PK across re-exports; it's the closest thing to object permanence
/// the data allows when there's no stable server id.
///
/// Recipe: `uuidv5(CONTACTS_UUID_NS, "contact:name:{given}:{family}")`,
/// each component trimmed and lowercased so capitalization / whitespace
/// churn between exports doesn't fork the identity.
///
/// Caveat by construction: two distinct people who share a first + last
/// name hash to the same id and collapse into one row. The ingest path
/// ([`super::vcf_dir`]) detects and warns when that happens, and an
/// edit that changes the name also re-keys the row (it reads as a
/// delete + insert, not an update). A real `UID:` avoids both; prefer
/// fixing the source over relying on this.
pub fn synthesized_name_uid(given: &str, family: &str) -> String {
    let recipe = format!(
        "contact:name:{}:{}",
        given.trim().to_lowercase(),
        family.trim().to_lowercase(),
    );
    Uuid::new_v5(&CONTACTS_UUID_NS, recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

// ── Render-side grid identity ────────────────────────────────────
//
// These derive the UUIDs the rendered grid rows carry (`row.id`,
// `conversation_uuid`). Distinct from the download-side surrogate above:
// changing either namespace re-keys everything downstream, so both are
// frozen. The render path re-exports these via `crate::render`.

/// Stable namespace for the render-side contact / addressbook
/// UUIDs. Picked once + frozen so re-ingests are idempotent across
/// machines.
pub fn contacts_uuid_ns() -> &'static Uuid {
    static NS: OnceLock<Uuid> = OnceLock::new();
    NS.get_or_init(|| {
        Uuid::parse_str("3f4c6e9a-7c2b-4f1d-8b5a-1c2d3e4f5a6b").expect("valid contacts uuid ns")
    })
}

/// PK derivation for a contact across the whole stack: vCards from
/// the same UID under the same `(account, addressbook)` collapse
/// into the same row, no matter whether they came from a
/// sync-collection REPORT or a `.vcf` file on disk.
pub fn contact_uuid(account_id: &str, addressbook_label: &str, uid: &str) -> String {
    let name = format!("contact:{account_id}:{addressbook_label}:{uid}");
    Uuid::new_v5(contacts_uuid_ns(), name.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// Stable UUID for an addressbook. Used as the `conversation_uuid`
/// on every grid row so the UI groups all contacts in one
/// addressbook together.
pub fn addressbook_uuid(account_id: &str, addressbook_label: &str) -> String {
    let name = format!("addressbook:{account_id}:{addressbook_label}");
    Uuid::new_v5(contacts_uuid_ns(), name.as_bytes())
        .as_hyphenated()
        .to_string()
}

// ─────────────────────────────────────────────────────────────────────
// contact_photos — CAS edge for contact pictures
// ─────────────────────────────────────────────────────────────────────
//
// The same shape the LinkedIn provider uses (`contact_photos(id,
// owner_id, source_url, blake3)`), so the contact→photo-blob mapping is
// consistent across providers even though the code isn't shared (raw
// data is owned per-provider). Bytes live in the sibling
// `blobs.doltlite_db` CAS keyed by blake3; `owner_id` is the
// `contacts.id` (see [`contact_pk`]), `source_url` is `"vcard:inline"`
// for embedded base64 photos (or the URL for URL-only `PHOTO`s).

/// Edge table name. Shared (by convention, not code) with the LinkedIn
/// provider's `contact_photos`.
pub const CONTACT_PHOTOS_TABLE: &str = "contact_photos";

pub const CONTACT_PHOTOS_DDL: &str = "CREATE TABLE IF NOT EXISTS contact_photos (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT NOT NULL,
    source_url TEXT NOT NULL,
    blake3     TEXT NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)";

pub const CONTACT_PHOTOS_BY_OWNER_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS contact_photos_by_owner ON contact_photos(owner_id)";

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
        CONTACT_PHOTOS_DDL.to_string(),
        CONTACT_PHOTOS_BY_OWNER_INDEX_DDL.to_string(),
        // Resume cursor for the local-`.vcf` path: skip re-ingesting a
        // file whose `(size, mtime)` hasn't moved since last run. The
        // CardDAV server path uses etags/sync-tokens instead and never
        // touches this table. See [`vcf_dir`].
        frankweiler_etl::file_checkpoint::INGESTED_FILES_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_name_uid_is_stable_and_normalized() {
        let a = synthesized_name_uid("Ada", "Lovelace");
        // Same person, re-exported with different case/whitespace, keeps
        // one identity — no churn across exports.
        assert_eq!(a, synthesized_name_uid("  ada ", "LOVELACE"));
        // Distinct names get distinct ids.
        assert_ne!(a, synthesized_name_uid("Alan", "Turing"));
        // Stable, hyphenated UUID string.
        assert_eq!(a.len(), 36);
    }

    #[test]
    fn synthesized_name_uid_collides_on_shared_first_last_name() {
        // The documented hazard: two distinct people, same first+last
        // name, collapse onto one id. Callers warn on this.
        assert_eq!(
            synthesized_name_uid("John", "Smith"),
            synthesized_name_uid("John", "Smith"),
        );
    }

    #[test]
    fn contact_uuid_is_stable() {
        let a = contact_uuid("contacts.icloud.com", "Personal", "uid-1");
        let b = contact_uuid("contacts.icloud.com", "Personal", "uid-1");
        assert_eq!(a, b);
        let c = contact_uuid("contacts.icloud.com", "Work", "uid-1");
        assert_ne!(a, c);
    }
}
