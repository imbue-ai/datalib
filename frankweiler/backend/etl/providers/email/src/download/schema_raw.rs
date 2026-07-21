//! Raw-store schema for the email provider.
//!
//! Declarations-only, proto-flavored. The schema is the same regardless
//! of where the data came from — Mbox and JMAP both populate it.
//!
//! ## Every table is a Rust struct
//!
//! There is no hand-written `CREATE TABLE` / `CREATE INDEX` text and no
//! hand-written `BulkUpsertable` in this file. Each table is a struct
//! deriving [`RawTable`](frankweiler_etl_macros::RawTable) (or, for the
//! CAS edge, [`CasEdgeRow`](frankweiler_etl_macros::CasEdgeRow)); the
//! derive emits the DDL, the index DDLs, and the bulk-upsert plumbing.
//! `full_ddl()` is then just a concatenation of each struct's
//! `all_ddl()`. Every write goes through the generic
//! `frankweiler_etl::bulk` helpers.
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
//! we don't download them into separate CAS entries during ingest.
//! Both Mbox and JMAP land *only the `.eml`* in the CAS.
//!
//! ## `emails` carries the envelope as `payload`
//!
//! [`EmailRow`] is payload-shaped like every other entity table: the
//! `id`/`payload` pair plus promoted metadata columns (time, subject,
//! from/to/cc, message-id, threading headers, the `.eml`'s blob ref).
//! The `payload` is the JMAP `Email/get` envelope (envelope-only — no
//! body parts; the body comes back from the `.eml`), and the Mbox path
//! synthesizes a JMAP-shaped envelope so both sources are identical.
//! The promoted columns exist for indexing / cheap projection; the
//! `mailboxIds` / `keywords` join inputs are read back out of the
//! payload (see [`EmailRow::mailbox_ids`] / [`EmailRow::keywords`]).
//!
//! ## The `.eml` content hash lives on `email_blobs`
//!
//! The CAS `blake3` for each message's `.eml` is **not** a column on
//! `emails` — it has a second writer (the blob-download pass backfills
//! it after the envelope row already exists), so it lives on its own
//! [`EmlBlobRow`] CAS edge table, exactly like every other provider's
//! attachment edge. That keeps `emails` single-writer: re-upserting a
//! changed envelope (flag/move churn) never clobbers a stored hash.
//!
//! ## Tables
//!
//! - `accounts`, `mailboxes`, `threads`, `emails` — payload-shaped
//!   entity tables ([`RawTable`] payload mode); each gets a paired
//!   `<table>_bookkeeping` sidecar.
//! - `email_mailboxes`, `email_keywords` — two N:M join tables
//!   ([`RawTable`] plain mode, synthesized `id` PK) refreshed
//!   delete-then-insert per email upsert. No bookkeeping sidecars.
//! - `email_blobs` — CAS edge ([`EmlBlobRow`]) carrying the `.eml`
//!   `blake3`, NULL until the bytes land in the CAS.
//! - `mbox_files_checkpoint` — Mbox-only cursor ([`RawTable`] plain
//!   mode, PK `path`): per file, the `(size_bytes, mtime_ns)` stamp
//!   from the last full ingest. Lets `mbox::fetch` skip files that
//!   haven't been appended to since the last run.

use frankweiler_etl::blob_cas::CasEdgeRow as _;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload};
use frankweiler_etl_macros::{CasEdgeRow, RawTable};
use serde_json::Value;

/// Entity tables — what `dolt diff` should see across re-fetches.
/// Each gets a paired `<table>_bookkeeping` sidecar. `email_blobs` is
/// here too: the shared CAS-edge flush
/// ([`frankweiler_etl::blob_cas::flush_cas_edges`]) stamps
/// `email_blobs_bookkeeping` for error tracking, so the sidecar must
/// exist, and `RawDb::reset` truncates the pair via
/// [`frankweiler_etl::doltlite_raw::truncate_data_tables`].
pub const DATA_TABLES: &[&str] = &["accounts", "mailboxes", "threads", "emails", "email_blobs"];

/// Many-to-many join tables. Not in [`DATA_TABLES`] because they're
/// refreshed delete-then-insert per parent email upsert; per-row
/// retry state would be noise. `RawDb::reset` truncates them
/// alongside [`DATA_TABLES`].
pub const JOIN_TABLES: &[&str] = &["email_mailboxes", "email_keywords"];

