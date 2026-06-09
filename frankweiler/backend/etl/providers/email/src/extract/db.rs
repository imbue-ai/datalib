//! Doltlite-backed raw store for the JMAP provider.
//!
//! Single sqlite file at `<data_root>/raw/<name>.doltlite_db`. Shared
//! bookkeeping (`blobs`, `sync_runs`, `sync_scope_state`) and the
//! open / blob plumbing live in
//! [`frankweiler_etl::doltlite_raw`].
//!
//! Object tables (each carries a `<table>_bookkeeping` sidecar):
//!
//! - `accounts` — PK is JMAP `accountId`.
//! - `mailboxes` — PK is JMAP Mailbox `id`. Fastmail uses mailboxes as
//!   both folders and labels; this table is the source of truth for
//!   label names.
//! - `threads` — PK is JMAP Thread `id`.
//! - `emails` — PK is JMAP Email `id`. The full `Email/get` response is
//!   stored as JSONB in `payload`; a handful of fields are promoted to
//!   typed columns for cheap querying. The `.eml` RFC5322 source for
//!   the message lives in the shared `blobs` table keyed by
//!   `Email.blobId`.
//!
//! Join tables (no bookkeeping — owned by their parent email's
//! transaction):
//!
//! - `email_mailboxes` — `(email_id, mailbox_id)`. Refreshed
//!   delete-then-insert per email on every email upsert.
//! - `email_keywords` — `(email_id, keyword)`. Same refresh shape.
//! - `email_attachments` — `(email_id, part_id)` with promoted
//!   columns and a `blob_id` pointer into `blobs`. Refreshed
//!   delete-then-insert per email.
//!
//! Hard-delete semantics: when JMAP reports an email `destroyed`, we
//! DELETE the row + its joins + its bookkeeping. We do NOT delete blobs
//! (other emails may reference the same `.eml` blob or attachment).
//! Doltlite's history retains the prior state.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::{
    self, BlobCas, BlobReader, InMemoryBlobReader, RefStub, SqliteBlobReader,
};
use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

/// Object tables — wiped by [`RawDb::reset`].
const DATA_TABLES: &[&str] = &["accounts", "mailboxes", "threads", "emails"];

/// Join tables — also wiped by [`RawDb::reset`] but don't get
/// bookkeeping sidecars (they live and die with their parent email's
/// upsert transaction).
const JOIN_TABLES: &[&str] = &["email_mailboxes", "email_keywords", "email_attachments"];

/// Blob kinds we write into the shared `blobs` table.
pub const BLOB_KIND_EML: &str = "email";
pub const BLOB_KIND_ATTACHMENT: &str = "attachment";

const DDL_DATA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS accounts (
        id TEXT PRIMARY KEY,
        name TEXT NULL,
        is_personal INTEGER NULL,
        is_read_only INTEGER NULL,
        payload TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS mailboxes (
        id TEXT PRIMARY KEY,
        account_id TEXT NOT NULL,
        name TEXT NULL,
        parent_id TEXT NULL,
        role TEXT NULL,
        sort_order INTEGER NULL,
        total_emails INTEGER NULL,
        unread_emails INTEGER NULL,
        payload TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS mailboxes_by_account ON mailboxes(account_id)",
    "CREATE TABLE IF NOT EXISTS threads (
        id TEXT PRIMARY KEY,
        account_id TEXT NOT NULL,
        email_count INTEGER NULL,
        payload TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS threads_by_account ON threads(account_id)",
    "CREATE TABLE IF NOT EXISTS emails (
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
    )",
    "CREATE INDEX IF NOT EXISTS emails_by_thread ON emails(thread_id)",
    "CREATE INDEX IF NOT EXISTS emails_by_account_received \
        ON emails(account_id, received_at)",
    "CREATE TABLE IF NOT EXISTS email_mailboxes (
        email_id TEXT NOT NULL,
        mailbox_id TEXT NOT NULL,
        PRIMARY KEY (email_id, mailbox_id)
    )",
    "CREATE INDEX IF NOT EXISTS email_mailboxes_by_mailbox \
        ON email_mailboxes(mailbox_id)",
    "CREATE TABLE IF NOT EXISTS email_keywords (
        email_id TEXT NOT NULL,
        keyword TEXT NOT NULL,
        PRIMARY KEY (email_id, keyword)
    )",
    "CREATE INDEX IF NOT EXISTS email_keywords_by_keyword \
        ON email_keywords(keyword)",
    "CREATE TABLE IF NOT EXISTS email_attachments (
        email_id TEXT NOT NULL,
        part_id TEXT NOT NULL,
        blob_id TEXT NOT NULL,
        name TEXT NULL,
        type TEXT NULL,
        size INTEGER NULL,
        disposition TEXT NULL,
        cid TEXT NULL,
        PRIMARY KEY (email_id, part_id)
    )",
    "CREATE INDEX IF NOT EXISTS email_attachments_by_blob \
        ON email_attachments(blob_id)",
];

fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = DDL_DATA.iter().map(|s| (*s).to_string()).collect();
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

// ─────────────────────────────────────────────────────────────────────
// State-token namespacing
// ─────────────────────────────────────────────────────────────────────
//
// JMAP's incremental sync is driven by opaque per-type `state` tokens
// returned by `Foo/get` and consumed by `Foo/changes`. We persist them
// in the shared `sync_scope_state` table under provider-namespaced keys
// so multiple JMAP accounts in the same doltlite file don't collide.

pub fn state_scope(account_id: &str, type_name: &str) -> String {
    format!("jmap:{account_id}:state:{type_name}")
}

// ─────────────────────────────────────────────────────────────────────
// Row structs
// ─────────────────────────────────────────────────────────────────────

/// Promoted columns + raw JMAP payload for one email. Construct via
/// [`EmailRow::from_payload`] and then hand to [`RawDb::upsert_email`].
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
    pub payload: Value,
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

impl EmailRow {
    /// Promote the fields we want as columns from a JMAP `Email/get`
    /// response. Returns `None` if the payload is missing one of the
    /// required identifiers (`id`, `blobId`, `threadId`).
    pub fn from_payload(account_id: &str, payload: Value) -> Option<Self> {
        let id = payload.get("id")?.as_str()?.to_string();
        let blob_id = payload.get("blobId")?.as_str()?.to_string();
        let thread_id = payload.get("threadId")?.as_str()?.to_string();
        let message_id = payload
            .get("messageId")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let received_at = payload
            .get("receivedAt")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let sent_at = payload
            .get("sentAt")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let size = payload.get("size").and_then(|v| v.as_i64());
        let subject = payload
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let from_json = payload
            .get("from")
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let has_attachment = payload
            .get("hasAttachment")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mailbox_ids = payload
            .get("mailboxIds")
            .and_then(|v| v.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let keywords = payload
            .get("keywords")
            .and_then(|v| v.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let attachments = payload
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
            payload,
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

// ─────────────────────────────────────────────────────────────────────
// RawDb
// ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        let cas = BlobCas::open(&blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { pool, cas })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await?;
        let mut tx = self.pool.begin().await.context("begin reset tx")?;
        for table in JOIN_TABLES {
            let sql = format!("DELETE FROM {table}");
            sqlx::query(&sql)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("truncate {table}"))?;
        }
        sqlx::query("DELETE FROM sync_scope_state WHERE scope LIKE 'jmap:%'")
            .execute(&mut *tx)
            .await
            .context("clear jmap scope state on reset")?;
        tx.commit().await.context("commit reset tx")?;
        Ok(())
    }

    // ── state tokens ────────────────────────────────────────────────

    pub async fn load_state(&self, account_id: &str, type_name: &str) -> Result<Option<String>> {
        let scope = state_scope(account_id, type_name);
        let row = sqlx::query("SELECT last_seen_at FROM sync_scope_state WHERE scope = ?")
            .bind(&scope)
            .fetch_optional(&self.pool)
            .await
            .context("select state token")?;
        Ok(row.and_then(|r| r.try_get::<String, _>("last_seen_at").ok()))
    }

    pub async fn save_state(&self, account_id: &str, type_name: &str, token: &str) -> Result<()> {
        dr::upsert_scope_state(&self.pool, &state_scope(account_id, type_name), token).await
    }

    // ── accounts ───────────────────────────────────────────────────

    pub async fn upsert_account(&self, id: &str, payload: &Value) -> Result<()> {
        let name = payload.get("name").and_then(|v| v.as_str());
        let is_personal = payload.get("isPersonal").and_then(|v| v.as_bool());
        let is_read_only = payload.get("isReadOnly").and_then(|v| v.as_bool());
        let payload_str = serde_json::to_string(payload).context("serialize account")?;
        let mut tx = self.pool.begin().await.context("begin account tx")?;
        sqlx::query(
            "INSERT INTO accounts (id, name, is_personal, is_read_only, payload)
             VALUES (?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                name = COALESCE(excluded.name, accounts.name),
                is_personal = COALESCE(excluded.is_personal, accounts.is_personal),
                is_read_only = COALESCE(excluded.is_read_only, accounts.is_read_only),
                payload = excluded.payload",
        )
        .bind(id)
        .bind(name)
        .bind(is_personal.map(|b| b as i64))
        .bind(is_read_only.map(|b| b as i64))
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert account {id}"))?;
        dr::record_object_attempt(&mut tx, "accounts", id, None).await?;
        tx.commit().await.context("commit account tx")?;
        Ok(())
    }

    pub async fn load_accounts(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "accounts").await
    }

    // ── mailboxes ──────────────────────────────────────────────────

    pub async fn upsert_mailbox(&self, account_id: &str, payload: &Value) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin mailbox tx")?;
        upsert_mailbox_in(&mut tx, account_id, payload).await?;
        tx.commit().await.context("commit mailbox tx")?;
        Ok(())
    }

