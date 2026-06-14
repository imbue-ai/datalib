//! Raw-store schema for the email provider.
//!
//! Declarations-only, proto-flavored. The schema is the same regardless
//! of where the data came from — Mbox and JMAP both populate it.
//!
//! ## The eml is the canonical body
//!
//! The RFC 5322 `.eml` is the **complete backup** of a message: body,
//! headers, MIME parts (attachments included). It rides in the shared
//! per-source CAS keyed by `blob_id`. Everything else is metadata
//! around it.
//!
//! Concretely: there is no `email_attachments` table. The parts inside
//! an `.eml` are reachable by mail-parsing the bytes at render time;
//! we don't extract them into separate CAS entries during ingest.
//! Both Mbox and JMAP land *only the `.eml`* in the CAS. JMAP no
//! longer calls `Blob/download` per attachment — only for the `.eml`
//! itself.
//!
//! ## `emails` is metadata-only
//!
//! [`EmailRow`] is the event table: time, subject, from/to/cc,
//! message-id, threading headers, the `.eml`'s blob ref. **No body
//! columns, no JSONB payload.** The body comes back from the `.eml`
//! when render needs it.
//!
//! ## Tables
//!
//! - `accounts`, `mailboxes`, `threads` — wire-payload entity tables
//!   (`WirePayloadRow` derive); JSONB payload preserved.
//! - `emails` — envelope-only event table; hand-rolled
//!   `BulkUpsertable` (no payload column).
//! - `email_mailboxes`, `email_keywords` — two many-to-many join
//!   tables refreshed delete-then-insert per email upsert. No
//!   bookkeeping sidecars.
//! - `mbox_files_checkpoint` — Mbox-only cursor: per file, the
//!   `(size_bytes, mtime_ns)` stamp from the last full ingest. Lets
//!   `mbox::fetch` skip files that haven't been appended to since
//!   the last run.

use frankweiler_etl::bulk::BulkUpsertable;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::WirePayloadRow;
use serde_json::Value;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// Entity tables — what `dolt diff` should see across re-fetches.
/// Each gets a paired `<table>_bookkeeping` sidecar.
pub const DATA_TABLES: &[&str] = &["accounts", "mailboxes", "threads", "emails"];

/// Many-to-many join tables. Not in [`DATA_TABLES`] because they're
/// refreshed delete-then-insert per parent email upsert; per-row
/// retry state would be noise. `RawDb::reset` truncates them
/// alongside [`DATA_TABLES`].
pub const JOIN_TABLES: &[&str] = &["email_mailboxes", "email_keywords"];

/// `blobs.kind` discriminator for the RFC5322 `.eml` source of an
/// email, stored in the shared `blobs` table keyed by JMAP
/// `Email.blobId` (or `sha256(eml_bytes)` for Mbox).
pub const BLOB_KIND_EML: &str = "email";

// ── entity rows / DDLs ──────────────────────────────────────────────

/// `accounts` — one row per JMAP account or Mbox-config-supplied
/// account.
///
/// For Mbox: the orchestrator passes a `MboxAccountConfig { id,
/// name, email_address, is_personal }` from the YAML; one row
/// lands per configured Mbox input.
///
/// For JMAP: one row per account exposed in the session response.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "accounts")]
pub struct AccountRow {
    pub id_and_payload: WirePayload,
    pub name: Option<String>,
    pub email_address: Option<String>,
    pub is_personal: Option<i64>,
    pub is_read_only: Option<i64>,
}

impl AccountRow {
    /// Build from a JMAP session-fragment account entry.
    pub fn from_jmap_payload(id: &str, payload: &Value) -> anyhow::Result<Self> {
        Ok(Self {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: serde_json::to_string(payload)?,
            },
            name: payload
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            email_address: None,
            is_personal: payload
                .get("isPersonal")
                .and_then(|v| v.as_bool())
                .map(|b| b as i64),
            is_read_only: payload
                .get("isReadOnly")
                .and_then(|v| v.as_bool())
                .map(|b| b as i64),
        })
    }

    /// Build from an mbox `MboxAccountConfig`. Synthesizes a payload
    /// in the same shape the JMAP path produces so render can be
    /// source-agnostic.
    pub fn from_mbox_config(
        id: &str,
        name: Option<&str>,
        email_address: Option<&str>,
        is_personal: bool,
    ) -> Self {
        let payload = serde_json::json!({
            "id": id,
            "name": name,
            "emailAddress": email_address,
            "isPersonal": is_personal,
            "isReadOnly": false,
        });
        Self {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: payload.to_string(),
            },
            name: name.map(str::to_string),
            email_address: email_address.map(str::to_string),
            is_personal: Some(is_personal as i64),
            is_read_only: Some(0),
        }
    }
}