/// `blobs.kind` discriminator for the RFC5322 `.eml` source of an
/// email, stored in the shared `blobs` table keyed by JMAP
/// `Email.blobId` (or `sha256(eml_bytes)` for Mbox).
pub const BLOB_KIND_EML: &str = "email";

// ── entity rows ─────────────────────────────────────────────────────

/// `accounts` — one row per JMAP account or Mbox-config-supplied
/// account.
///
/// For Mbox: the orchestrator passes a `MboxAccountConfig { id,
/// name, email_address, is_personal }` from the YAML; one row
/// lands per configured Mbox input.
///
/// For JMAP: one row per account exposed in the session response.
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "accounts")]
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
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "mailboxes", index = "mailboxes_by_account:account_id")]
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

/// `threads` — one row per JMAP Thread or Mbox-derived thread
/// grouping.
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "threads", index = "threads_by_account:account_id")]
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

/// `emails` — one row per email. Payload-shaped: the `id`/`payload`
/// pair (payload = JMAP `Email/get` envelope, envelope-only) plus
/// promoted metadata columns.
///
/// The body and all attachment bytes live inside the `.eml` blob in
/// the CAS, reachable via `blob_id` → [`EmlBlobRow`] `blake3` →
/// `cas_objects.bytes`. Render mail-parses the `.eml` on demand for
/// both body display and per-part attachment extraction.
///
/// `references_header` (not `references`, which is a SQL reserved
/// word) holds the space-joined `References:` message-ids.
/// `has_attachment` is `0`/`1` in an INTEGER column.
#[derive(Debug, Clone, RawTable)]
#[raw_table(
    table = "emails",
    index = "emails_by_thread:thread_id",
    index = "emails_by_account_received:account_id,received_at"
)]
pub struct EmailRow {
    pub id_and_payload: WirePayload,
    pub account_id: String,
    pub thread_id: String,
    pub blob_id: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references_header: Option<String>,
    pub received_at: Option<String>,
    pub sent_at: Option<String>,
    pub size: Option<i64>,
    pub subject: Option<String>,
    pub from_json: Option<String>,
    pub to_json: Option<String>,
    pub cc_json: Option<String>,
    pub has_attachment: Option<i64>,
}

impl EmailRow {
    /// Promote metadata columns from a JMAP `Email/get` envelope (or a
    /// JMAP-shaped envelope synthesized by the mbox path), storing the
    /// whole envelope as the `payload`. Returns `None` if the required
    /// identifiers (`id`, `blobId`, `threadId`) are missing. Body
    /// parts in the JMAP response are deliberately ignored — render
    /// reads them out of the `.eml` blob.
    pub fn from_jmap_envelope(account_id: &str, envelope: &Value) -> Option<Self> {
        let id = envelope.get("id")?.as_str()?.to_string();
        let blob_id = envelope.get("blobId")?.as_str()?.to_string();
        let thread_id = envelope.get("threadId")?.as_str()?.to_string();
        let message_id = first_str(envelope.get("messageId"));
        let in_reply_to = first_str(envelope.get("inReplyTo"));
        let references_header = envelope
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
        Some(Self {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(envelope).ok()?,
            },
            account_id: account_id.to_string(),
            thread_id,
            blob_id,
            message_id,
            in_reply_to,
            references_header,
            received_at,
            sent_at,
            size,
            subject,
            from_json,
            to_json,
            cc_json,
            has_attachment: Some(has_attachment as i64),
        })
    }

    /// The email's primary key (the JMAP `Email.id`, or the mbox
    /// `Message-ID` / content hash fallback).
    pub fn id(&self) -> &str {
        &self.id_and_payload.id
    }

    /// `mailboxIds` keys read back out of the stored envelope payload.
    /// Drives both the per-email join refresh and the
    /// `--only-mailbox` client-side filter.
    pub fn mailbox_ids(&self) -> Vec<String> {
        self.payload_object_keys("mailboxIds")
    }

    /// `keywords` keys read back out of the stored envelope payload.
    pub fn keywords(&self) -> Vec<String> {
        self.payload_object_keys("keywords")
    }

    fn payload_object_keys(&self, field: &str) -> Vec<String> {
        serde_json::from_str::<Value>(&self.id_and_payload.payload)
            .ok()
            .as_ref()
            .and_then(|v| v.get(field))
            .and_then(|v| v.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// First string element of a JMAP header array (`messageId`,
/// `inReplyTo` are arrays of message-ids), stripped of angle brackets
/// by the upstream/parser already.
fn first_str(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

// ── join tables ─────────────────────────────────────────────────────

/// `email_mailboxes` — N:M: an email can live in multiple mailboxes
/// simultaneously (Fastmail and friends model labels as mailboxes;
/// Gmail does the same via `X-Gmail-Labels:`). Synthesized `id` PK
/// (`"{email_id}#{mailbox_id}"`) so the generic bulk-upsert's single
/// conflict column applies; refreshed delete-then-insert per email
/// upsert. No bookkeeping sidecar.
#[derive(Debug, Clone, RawTable)]
#[raw_table(
    table = "email_mailboxes",
    index = "email_mailboxes_by_mailbox:mailbox_id"
)]
pub struct EmailMailboxRow {
    pub id: String,
    pub email_id: String,
    pub mailbox_id: String,
}

impl EmailMailboxRow {
    pub fn new(email_id: &str, mailbox_id: &str) -> Self {
        Self {
            id: format!("{email_id}#{mailbox_id}"),
            email_id: email_id.to_string(),
            mailbox_id: mailbox_id.to_string(),
        }
    }
}

/// `email_keywords` — N:M: JMAP keywords (`$seen`, `$flagged`, user
/// keywords). Mbox equivalents come from `Status:` / `X-Status:`
/// headers (`R` → seen, `F` → flagged) and from `X-Keywords:` when
/// present. Synthesized `id` PK like [`EmailMailboxRow`].
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "email_keywords", index = "email_keywords_by_keyword:keyword")]
pub struct EmailKeywordRow {
    pub id: String,
    pub email_id: String,
    pub keyword: String,
}

