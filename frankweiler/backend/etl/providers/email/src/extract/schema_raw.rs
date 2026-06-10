//! Raw-store schema for the JMAP (email) provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture.md`](../../../../../docs/data_architecture.md)
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

use frankweiler_etl::doltlite_raw as dr;

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

// ── entity tables ───────────────────────────────────────────────────

/// `accounts` — one row per JMAP account known to the session.
///
/// Columns:
/// - `id` — upstream JMAP `accountId`. Primary key.
/// - `name` — display name from the session response.
/// - `is_personal` — JMAP `isPersonal` flag (boolean stored 0/1).
/// - `is_read_only` — JMAP `isReadOnly` flag (0/1).
/// - `payload` — raw session-fragment account entry
///   (JSONB-encoded on disk).
pub const ACCOUNTS_DDL: &str = "CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    name TEXT NULL,
    is_personal INTEGER NULL,
    is_read_only INTEGER NULL,
    payload TEXT NULL
)";

/// `mailboxes` — one row per JMAP Mailbox.
///
/// Fastmail (and other JMAP servers) uses mailboxes as both folders
/// and labels; this table is the source of truth for label names.
///
/// Columns:
/// - `id` — upstream JMAP Mailbox `id`. Primary key.
/// - `account_id` — owning account; FK into [`ACCOUNTS_DDL`].
/// - `name` — denormalized display name.
/// - `parent_id` — parent mailbox `id` when this is a sub-mailbox,
///   NULL at the root.
/// - `role` — JMAP system role (`"inbox"`, `"sent"`, `"trash"`,
///   `"archive"`, …) when set.
/// - `sort_order` — server-supplied display order.
/// - `total_emails`, `unread_emails` — counts surfaced for cheap
///   listing display.
/// - `payload` — raw `Mailbox/get` entry (JSONB-encoded on disk).
pub const MAILBOXES_DDL: &str = "CREATE TABLE IF NOT EXISTS mailboxes (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    name TEXT NULL,
    parent_id TEXT NULL,
    role TEXT NULL,
    sort_order INTEGER NULL,
    total_emails INTEGER NULL,
    unread_emails INTEGER NULL,
    payload TEXT NULL
)";

/// Index on `mailboxes(account_id)` — supports the per-account
/// listing walk without a full-table scan.
pub const MAILBOXES_BY_ACCOUNT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS mailboxes_by_account ON mailboxes(account_id)";

/// `threads` — one row per JMAP Thread.
///
/// Threads are the JMAP grouping for "messages in the same
/// conversation"; an email belongs to exactly one thread.
///
/// Columns:
/// - `id` — upstream JMAP Thread `id`. Primary key.
/// - `account_id` — owning account; FK into [`ACCOUNTS_DDL`].
/// - `email_count` — denormalized count of emails in the thread.
/// - `payload` — raw `Thread/get` entry (JSONB-encoded on disk).
pub const THREADS_DDL: &str = "CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    email_count INTEGER NULL,
    payload TEXT NULL
)";

/// Index on `threads(account_id)` — supports per-account thread
/// queries.
pub const THREADS_BY_ACCOUNT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS threads_by_account ON threads(account_id)";

/// `emails` — one row per JMAP Email.
///
/// The full `Email/get` response lives in `payload`; a handful of
/// fields are promoted to typed columns for cheap querying. The
/// RFC5322 `.eml` source for each email lives in the shared
/// `blobs` table keyed by `Email.blobId` and discriminated by
/// [`BLOB_KIND_EML`].
///
/// Columns:
/// - `id` — upstream JMAP Email `id`. Primary key.
/// - `account_id` — owning account; FK into [`ACCOUNTS_DDL`].
/// - `thread_id` — owning thread; FK into [`THREADS_DDL`].
/// - `blob_id` — upstream JMAP `Email.blobId`: a server-opaque
///   string the JMAP server issues for this email's RFC5322 source
///   bytes (e.g. Fastmail returns strings like `"B-eml-1"`). This
///   is **not** the bytes' blake3 hash; it's the JMAP-side
///   identifier we use as `ref_id` when calling into the shared
///   blob CAS. The shared `blob_refs(ref_id, blake3)` table is
///   what translates this opaque JMAP id to the blake3 that keys
///   the actual bytes in `cas_objects`.
/// - `message_id` — RFC 822 `Message-ID:` header value when
///   present.
/// - `received_at` — upstream `Email.receivedAt` (UTC ISO-8601).
///   The event timestamp for this row.
/// - `sent_at` — upstream `Email.sentAt` (the Date header).
/// - `size` — JMAP-reported size in bytes.
/// - `subject` — denormalized subject for cheap listing display.
/// - `from_json` — promoted JSON of the From: header(s); kept as
///   JSON because RFC 5322 permits multiple addresses, with
///   display names and groups.
/// - `has_attachment` — 0/1 flag from JMAP.
/// - `payload` — raw `Email/get` response (JSONB-encoded on disk).
pub const EMAILS_DDL: &str = "CREATE TABLE IF NOT EXISTS emails (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    blob_id TEXT NOT NULL,
    message_id TEXT NULL,
    received_at TEXT NULL,
    sent_at TEXT NULL,
    size INTEGER NULL,
    subject TEXT NULL,
    from_json TEXT NULL,
    has_attachment INTEGER NULL,
    payload TEXT NULL
)";