    pub async fn upsert_mailboxes(&self, account_id: &str, payloads: &[Value]) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin mailboxes batch tx")?;
        for p in payloads {
            upsert_mailbox_in(&mut tx, account_id, p).await?;
        }
        tx.commit().await.context("commit mailboxes batch tx")?;
        Ok(())
    }

    pub async fn delete_mailboxes(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin delete mailboxes tx")?;
        for id in ids {
            for sql in [
                "DELETE FROM mailboxes WHERE id = ?",
                "DELETE FROM mailboxes_bookkeeping WHERE id = ?",
            ] {
                sqlx::query(sql)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| format!("delete mailbox {id}"))?;
            }
        }
        tx.commit().await.context("commit delete mailboxes tx")?;
        Ok(())
    }

    pub async fn load_mailboxes(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "mailboxes").await
    }

    // ── threads ────────────────────────────────────────────────────

    pub async fn upsert_thread(&self, id: &str, account_id: &str, payload: &Value) -> Result<()> {
        let email_count = payload
            .get("emailIds")
            .and_then(|v| v.as_array())
            .map(|a| a.len() as i64);
        let payload_str = serde_json::to_string(payload).context("serialize thread")?;
        let mut tx = self.pool.begin().await.context("begin thread tx")?;
        sqlx::query(
            "INSERT INTO threads (id, account_id, email_count, payload)
             VALUES (?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                email_count = excluded.email_count,
                payload = excluded.payload",
        )
        .bind(id)
        .bind(account_id)
        .bind(email_count)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert thread {id}"))?;
        dr::record_object_attempt(&mut tx, "threads", id, None).await?;
        tx.commit().await.context("commit thread tx")?;
        Ok(())
    }

    pub async fn delete_threads(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin delete threads tx")?;
        for id in ids {
            for sql in [
                "DELETE FROM threads WHERE id = ?",
                "DELETE FROM threads_bookkeeping WHERE id = ?",
            ] {
                sqlx::query(sql)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| format!("delete thread {id}"))?;
            }
        }
        tx.commit().await.context("commit delete threads tx")?;
        Ok(())
    }

    pub async fn load_threads(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "threads").await
    }

    /// `id → email_count` for every thread we've persisted. The
    /// translate-side cheap probe uses this to skip re-rendering
    /// threads whose membership hasn't changed.
    pub async fn thread_email_counts(&self) -> Result<HashMap<String, i64>> {
        let rows = sqlx::query("SELECT id, email_count FROM threads")
            .fetch_all(&self.pool)
            .await
            .context("select thread_email_counts")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let n: Option<i64> = r.try_get("email_count").ok();
            if !id.is_empty() {
                out.insert(id, n.unwrap_or(0));
            }
        }
        Ok(out)
    }

    // ── emails ─────────────────────────────────────────────────────

    pub async fn upsert_email(&self, row: &EmailRow) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin email tx")?;
        upsert_email_in(&mut tx, row).await?;
        tx.commit().await.context("commit email tx")?;
        Ok(())
    }

    pub async fn upsert_emails(&self, rows: &[EmailRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin emails batch tx")?;
        for row in rows {
            upsert_email_in(&mut tx, row).await?;
        }
        tx.commit().await.context("commit emails batch tx")?;
        Ok(())
    }

    /// Hard-delete one email plus its joins + bookkeeping. Blobs are
    /// untouched — another email may share the same `.eml` blob or an
    /// attachment blob. Dolt history preserves the pre-delete state.
    pub async fn delete_emails(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin delete emails tx")?;
        for id in ids {
            for sql in [
                "DELETE FROM email_mailboxes WHERE email_id = ?",
                "DELETE FROM email_keywords WHERE email_id = ?",
                "DELETE FROM email_attachments WHERE email_id = ?",
                "DELETE FROM emails WHERE id = ?",
                "DELETE FROM emails_bookkeeping WHERE id = ?",
            ] {
                sqlx::query(sql)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| format!("delete email {id}"))?;
            }
        }
        tx.commit().await.context("commit delete emails tx")?;
        Ok(())
    }

    pub async fn load_emails(&self) -> Result<Vec<LoadedEmail>> {
        let rows = sqlx::query(
            "SELECT id, account_id, thread_id, blob_id, message_id, received_at, sent_at,
                    size, subject, from_json, has_attachment, json(payload) AS payload
             FROM emails
             WHERE payload IS NOT NULL
             ORDER BY thread_id, received_at, id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select emails")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload_str: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Ok(payload) = serde_json::from_str::<Value>(&payload_str) else {
                continue;
            };
            out.push(LoadedEmail {
                id: r.try_get("id").unwrap_or_default(),
                account_id: r.try_get("account_id").unwrap_or_default(),
                thread_id: r.try_get("thread_id").unwrap_or_default(),
                blob_id: r.try_get("blob_id").unwrap_or_default(),
                message_id: r.try_get::<Option<String>, _>("message_id").unwrap_or(None),
                received_at: r
                    .try_get::<Option<String>, _>("received_at")
                    .unwrap_or(None),
                sent_at: r.try_get::<Option<String>, _>("sent_at").unwrap_or(None),
                size: r.try_get::<Option<i64>, _>("size").unwrap_or(None),
                subject: r.try_get::<Option<String>, _>("subject").unwrap_or(None),
                has_attachment: r
                    .try_get::<Option<i64>, _>("has_attachment")
                    .unwrap_or(None)
                    .unwrap_or(0)
                    != 0,
                payload,
            });
        }
        Ok(out)
    }

    /// Snapshot every email's mailbox/keyword/attachment joins, keyed
    /// by `email_id`. Cheaper than per-row queries when the translate
    /// pass needs them all.
    pub async fn load_email_joins(&self) -> Result<EmailJoins> {
        let mut mailboxes: HashMap<String, Vec<String>> = HashMap::new();
        for r in sqlx::query("SELECT email_id, mailbox_id FROM email_mailboxes")
            .fetch_all(&self.pool)
            .await
            .context("load email_mailboxes")?
        {
            let e: String = r.try_get("email_id").unwrap_or_default();
            let m: String = r.try_get("mailbox_id").unwrap_or_default();
            if !e.is_empty() && !m.is_empty() {
                mailboxes.entry(e).or_default().push(m);
            }
        }
        let mut keywords: HashMap<String, Vec<String>> = HashMap::new();
        for r in sqlx::query("SELECT email_id, keyword FROM email_keywords")
            .fetch_all(&self.pool)
            .await
            .context("load email_keywords")?
        {
            let e: String = r.try_get("email_id").unwrap_or_default();
            let k: String = r.try_get("keyword").unwrap_or_default();
            if !e.is_empty() && !k.is_empty() {
                keywords.entry(e).or_default().push(k);
            }
        }
        let mut attachments: HashMap<String, Vec<LoadedAttachment>> = HashMap::new();
        for r in sqlx::query(
            "SELECT email_id, part_id, blob_id, name, type, size, disposition, cid
             FROM email_attachments ORDER BY email_id, part_id",
        )
        .fetch_all(&self.pool)
        .await
        .context("load email_attachments")?
        {
            let e: String = r.try_get("email_id").unwrap_or_default();
            if e.is_empty() {
                continue;
            }
            attachments.entry(e).or_default().push(LoadedAttachment {
                part_id: r.try_get("part_id").unwrap_or_default(),
                blob_id: r.try_get("blob_id").unwrap_or_default(),
                name: r.try_get::<Option<String>, _>("name").unwrap_or(None),
                content_type: r.try_get::<Option<String>, _>("type").unwrap_or(None),
                size: r.try_get::<Option<i64>, _>("size").unwrap_or(None),
                disposition: r
                    .try_get::<Option<String>, _>("disposition")
                    .unwrap_or(None),
                cid: r.try_get::<Option<String>, _>("cid").unwrap_or(None),
            });
        }
        Ok(EmailJoins {
            mailboxes,
            keywords,
            attachments,
        })
    }

    /// Every persisted email id — for a quick set-membership check
    /// during incremental sync ("do we already have this id?").
    pub async fn known_email_ids(&self) -> Result<HashSet<String>> {
        let rows = sqlx::query("SELECT id FROM emails WHERE payload IS NOT NULL")
            .fetch_all(&self.pool)
            .await
            .context("select known_email_ids")?;
        let mut out = HashSet::with_capacity(rows.len());
        for r in rows {
            if let Ok(id) = r.try_get::<String, _>("id") {
                out.insert(id);
            }
        }
        Ok(out)
    }

    // ── blobs (delegate to shared `blob_cas`) ───────────────────────

    pub async fn blob_exists(&self, ref_id: &str) -> Result<bool> {
        blob_cas::ref_has_hash(&self.pool, ref_id).await
    }

    /// Snapshot every ref_id that already has a hash attached. Loaded
    /// once at run start so the per-blob `download_bytes` skip-check is
    /// a `HashSet` hit instead of a SQLite round trip.
    pub async fn loaded_blob_ids(&self) -> Result<HashSet<String>> {
        let rows = sqlx::query("SELECT id FROM blob_refs WHERE blake3 IS NOT NULL")
            .fetch_all(&self.pool)
            .await
            .context("loaded_blob_ids")?;
        let mut out = HashSet::with_capacity(rows.len());
        for r in rows {
            if let Ok(id) = r.try_get::<String, _>("id") {
                out.insert(id);
            }
        }
        Ok(out)
    }

    pub async fn pre_seed_blob_stub(
        &self,
        ref_id: &str,
        kind: &str,
        owning_id: &str,
        slot: &str,
        content_type: Option<&str>,
        source_url: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin blob stub tx")?;
        blob_cas::pre_seed_ref(
            &mut tx,
            &RefStub {
                ref_id,
                kind,
                owning_id,
                slot,
                upstream_uuid: Some(ref_id),
                upstream_name: None,
                source_url,
                content_type,
            },
        )
        .await?;
        tx.commit().await.context("commit blob stub tx")?;
        Ok(())
    }

    pub async fn store_blob(&self, stub: &RefStub<'_>, bytes: &[u8]) -> Result<String> {
        blob_cas::store_bytes(&self.pool, &self.cas, stub, bytes).await
    }

    pub async fn record_blob_error(
        &self,
        ref_id: &str,
        owning_id: &str,
        slot: &str,
        err: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin blob error tx")?;
        blob_cas::record_ref_error(&mut tx, ref_id, owning_id, slot, err).await?;
        tx.commit().await.context("commit blob error tx")?;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Row-level upserts (shared by single + batch APIs)
// ─────────────────────────────────────────────────────────────────────

async fn upsert_mailbox_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
    payload: &Value,
) -> Result<()> {
    let id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("mailbox payload missing id"))?;
    let name = payload.get("name").and_then(|v| v.as_str());
    let parent_id = payload.get("parentId").and_then(|v| v.as_str());
    let role = payload.get("role").and_then(|v| v.as_str());
    let sort_order = payload.get("sortOrder").and_then(|v| v.as_i64());
    let total_emails = payload.get("totalEmails").and_then(|v| v.as_i64());
    let unread_emails = payload.get("unreadEmails").and_then(|v| v.as_i64());
    let payload_str = serde_json::to_string(payload).context("serialize mailbox")?;
    sqlx::query(
        "INSERT INTO mailboxes
            (id, account_id, name, parent_id, role, sort_order, total_emails,
             unread_emails, payload)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
         ON CONFLICT(id) DO UPDATE SET
            account_id = excluded.account_id,
            name = COALESCE(excluded.name, mailboxes.name),
            parent_id = COALESCE(excluded.parent_id, mailboxes.parent_id),
            role = COALESCE(excluded.role, mailboxes.role),
            sort_order = COALESCE(excluded.sort_order, mailboxes.sort_order),
            total_emails = COALESCE(excluded.total_emails, mailboxes.total_emails),
            unread_emails = COALESCE(excluded.unread_emails, mailboxes.unread_emails),
            payload = excluded.payload",
    )
    .bind(id)
    .bind(account_id)
    .bind(name)
    .bind(parent_id)
    .bind(role)
    .bind(sort_order)
    .bind(total_emails)
    .bind(unread_emails)
    .bind(&payload_str)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert mailbox {id}"))?;
    dr::record_object_attempt(tx, "mailboxes", id, None).await
}