/// `mailboxes` — one row per JMAP Mailbox or Mbox-derived
/// folder/label.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "mailboxes")]
pub struct MailboxRow {
    pub id_and_payload: WirePayload,
    pub account_id: String,
    pub name: Option<String>,
    pub parent_id: Option<String>,
    pub role: Option<String>,
    pub sort_order: Option<i64>,
    pub total_emails: Option<i64>,
    pub unread_emails: Option<i64>,
}

impl MailboxRow {
    pub fn from_jmap_payload(account_id: &str, payload: &Value) -> anyhow::Result<Self> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("mailbox payload missing id"))?;
        Ok(Self {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: serde_json::to_string(payload)?,
            },
            account_id: account_id.to_string(),
            name: payload
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            parent_id: payload
                .get("parentId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            role: payload
                .get("role")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            sort_order: payload.get("sortOrder").and_then(|v| v.as_i64()),
            total_emails: payload.get("totalEmails").and_then(|v| v.as_i64()),
            unread_emails: payload.get("unreadEmails").and_then(|v| v.as_i64()),
        })
    }
}

pub const MAILBOXES_BY_ACCOUNT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS mailboxes_by_account ON mailboxes(account_id)";

/// `threads` — one row per JMAP Thread or Mbox-derived thread
/// grouping.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "threads")]
pub struct ThreadRow {
    pub id_and_payload: WirePayload,
    pub account_id: String,
    pub email_count: Option<i64>,
}

impl ThreadRow {
    pub fn from_jmap_payload(id: &str, account_id: &str, payload: &Value) -> anyhow::Result<Self> {
        let email_count = payload
            .get("emailIds")
            .and_then(|v| v.as_array())
            .map(|a| a.len() as i64);
        Ok(Self {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: serde_json::to_string(payload)?,
            },
            account_id: account_id.to_string(),
            email_count,
        })
    }
}

pub const THREADS_BY_ACCOUNT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS threads_by_account ON threads(account_id)";

/// `emails` — one row per email. **Metadata only.**
///
/// The body and all attachment bytes live inside the `.eml` blob in
/// the CAS, reachable via `blob_id → blake3 → cas_objects.bytes`.
/// Render mail-parses the `.eml` on demand for both body display and
/// per-part attachment extraction.
pub const EMAILS_DDL: &str = "CREATE TABLE IF NOT EXISTS emails (
    id              TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL,
    thread_id       TEXT NOT NULL,
    blob_id         TEXT NOT NULL,
    blake3          TEXT NULL,
    message_id      TEXT NULL,
    in_reply_to     TEXT NULL,
    \"references\"  TEXT NULL,
    received_at     TEXT NULL,
    sent_at         TEXT NULL,
    size            INTEGER NULL,
    subject         TEXT NULL,
    from_json       TEXT NULL,
    to_json         TEXT NULL,
    cc_json         TEXT NULL,
    has_attachment  INTEGER NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)";

/// Envelope-only `emails` row. Caller-supplied join inputs
/// (`mailbox_ids`, `keywords`) are NOT bound here — they're consumed
/// by the per-email join-refresh helper run alongside the bulk
/// upsert.
#[derive(Debug, Clone)]
pub struct EmailRow {
    pub id: String,
    pub account_id: String,
    pub thread_id: String,
    pub blob_id: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub received_at: Option<String>,
    pub sent_at: Option<String>,
    pub size: Option<i64>,
    pub subject: Option<String>,
    pub from_json: Option<String>,
    pub to_json: Option<String>,
    pub cc_json: Option<String>,
    pub has_attachment: bool,
    pub mailbox_ids: Vec<String>,
    pub keywords: Vec<String>,
}

impl BulkUpsertable for EmailRow {
    const TABLE: &'static str = "emails";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "account_id",
        "thread_id",
        "blob_id",
        "message_id",
        "in_reply_to",
        "\"references\"",
        "received_at",
        "sent_at",
        "size",
        "subject",
        "from_json",
        "to_json",
        "cc_json",
        "has_attachment",
    ];
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
            .bind(self.in_reply_to.as_deref())
            .bind(self.references.as_deref())
            .bind(self.received_at.as_deref())
            .bind(self.sent_at.as_deref())
            .bind(self.size)
            .bind(self.subject.as_deref())
            .bind(self.from_json.as_deref())
            .bind(self.to_json.as_deref())
            .bind(self.cc_json.as_deref())
            .bind(self.has_attachment as i64)
    }
}