impl EmailKeywordRow {
    pub fn new(email_id: &str, keyword: &str) -> Self {
        Self {
            id: format!("{email_id}#{keyword}"),
            email_id: email_id.to_string(),
            keyword: keyword.to_string(),
        }
    }
}

// ── CAS edge ────────────────────────────────────────────────────────

/// `email_blobs` — CAS edge from an email to its `.eml` bytes. One
/// row per `(email_id, blob_id)`; `blake3` is NULL until the blob
/// download pass stores the bytes and backfills the hash. Carrying
/// the hash here rather than on `emails` keeps the envelope table
/// single-writer. The universal `(ref, blake3)` index supports the
/// "have we stored this `.eml`?" skip-check and render's
/// `blob_id IN (…) AND blake3 IS NOT NULL` projection.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "email_blobs")]
pub struct EmlBlobRow {
    pub id: String,
    pub email_id: String,
    pub blob_id: String,
    pub blake3: Option<String>,
}

impl EmlBlobRow {
    /// A fresh edge with `blake3` unset (the download pass backfills
    /// it once the `.eml` bytes are stored in the CAS).
    pub fn new(email_id: &str, blob_id: &str) -> Self {
        Self {
            id: Self::pk_recipe(email_id, blob_id),
            email_id: email_id.to_string(),
            blob_id: blob_id.to_string(),
            blake3: None,
        }
    }
}

// ── cursor table ────────────────────────────────────────────────────

/// `mbox_files_checkpoint` — Mbox-only resume cursor. One row per
/// mbox file the extractor has fully ingested. Before opening a
/// file, `mbox::fetch` checks the row: if `(size_bytes, mtime_ns)`
/// match what's on disk, the file is skipped entirely. Mbox is
/// append-only by convention (mail clients only ever append), so
/// `(size, mtime)` is a sufficient fingerprint without re-hashing
/// contents.
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "mbox_files_checkpoint", primary_key = "path")]
pub struct MboxFilesCheckpointRow {
    pub path: String,
    pub size_bytes: i64,
    pub mtime_ns: i64,
    pub last_finished_at: String,
}

impl MboxFilesCheckpointRow {
    pub fn new(path: &str, size_bytes: i64, mtime_ns: i64, last_finished_at: &str) -> Self {
        Self {
            path: path.to_string(),
            size_bytes,
            mtime_ns,
            last_finished_at: last_finished_at.to_string(),
        }
    }
}

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`].
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    out.extend(AccountRow::all_ddl());
    out.extend(MailboxRow::all_ddl());
    out.extend(ThreadRow::all_ddl());
    out.extend(EmailRow::all_ddl());
    out.extend(EmailMailboxRow::all_ddl());
    out.extend(EmailKeywordRow::all_ddl());
    out.extend(EmlBlobRow::all_ddl());
    out.extend(MboxFilesCheckpointRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
