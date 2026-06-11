//! Raw-store schema for the JMAP (email) provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! JMAP-specific notes:
//!
//! - **Two table families.** [`DATA_TABLES`] are the JMAP entity
//!   tables — `accounts`, `mailboxes`, `threads`, `emails` — each
//!   paired with a `<table>_bookkeeping` sidecar.
//!   [`JOIN_TABLES`] are the many-to-many sidecar tables
//!   (`email_mailboxes`, `email_keywords`, `email_attachments`)
//!   that live and die with their parent email's upsert
//!   transaction. The join tables deliberately don't get
//!   `<table>_bookkeeping` companions; their PK is composite, they
//!   refresh delete-then-insert per email, and there's no per-row
//!   retry story to track.
//!
//! - **Row structs and the bulk-upsert path.** Three of the four
//!   entity tables (`accounts`, `mailboxes`, `threads`) follow the
//!   shared wire-payload shape and are declared as
//!   `#[derive(WirePayloadRow)]` row structs — the derive emits both
//!   the table's DDL and its `BulkUpsertable` impl from the field
//!   list. The fourth (`emails`) is **envelope-only**: the
//!   `.eml` body lives in the shared blob CAS keyed by `blob_id`, so
//!   the table carries promoted columns but no JSONB payload. It
//!   gets a hand-written DDL + manual `BulkUpsertable` impl
//!   (with `PAYLOAD_COLUMN = None`). All four route through the
//!   generic `bulk_upsert_in_tx` helper — no table-specific bulk
//!   SQL anywhere in this provider's code.
//!
//! - **PKs are upstream-supplied JMAP ids** for every entity:
//!   `accountId`, Mailbox `id`, Thread `id`, Email `id`. No UUIDv5
//!   recipes needed.
//!
//! - **Event-shaped.** `emails.received_at` carries the upstream
//!   `Email/get.receivedAt` and is the event timestamp. `sent_at`
//!   carries the message's Date header. `accounts`, `mailboxes`,
//!   `threads` are not event-shaped.
//!
//! - **Hard-delete on destroy.** When JMAP reports an email
//!   `destroyed` the row + its joins + its bookkeeping get
//!   DELETEd (blobs survive since other emails may share them).
//!   Doltlite history retains the prior state — this is the
//!   only place in the raw-store family where we hard-delete
//!   rather than mark.
//!
//! - **Blobs in the shared CAS, kind-discriminated.** The `.eml`
//!   RFC5322 source for each message and each attachment ride in
//!   the shared `blobs` table via `Email.blobId`; see
//!   [`BLOB_KIND_EML`] and [`BLOB_KIND_ATTACHMENT`] for the
//!   `kind` discriminator values.

use frankweiler_etl::bulk::BulkUpsertable;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayloadRow, WirePayloadTriad};
use frankweiler_etl_macros::WirePayloadRow;
use serde_json::Value;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// Entity tables — what `dolt diff` should see across re-fetches.
/// Each gets a paired `<table>_bookkeeping` sidecar (added in
/// [`full_ddl`]) and is wiped by `extract::db::RawDb::reset`.
pub const DATA_TABLES: &[&str] = &["accounts", "mailboxes", "threads", "emails"];

/// Many-to-many join tables. Not in [`DATA_TABLES`] because they
/// don't get bookkeeping sidecars — they're refreshed
/// delete-then-insert per parent email upsert, so per-row retry
/// state would be noise. `RawDb::reset` truncates them alongside
/// [`DATA_TABLES`].
pub const JOIN_TABLES: &[&str] = &["email_mailboxes", "email_keywords", "email_attachments"];

/// `blobs.kind` discriminator for the RFC5322 `.eml` source of an
/// email, stored in the shared `blobs` table keyed by JMAP
/// `Email.blobId`.
pub const BLOB_KIND_EML: &str = "email";

/// `blobs.kind` discriminator for an attachment blob, stored in
/// the shared `blobs` table keyed by the JMAP attachment
/// `blobId`. Cross-referenced from `email_attachments.blob_id`.
pub const BLOB_KIND_ATTACHMENT: &str = "attachment";

// ── entity rows / DDLs ──────────────────────────────────────────────