async fn upsert_email_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    row: &EmailRow,
) -> Result<()> {
    let payload_str = serde_json::to_string(&row.payload).context("serialize email")?;
    sqlx::query(
        "INSERT INTO emails
            (id, account_id, thread_id, blob_id, message_id, received_at, sent_at,
             size, subject, from_json, has_attachment, payload)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
         ON CONFLICT(id) DO UPDATE SET
            account_id = excluded.account_id,
            thread_id = excluded.thread_id,
            blob_id = excluded.blob_id,
            message_id = COALESCE(excluded.message_id, emails.message_id),
            received_at = COALESCE(excluded.received_at, emails.received_at),
            sent_at = COALESCE(excluded.sent_at, emails.sent_at),
            size = COALESCE(excluded.size, emails.size),
            subject = COALESCE(excluded.subject, emails.subject),
            from_json = COALESCE(excluded.from_json, emails.from_json),
            has_attachment = COALESCE(excluded.has_attachment, emails.has_attachment),
            payload = excluded.payload",
    )
    .bind(&row.id)
    .bind(&row.account_id)
    .bind(&row.thread_id)
    .bind(&row.blob_id)
    .bind(row.message_id.as_deref())
    .bind(row.received_at.as_deref())
    .bind(row.sent_at.as_deref())
    .bind(row.size)
    .bind(row.subject.as_deref())
    .bind(row.from_json.as_deref())
    .bind(row.has_attachment as i64)
    .bind(&payload_str)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert email {}", row.id))?;
    dr::record_object_attempt(tx, "emails", &row.id, None).await?;

    // Refresh join tables for this email (delete-then-insert). Owning
    // table updates are the source of truth for labels/keywords —
    // anything we previously had for this id but is no longer present
    // upstream must disappear.
    sqlx::query("DELETE FROM email_mailboxes WHERE email_id = ?")
        .bind(&row.id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("clear email_mailboxes {}", row.id))?;
    for mid in &row.mailbox_ids {
        sqlx::query(
            "INSERT INTO email_mailboxes (email_id, mailbox_id) VALUES (?, ?)
             ON CONFLICT(email_id, mailbox_id) DO NOTHING",
        )
        .bind(&row.id)
        .bind(mid)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("insert email_mailbox {}={mid}", row.id))?;
    }

    sqlx::query("DELETE FROM email_keywords WHERE email_id = ?")
        .bind(&row.id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("clear email_keywords {}", row.id))?;
    for k in &row.keywords {
        sqlx::query(
            "INSERT INTO email_keywords (email_id, keyword) VALUES (?, ?)
             ON CONFLICT(email_id, keyword) DO NOTHING",
        )
        .bind(&row.id)
        .bind(k)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("insert email_keyword {}={k}", row.id))?;
    }

    sqlx::query("DELETE FROM email_attachments WHERE email_id = ?")
        .bind(&row.id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("clear email_attachments {}", row.id))?;
    for a in &row.attachments {
        sqlx::query(
            "INSERT INTO email_attachments
                (email_id, part_id, blob_id, name, type, size, disposition, cid)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(email_id, part_id) DO UPDATE SET
                blob_id = excluded.blob_id,
                name = excluded.name,
                type = excluded.type,
                size = excluded.size,
                disposition = excluded.disposition,
                cid = excluded.cid",
        )
        .bind(&row.id)
        .bind(&a.part_id)
        .bind(&a.blob_id)
        .bind(a.name.as_deref())
        .bind(a.content_type.as_deref())
        .bind(a.size)
        .bind(a.disposition.as_deref())
        .bind(a.cid.as_deref())
        .execute(&mut **tx)
        .await
        .with_context(|| format!("insert email_attachment {}/{}", row.id, a.part_id))?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Loaded shapes (consumed by translate)
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LoadedEmail {
    pub id: String,
    pub account_id: String,
    pub thread_id: String,
    pub blob_id: String,
    pub message_id: Option<String>,
    pub received_at: Option<String>,
    pub sent_at: Option<String>,
    pub size: Option<i64>,
    pub subject: Option<String>,
    pub has_attachment: bool,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct LoadedAttachment {
    pub part_id: String,
    pub blob_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<i64>,
    pub disposition: Option<String>,
    pub cid: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct EmailJoins {
    pub mailboxes: HashMap<String, Vec<String>>,
    pub keywords: HashMap<String, Vec<String>>,
    pub attachments: HashMap<String, Vec<LoadedAttachment>>,
}

/// Bag passed to translate's sync render path. `blobs` is a streaming
/// handle so peak RSS stays low even for accounts with multi-GB
/// attachment totals.
#[derive(Clone)]
pub struct LoadedRaw {
    pub accounts: Vec<Value>,
    pub mailboxes: Vec<Value>,
    pub threads: Vec<Value>,
    pub emails: Vec<LoadedEmail>,
    pub joins: EmailJoins,
    pub blobs: Arc<dyn BlobReader>,
}

impl Default for LoadedRaw {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            mailboxes: Vec::new(),
            threads: Vec::new(),
            emails: Vec::new(),
            joins: EmailJoins::default(),
            blobs: InMemoryBlobReader::empty_handle(),
        }
    }
}

/// Synchronous loader for translate / synthesize callers that already
/// sit under `#[tokio::main(flavor = "multi_thread")]`. Uses
/// `block_in_place` + the current Handle, so it must be invoked on a
/// multi-thread runtime.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            let blobs: Arc<dyn BlobReader> = Arc::new(SqliteBlobReader::new(
                db.pool().clone(),
                db.cas().pool().clone(),
            ));
            Ok::<_, anyhow::Error>(LoadedRaw {
                accounts: db.load_accounts().await?,
                mailboxes: db.load_mailboxes().await?,
                threads: db.load_threads().await?,
                emails: db.load_emails().await?,
                joins: db.load_email_joins().await?,
                blobs,
            })
        })
    })
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn tmp_db() -> (tempfile::TempDir, RawDb) {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("j.doltlite_db")).await.unwrap();
        (d, db)
    }

    #[tokio::test]
    async fn account_round_trips() {
        let (_d, db) = tmp_db().await;
        db.upsert_account(
            "A1",
            &json!({"name": "thad@fastmail.com", "isPersonal": true}),
        )
        .await
        .unwrap();
        let accts = db.load_accounts().await.unwrap();
        assert_eq!(accts.len(), 1);
        assert_eq!(accts[0]["name"], "thad@fastmail.com");
    }

    #[tokio::test]
    async fn mailbox_round_trips_and_filters_by_account() {
        let (_d, db) = tmp_db().await;
        db.upsert_mailbox(
            "A1",
            &json!({"id": "M1", "name": "Inbox", "role": "inbox", "totalEmails": 42}),
        )
        .await
        .unwrap();
        db.upsert_mailbox("A1", &json!({"id": "M2", "name": "Sent", "role": "sent"}))
            .await
            .unwrap();
        let mboxes = db.load_mailboxes().await.unwrap();
        assert_eq!(mboxes.len(), 2);
        // promoted columns
        let row: (String, i64) =
            sqlx::query_as("SELECT name, total_emails FROM mailboxes WHERE id = 'M1'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(row.0, "Inbox");
        assert_eq!(row.1, 42);
    }

    #[tokio::test]
    async fn email_round_trips_with_joins() {
        let (_d, db) = tmp_db().await;
        let payload = json!({
            "id": "E1",
            "blobId": "B-eml-1",
            "threadId": "T1",
            "messageId": ["<abc@example.com>"],
            "receivedAt": "2026-01-01T00:00:00Z",
            "sentAt": "2026-01-01T00:00:00Z",
            "size": 1234,
            "subject": "Hello",
            "from": [{"name": "Alice", "email": "a@x.test"}],
            "hasAttachment": true,
            "mailboxIds": {"M1": true, "M2": true},
            "keywords": {"$seen": true, "$flagged": true},
            "attachments": [
                {"partId": "2", "blobId": "B-att-1", "name": "doc.pdf",
                 "type": "application/pdf", "size": 999, "disposition": "attachment"}
            ],
        });
        let row = EmailRow::from_payload("A1", payload).expect("from_payload");
        db.upsert_email(&row).await.unwrap();

        let loaded = db.load_emails().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "E1");
        assert_eq!(loaded[0].thread_id, "T1");
        assert_eq!(loaded[0].blob_id, "B-eml-1");
        assert_eq!(loaded[0].subject.as_deref(), Some("Hello"));

        let joins = db.load_email_joins().await.unwrap();
        let mut mboxes = joins.mailboxes["E1"].clone();
        mboxes.sort();
        assert_eq!(mboxes, vec!["M1", "M2"]);
        let mut kws = joins.keywords["E1"].clone();
        kws.sort();
        assert_eq!(kws, vec!["$flagged", "$seen"]);
        let atts = &joins.attachments["E1"];
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].blob_id, "B-att-1");
        assert_eq!(atts[0].name.as_deref(), Some("doc.pdf"));
    }

    /// Re-upserting an email with a different set of keywords drops the
    /// old ones — the join table mirrors current upstream state, never
    /// accumulates.
    #[tokio::test]
    async fn email_join_refresh_drops_stale_entries() {
        let (_d, db) = tmp_db().await;
        let mut payload = json!({
            "id": "E1", "blobId": "B", "threadId": "T",
            "mailboxIds": {"M1": true},
            "keywords": {"$seen": true},
        });
        db.upsert_email(&EmailRow::from_payload("A", payload.clone()).unwrap())
            .await
            .unwrap();
        payload["keywords"] = json!({"$flagged": true});
        payload["mailboxIds"] = json!({"M2": true});
        db.upsert_email(&EmailRow::from_payload("A", payload).unwrap())
            .await
            .unwrap();
        let joins = db.load_email_joins().await.unwrap();
        assert_eq!(joins.mailboxes["E1"], vec!["M2"]);
        assert_eq!(joins.keywords["E1"], vec!["$flagged"]);
    }

    /// Hard-delete cascades: the email row, its joins, and its
    /// bookkeeping all disappear. Blobs are untouched.
    #[tokio::test]
    async fn delete_email_cascades_to_joins_and_bookkeeping() {
        let (_d, db) = tmp_db().await;
        let p = json!({
            "id": "E1", "blobId": "B-eml", "threadId": "T",
            "mailboxIds": {"M1": true},
            "keywords": {"$seen": true},
            "attachments": [{"partId": "1", "blobId": "B-att"}],
        });
        db.upsert_email(&EmailRow::from_payload("A", p).unwrap())
            .await
            .unwrap();
        // Also stash a blob row so we can prove it survives.
        db.store_blob(
            &RefStub {
                ref_id: "B-eml",
                kind: BLOB_KIND_EML,
                owning_id: "E1",
                slot: "source",
                upstream_uuid: Some("B-eml"),
                upstream_name: None,
                source_url: None,
                content_type: None,
            },
            b"raw",
        )
        .await
        .unwrap();

        db.delete_emails(&["E1".to_string()]).await.unwrap();

        assert!(db.load_emails().await.unwrap().is_empty());
        let joins = db.load_email_joins().await.unwrap();
        assert!(!joins.mailboxes.contains_key("E1"));
        assert!(!joins.keywords.contains_key("E1"));
        assert!(!joins.attachments.contains_key("E1"));
        let bk_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM emails_bookkeeping WHERE id = 'E1'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(bk_count, 0);
        // Blob untouched.
        assert!(db.blob_exists("B-eml").await.unwrap());
    }

    #[tokio::test]
    async fn payload_stored_as_jsonb_blob() {
        let (_d, db) = tmp_db().await;
        db.upsert_mailbox("A1", &json!({"id": "M1", "name": "Inbox"}))
            .await
            .unwrap();
        let t: String = sqlx::query_scalar("SELECT typeof(payload) FROM mailboxes WHERE id='M1'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(t, "blob", "payload should be JSONB-encoded BLOB");
    }

    #[tokio::test]
    async fn state_token_round_trips() {
        let (_d, db) = tmp_db().await;
        assert!(db.load_state("A1", "Email").await.unwrap().is_none());
        db.save_state("A1", "Email", "state-token-xyz")
            .await
            .unwrap();
        assert_eq!(
            db.load_state("A1", "Email").await.unwrap().as_deref(),
            Some("state-token-xyz"),
        );
        // Namespaced — other type/account don't leak.
        assert!(db.load_state("A1", "Mailbox").await.unwrap().is_none());
        assert!(db.load_state("A2", "Email").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reset_clears_data_joins_and_state_but_not_runs() {
        let (_d, db) = tmp_db().await;
        db.upsert_account("A1", &json!({"name": "x"}))
            .await
            .unwrap();
        db.upsert_mailbox("A1", &json!({"id": "M1", "name": "Inbox"}))
            .await
            .unwrap();
        db.upsert_email(
            &EmailRow::from_payload(
                "A1",
                json!({
                    "id": "E1", "blobId": "B", "threadId": "T",
                    "mailboxIds": {"M1": true},
                }),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        db.save_state("A1", "Email", "tok").await.unwrap();
        let run = frankweiler_etl::doltlite_raw::start_run(db.pool(), &json!({"phase": "test"}))
            .await
            .unwrap();

        db.reset().await.unwrap();

        assert!(db.load_accounts().await.unwrap().is_empty());
        assert!(db.load_mailboxes().await.unwrap().is_empty());
        assert!(db.load_emails().await.unwrap().is_empty());
        assert!(db.load_email_joins().await.unwrap().mailboxes.is_empty());
        assert!(db.load_state("A1", "Email").await.unwrap().is_none());
        // sync_runs untouched.
        let run_count: i64 = sqlx::query_scalar("SELECT count(*) FROM sync_runs WHERE run_id = ?")
            .bind(run)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(run_count, 1);
    }
}