/// Index on `emails(thread_id)` — supports the "all emails in this
/// thread" lookup that pulls a conversation together.
pub const EMAILS_BY_THREAD_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS emails_by_thread ON emails(thread_id)";

/// Composite index on `emails(account_id, received_at)` — supports
/// per-account time-ordered queries (the "show me my recent mail"
/// pattern) without a full-table scan.
pub const EMAILS_BY_ACCOUNT_RECEIVED_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
        emails_by_account_received ON emails(account_id, received_at)";

// ── join tables ─────────────────────────────────────────────────────

/// `email_mailboxes` — many-to-many: an email can live in multiple
/// mailboxes simultaneously (because Fastmail and friends model
/// labels as mailboxes). Refreshed delete-then-insert per email on
/// every email upsert. No bookkeeping sidecar; lifecycle is the
/// parent email's transaction.
///
/// Columns:
/// - `email_id` — FK into [`EMAILS_DDL`].
/// - `mailbox_id` — FK into [`MAILBOXES_DDL`].
/// - Primary key is the composite `(email_id, mailbox_id)`.
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
///
/// Columns:
/// - `email_id` — FK into [`EMAILS_DDL`].
/// - `keyword` — the keyword string.
/// - Primary key is the composite `(email_id, keyword)`.
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
/// Columns:
/// - `email_id` — FK into [`EMAILS_DDL`].
/// - `part_id` — JMAP part id (stable within one email).
/// - `blob_id` — upstream JMAP attachment `blobId`: same shape as
///   `emails.blob_id` — a server-opaque string (e.g. `"B-att-1"`),
///   **not** a blake3 hash. Used as `ref_id` into the shared
///   `blob_refs(ref_id, blake3)` table, which carries the blake3
///   that keys the actual bytes in `cas_objects`. Two attachments
///   with identical bytes converge on a single blake3 (CAS-side
///   dedup) but each retains its own per-email JMAP `blob_id`
///   here — this column tracks the JMAP entity, not the bytes.
/// - `name` — original filename when present.
/// - `type` — MIME type.
/// - `size` — JMAP-reported size in bytes.
/// - `disposition` — `inline` / `attachment` per RFC 2183.
/// - `cid` — Content-ID for inline-rendered images.
/// - Primary key is the composite `(email_id, part_id)`.
pub const EMAIL_ATTACHMENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS email_attachments (
    email_id TEXT NOT NULL,
    part_id TEXT NOT NULL,
    blob_id TEXT NOT NULL,
    name TEXT NULL,
    type TEXT NULL,
    size INTEGER NULL,
    disposition TEXT NULL,
    cid TEXT NULL,
    PRIMARY KEY (email_id, part_id)
)";

/// Index on `email_attachments(blob_id)` — supports the
/// "which emails reference this blob?" query that
/// `blob_cas::gc_orphans` walks at GC time.
pub const EMAIL_ATTACHMENTS_BY_BLOB_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
        email_attachments_by_blob ON email_attachments(blob_id)";

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`]: every entity + join
/// table, every CREATE-INDEX, plus the paired
/// `<table>_bookkeeping` DDL produced by the shared layer for
/// [`DATA_TABLES`] entries. (Join tables deliberately don't get
/// bookkeeping sidecars — see the module rustdoc.)
///
/// Schema-local glue, kept here so the "what tables exist?" answer
/// is one function call from this file. Heavier composition (e.g. a
/// repo-wide bookkeeping macro) is deferred to P1.1.
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        ACCOUNTS_DDL.to_string(),
        MAILBOXES_DDL.to_string(),
        MAILBOXES_BY_ACCOUNT_INDEX_DDL.to_string(),
        THREADS_DDL.to_string(),
        THREADS_BY_ACCOUNT_INDEX_DDL.to_string(),
        EMAILS_DDL.to_string(),
        EMAILS_BY_THREAD_INDEX_DDL.to_string(),
        EMAILS_BY_ACCOUNT_RECEIVED_INDEX_DDL.to_string(),
        EMAIL_MAILBOXES_DDL.to_string(),
        EMAIL_MAILBOXES_BY_MAILBOX_INDEX_DDL.to_string(),
        EMAIL_KEYWORDS_DDL.to_string(),
        EMAIL_KEYWORDS_BY_KEYWORD_INDEX_DDL.to_string(),
        EMAIL_ATTACHMENTS_DDL.to_string(),
        EMAIL_ATTACHMENTS_BY_BLOB_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