/// `accounts` — one row per JMAP account known to the session.
///
/// Promoted columns: `name`, `is_personal`, `is_read_only`. The full
/// session-fragment account entry is stored as the JSONB payload.
/// `payload_blake3` is the blake3 hex of those bytes, used by
/// translate's bucket-fingerprint path.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "accounts")]
pub struct AccountRow {
    pub triad: WirePayloadTriad,
    pub name: Option<String>,
    pub is_personal: Option<i64>,
    pub is_read_only: Option<i64>,
}

impl AccountRow {
    /// Build an `AccountRow` from a session-fragment account JSON entry.
    pub fn from_payload(id: &str, payload: &Value) -> anyhow::Result<Self> {
        let name = payload
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let is_personal = payload
            .get("isPersonal")
            .and_then(|v| v.as_bool())
            .map(|b| b as i64);
        let is_read_only = payload
            .get("isReadOnly")
            .and_then(|v| v.as_bool())
            .map(|b| b as i64);
        let payload_str = serde_json::to_string(payload)?;
        let payload_blake3 = frankweiler_etl::blob_cas::blake3_hex(payload_str.as_bytes());
        Ok(Self {
            triad: WirePayloadTriad {
                id: id.to_string(),
                payload: payload_str,
                payload_blake3,
            },
            name,
            is_personal,
            is_read_only,
        })
    }
}

/// `mailboxes` — one row per JMAP Mailbox.
///
/// Fastmail (and other JMAP servers) uses mailboxes as both folders
/// and labels; this table is the source of truth for label names.
/// Promoted columns: `account_id` (FK), `name`, `parent_id`, `role`,
/// `sort_order`, `total_emails`, `unread_emails`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "mailboxes")]
pub struct MailboxRow {
    pub triad: WirePayloadTriad,
    pub account_id: String,
    pub name: Option<String>,
    pub parent_id: Option<String>,
    pub role: Option<String>,
    pub sort_order: Option<i64>,
    pub total_emails: Option<i64>,
    pub unread_emails: Option<i64>,
}

impl MailboxRow {
    pub fn from_payload(account_id: &str, payload: &Value) -> anyhow::Result<Self> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("mailbox payload missing id"))?;
        let name = payload
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let parent_id = payload
            .get("parentId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let role = payload
            .get("role")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let sort_order = payload.get("sortOrder").and_then(|v| v.as_i64());
        let total_emails = payload.get("totalEmails").and_then(|v| v.as_i64());
        let unread_emails = payload.get("unreadEmails").and_then(|v| v.as_i64());
        let payload_str = serde_json::to_string(payload)?;
        let payload_blake3 = frankweiler_etl::blob_cas::blake3_hex(payload_str.as_bytes());
        Ok(Self {
            triad: WirePayloadTriad {
                id: id.to_string(),
                payload: payload_str,
                payload_blake3,
            },
            account_id: account_id.to_string(),
            name,
            parent_id,
            role,
            sort_order,
            total_emails,
            unread_emails,
        })
    }
}

/// Index on `mailboxes(account_id)` — supports the per-account
/// listing walk without a full-table scan.
pub const MAILBOXES_BY_ACCOUNT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS mailboxes_by_account ON mailboxes(account_id)";

/// `threads` — one row per JMAP Thread.
///
/// Threads are the JMAP grouping for "messages in the same
/// conversation"; an email belongs to exactly one thread. Promoted
/// columns: `account_id` (FK), `email_count`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "threads")]
pub struct ThreadRow {
    pub triad: WirePayloadTriad,
    pub account_id: String,
    pub email_count: Option<i64>,
}

impl ThreadRow {
    pub fn from_payload(id: &str, account_id: &str, payload: &Value) -> anyhow::Result<Self> {
        let email_count = payload
            .get("emailIds")
            .and_then(|v| v.as_array())
            .map(|a| a.len() as i64);
        let payload_str = serde_json::to_string(payload)?;
        let payload_blake3 = frankweiler_etl::blob_cas::blake3_hex(payload_str.as_bytes());
        Ok(Self {
            triad: WirePayloadTriad {
                id: id.to_string(),
                payload: payload_str,
                payload_blake3,
            },
            account_id: account_id.to_string(),
            email_count,
        })
    }
}

/// Index on `threads(account_id)` — supports per-account thread
/// queries.
pub const THREADS_BY_ACCOUNT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS threads_by_account ON threads(account_id)";