impl EmailRow {
    /// Promote metadata columns from a JMAP `Email/get` envelope.
    /// Returns `None` if the required identifiers (`id`, `blobId`,
    /// `threadId`) are missing. Body parts in the JMAP response are
    /// deliberately ignored — render reads them out of the `.eml`
    /// blob.
    pub fn from_jmap_envelope(account_id: &str, envelope: &Value) -> Option<Self> {
        let id = envelope.get("id")?.as_str()?.to_string();
        let blob_id = envelope.get("blobId")?.as_str()?.to_string();
        let thread_id = envelope.get("threadId")?.as_str()?.to_string();
        let message_id = envelope
            .get("messageId")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let in_reply_to = envelope
            .get("inReplyTo")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let references = envelope
            .get("references")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty());
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
        let to_json = envelope
            .get("to")
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let cc_json = envelope
            .get("cc")
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
        Some(Self {
            id,
            account_id: account_id.to_string(),
            thread_id,
            blob_id,
            message_id,
            in_reply_to,
            references,
            received_at,
            sent_at,
            size,
            subject,
            from_json,
            to_json,
            cc_json,
            has_attachment,
            mailbox_ids,
            keywords,
        })
    }
}

pub const EMAILS_BY_THREAD_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS emails_by_thread ON emails(thread_id)";

pub const EMAILS_BY_ACCOUNT_RECEIVED_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
        emails_by_account_received ON emails(account_id, received_at)";

/// Index on `(blob_id, blake3)` — supports the skip-check
/// `WHERE blob_id = ? AND blake3 IS NOT NULL` and the per-thread
/// `BlobBundle::load` projection.
pub const EMAILS_BY_BLOB_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS emails_by_blob ON emails(blob_id, blake3)";

// ── join tables ─────────────────────────────────────────────────────

/// `email_mailboxes` — many-to-many: an email can live in multiple
/// mailboxes simultaneously (Fastmail and friends model labels as
/// mailboxes; Gmail does the same via `X-Gmail-Labels:`). Refreshed
/// delete-then-insert per upsert. No bookkeeping sidecar.
pub const EMAIL_MAILBOXES_DDL: &str = "CREATE TABLE IF NOT EXISTS email_mailboxes (
    email_id TEXT NOT NULL,
    mailbox_id TEXT NOT NULL,
    PRIMARY KEY (email_id, mailbox_id)
)";

pub const EMAIL_MAILBOXES_BY_MAILBOX_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
        email_mailboxes_by_mailbox ON email_mailboxes(mailbox_id)";

/// `email_keywords` — many-to-many: JMAP keywords (`$seen`,
/// `$flagged`, user keywords). Mbox equivalents come from
/// `Status:` / `X-Status:` headers (`R` → seen, `F` → flagged) and
/// from `X-Keywords:` when present.
pub const EMAIL_KEYWORDS_DDL: &str = "CREATE TABLE IF NOT EXISTS email_keywords (
    email_id TEXT NOT NULL,
    keyword TEXT NOT NULL,
    PRIMARY KEY (email_id, keyword)
)";

pub const EMAIL_KEYWORDS_BY_KEYWORD_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
        email_keywords_by_keyword ON email_keywords(keyword)";

// ── cursor table ────────────────────────────────────────────────────

/// `mbox_files_checkpoint` — Mbox-only resume cursor. One row per
/// mbox file the extractor has fully ingested. Before opening a
/// file, `mbox::fetch` checks the row: if `(size_bytes, mtime_ns)`
/// match what's on disk, the file is skipped entirely. Mbox is
/// append-only by convention (mail clients only ever append), so
/// `(size, mtime)` is a sufficient fingerprint without re-hashing
/// contents.
pub const MBOX_FILES_CHECKPOINT_DDL: &str = "CREATE TABLE IF NOT EXISTS mbox_files_checkpoint (
    path TEXT PRIMARY KEY,
    size_bytes INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    last_finished_at TEXT NOT NULL
)";

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`].
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
        MBOX_FILES_CHECKPOINT_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