/// `emails` — one row per email.
///
/// **Envelope-only.** The body lives in the shared `blobs` CAS as the
/// RFC5322 `.eml` source, keyed by `blob_id` and discriminated by
/// [`BLOB_KIND_EML`]. Translate mail-parses the `.eml` on demand;
/// nothing about bodies is materialized at extract.
///
/// Unlike the other entity tables, `emails` has no JSONB payload column
/// — the wire bytes ARE the .eml, and they live in the CAS, not inline.
/// The DDL is therefore hand-written, and the [`EmailRow`] type carries
/// its own [`BulkUpsertable`] impl (with `PAYLOAD_COLUMN = None`).
///
/// Columns:
/// - `id` — upstream email id. Primary key. For JMAP this is the
///   server's opaque `Email.id`; for mbox sources it's the RFC 822
///   `Message-ID:` value (angle brackets stripped) or
///   `sha256(eml_bytes)` when the header is missing.
/// - `account_id` — owning account; FK into [`AccountRow`].
/// - `thread_id` — owning thread; FK into [`ThreadRow`]. For JMAP
///   this is the server's `Email.threadId`; for mbox it's
///   `X-GM-THRID` verbatim, falling back to `id` for single-message
///   threads.
/// - `blob_id` — `ref_id` into the shared blob CAS for this email's
///   `.eml` bytes. For JMAP this is the server-opaque
///   `Email.blobId` (e.g. `"B-eml-1"`); for mbox it's
///   `sha256(eml_bytes)`. In both cases the shared
///   `blob_refs(ref_id, blake3)` table maps it to the blake3 that
///   keys the actual bytes in `cas_objects`.
/// - `message_id` — RFC 822 `Message-ID:` header value when present.
/// - `received_at` — event timestamp (UTC ISO-8601). For JMAP,
///   `Email.receivedAt`; for mbox, the parsed `Date:` header.
/// - `sent_at` — `Date:` header value. For mbox sources this equals
///   `received_at`; JMAP keeps them distinct.
/// - `size` — `.eml` size in bytes.
/// - `subject` — denormalized subject for cheap listing display.
/// - `from_json` — promoted JSON of the From: header(s); kept as
///   JSON because RFC 5322 permits multiple addresses with display
///   names and groups.
/// - `has_attachment` — 0/1 flag, set when at least one non-body
///   MIME part is present.
pub const EMAILS_DDL: &str = "CREATE TABLE IF NOT EXISTS emails (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    blob_id TEXT NOT NULL,
    blake3 TEXT NULL,
    message_id TEXT NULL,
    received_at TEXT NULL,
    sent_at TEXT NULL,
    size INTEGER NULL,
    subject TEXT NULL,
    from_json TEXT NULL,
    has_attachment INTEGER NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)";

/// Row struct for the `emails` table. Carries the envelope columns
/// plus the join-table inputs the extract path will use to refresh
/// `email_mailboxes` / `email_keywords` / `email_attachments` in the
/// same transaction.
///
/// `BulkUpsertable` is hand-rolled (not derived) because emails is
/// envelope-only — there is no `payload` / `payload_blake3` triad
/// to fit. The bind sequence binds `id` plus the typed columns;
/// join inputs (`mailbox_ids`, `keywords`, `attachments`) are NOT
/// bound here — they're consumed by the per-row join-refresh helper
/// the caller runs alongside the bulk-upsert.
#[derive(Debug, Clone)]
pub struct EmailRow {
    pub id: String,
    pub account_id: String,
    pub thread_id: String,
    pub blob_id: String,
    pub message_id: Option<String>,
    pub received_at: Option<String>,
    pub sent_at: Option<String>,
    pub size: Option<i64>,
    pub subject: Option<String>,
    pub from_json: Option<String>,
    pub has_attachment: bool,
    pub mailbox_ids: Vec<String>,
    pub keywords: Vec<String>,
    pub attachments: Vec<AttachmentRow>,
}

#[derive(Debug, Clone)]
pub struct AttachmentRow {
    pub part_id: String,
    pub blob_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<i64>,
    pub disposition: Option<String>,
    pub cid: Option<String>,
}

impl BulkUpsertable for EmailRow {
    const TABLE: &'static str = "emails";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "account_id",
        "thread_id",
        "blob_id",
        "message_id",
        "received_at",
        "sent_at",
        "size",
        "subject",
        "from_json",
        "has_attachment",
    ];
    // Envelope-only — no JSONB payload column.
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
            .bind(&self.thread_id)
            .bind(&self.blob_id)
            .bind(self.message_id.as_deref())
            .bind(self.received_at.as_deref())
            .bind(self.sent_at.as_deref())
            .bind(self.size)
            .bind(self.subject.as_deref())
            .bind(self.from_json.as_deref())
            .bind(self.has_attachment as i64)
    }
}

impl EmailRow {
    /// Promote the envelope columns from a JMAP `Email/get` response.
    /// Returns `None` if the response is missing one of the required
    /// identifiers (`id`, `blobId`, `threadId`). The body parts
    /// (`bodyValues`, `textBody`, `htmlBody`) are deliberately
    /// ignored — translate reads them out of the `.eml` blob.
    pub fn from_envelope(account_id: &str, envelope: &Value) -> Option<Self> {
        let id = envelope.get("id")?.as_str()?.to_string();
        let blob_id = envelope.get("blobId")?.as_str()?.to_string();
        let thread_id = envelope.get("threadId")?.as_str()?.to_string();
        let message_id = envelope
            .get("messageId")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let received_at = envelope
            .get("receivedAt")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let sent_at = envelope
            .get("sentAt")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let size = envelope.get("size").and_then(|v| v.as_i64());
        let subject = envelope
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let from_json = envelope
            .get("from")
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let has_attachment = envelope
            .get("hasAttachment")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mailbox_ids = envelope
            .get("mailboxIds")
            .and_then(|v| v.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let keywords = envelope
            .get("keywords")
            .and_then(|v| v.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let attachments = envelope
            .get("attachments")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(AttachmentRow::from_json).collect())
            .unwrap_or_default();
        Some(Self {
            id,
            account_id: account_id.to_string(),
            thread_id,
            blob_id,
            message_id,
            received_at,
            sent_at,
            size,
            subject,
            from_json,
            has_attachment,
            mailbox_ids,
            keywords,
            attachments,
        })
    }
}

impl AttachmentRow {
    fn from_json(v: &Value) -> Option<Self> {
        let part_id = v.get("partId")?.as_str()?.to_string();
        let blob_id = v.get("blobId")?.as_str()?.to_string();
        Some(Self {
            part_id,
            blob_id,
            name: v.get("name").and_then(|x| x.as_str()).map(str::to_string),
            content_type: v.get("type").and_then(|x| x.as_str()).map(str::to_string),
            size: v.get("size").and_then(|x| x.as_i64()),
            disposition: v
                .get("disposition")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            cid: v.get("cid").and_then(|x| x.as_str()).map(str::to_string),
        })
    }
}

/// Index on `emails(thread_id)` — supports the "all emails in this
/// thread" lookup that pulls a conversation together.
pub const EMAILS_BY_THREAD_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS emails_by_thread ON emails(thread_id)";

/// Composite index on `emails(account_id, received_at)` — supports
/// per-account time-ordered queries (the "show me my recent mail"
/// pattern) without a full-table scan.
pub const EMAILS_BY_ACCOUNT_RECEIVED_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
        emails_by_account_received ON emails(account_id, received_at)";

/// Composite index on `emails(blob_id, blake3)` — supports the
/// `EmailBlobReader` skip-check `WHERE blob_id = ? AND blake3 IS NOT NULL`.
pub const EMAILS_BY_BLOB_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS emails_by_blob ON emails(blob_id, blake3)";

// ── join tables ─────────────────────────────────────────────────────

/// `email_mailboxes` — many-to-many: an email can live in multiple
/// mailboxes simultaneously (because Fastmail and friends model
/// labels as mailboxes). Refreshed delete-then-insert per email on
/// every email upsert. No bookkeeping sidecar; lifecycle is the
/// parent email's transaction.
pub const EMAIL_MAILBOXES_DDL: &str = "CREATE TABLE IF NOT EXISTS email_mailboxes (
    email_id TEXT NOT NULL,
    mailbox_id TEXT NOT NULL,
    PRIMARY KEY (email_id, mailbox_id)
)";

/// Index on `email_mailboxes(mailbox_id)` — supports the
/// reverse-direction "all emails in this mailbox" query.
pub const EMAIL_MAILBOXES_BY_MAILBOX_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
        email_mailboxes_by_mailbox ON email_mailboxes(mailbox_id)";

/// `email_keywords` — many-to-many between emails and JMAP
/// keywords (`$seen`, `$flagged`, IMAP-style user keywords, …).
/// Refreshed delete-then-insert per email upsert, no bookkeeping
/// sidecar.
pub const EMAIL_KEYWORDS_DDL: &str = "CREATE TABLE IF NOT EXISTS email_keywords (
    email_id TEXT NOT NULL,
    keyword TEXT NOT NULL,
    PRIMARY KEY (email_id, keyword)
)";

/// Index on `email_keywords(keyword)` — supports "all emails with
/// this keyword" queries (e.g. unread, flagged).
pub const EMAIL_KEYWORDS_BY_KEYWORD_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
        email_keywords_by_keyword ON email_keywords(keyword)";

/// `email_attachments` — per-email attachment metadata. The bytes
/// themselves live in the shared `blobs` table discriminated by
/// [`BLOB_KIND_ATTACHMENT`]; this table carries everything we know
/// about each part as exposed by JMAP. Refreshed
/// delete-then-insert per email upsert, no bookkeeping sidecar.
///
/// Phase 2 of the email port will replace this table with a
/// per-provider CAS-backed edge table (mirroring signal's
/// `chat_item_attachments`) that retires the shared `blob_refs`
/// usage. For now it's the original JMAP-id-shaped table.
pub const EMAIL_ATTACHMENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS email_attachments (
    email_id TEXT NOT NULL,
    part_id TEXT NOT NULL,
    blob_id TEXT NOT NULL,
    blake3 TEXT NULL,
    name TEXT NULL,
    type TEXT NULL,
    size INTEGER NULL,
    disposition TEXT NULL,
    cid TEXT NULL,
    PRIMARY KEY (email_id, part_id),
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)";

/// Composite index on `email_attachments(blob_id, blake3)` — supports
/// the `EmailBlobReader` skip-check and the "which emails reference
/// this blob?" GC walk in a single index.
pub const EMAIL_ATTACHMENTS_BY_BLOB_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
        email_attachments_by_blob ON email_attachments(blob_id, blake3)";

// ── cursor table ────────────────────────────────────────────────────

/// `mbox_files_checkpoint` — one row per mbox file the extractor has
/// fully ingested. The mbox extractor consults this before opening
/// each file: if the on-disk `(size_bytes, mtime_ns)` still match the
/// stamped row, the file is skipped entirely. Append-only mbox
/// semantics (mail clients only ever append) make `(size, mtime)` a
/// sufficient fingerprint without re-hashing contents.
///
/// Not an entity table and not in [`DATA_TABLES`] — it's
/// extractor-side bookkeeping that survives `RawDb::reset` only by
/// being explicitly truncated alongside the data/join tables.
pub const MBOX_FILES_CHECKPOINT_DDL: &str = "CREATE TABLE IF NOT EXISTS mbox_files_checkpoint (
    path TEXT PRIMARY KEY,
    size_bytes INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    last_finished_at TEXT NOT NULL
)";

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`]: every entity + join
/// table, every CREATE-INDEX, plus the paired
/// `<table>_bookkeeping` DDL produced by the shared layer for
/// [`DATA_TABLES`] entries. (Join tables deliberately don't get
/// bookkeeping sidecars — see the module rustdoc.)
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        AccountRow::ddl(),
        MailboxRow::ddl(),
        MAILBOXES_BY_ACCOUNT_INDEX_DDL.to_string(),
        ThreadRow::ddl(),
        THREADS_BY_ACCOUNT_INDEX_DDL.to_string(),
        EMAILS_DDL.to_string(),
        EMAILS_BY_THREAD_INDEX_DDL.to_string(),
        EMAILS_BY_ACCOUNT_RECEIVED_INDEX_DDL.to_string(),
        EMAILS_BY_BLOB_INDEX_DDL.to_string(),
        EMAIL_MAILBOXES_DDL.to_string(),
        EMAIL_MAILBOXES_BY_MAILBOX_INDEX_DDL.to_string(),
        EMAIL_KEYWORDS_DDL.to_string(),
        EMAIL_KEYWORDS_BY_KEYWORD_INDEX_DDL.to_string(),
        EMAIL_ATTACHMENTS_DDL.to_string(),
        EMAIL_ATTACHMENTS_BY_BLOB_INDEX_DDL.to_string(),
        MBOX_FILES_CHECKPOINT_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
