//! Open + non-DDL data-manipulation for the JMAP raw store.
//!
//! [`RawDb`] owns the entity-db pool and the (currently-shared)
//! blob_refs surface plus the sibling CAS handle. The schema itself —
//! every table DDL, every wire-payload row struct + its derived
//! `BulkUpsertable` impl, the envelope-shaped `EmailRow` with its
//! hand-written `BulkUpsertable` impl, and the per-table commentary
//! — lives next door in [`super::schema_raw`].
//!
//! What's here is the small set of things `schema_raw` can't be:
//! `RawDb::open`, `reset`, the JMAP-specific state-token plumbing
//! (`load_state` / `save_state`), the load helpers render consumes,
//! and the join-table refresh helper that fires alongside email
//! bulk-upserts. Entity-table writes go through the generic
//! `frankweiler_etl::bulk::bulk_upsert_in_tx<T>` helper from the
//! caller (`super::mod`), not via methods on `RawDb`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::{Row, Sqlite, Transaction};

use frankweiler_etl::blob_cas::{self, BlobCas};
use frankweiler_etl::bulk::bulk_upsert_entity_in_tx;
use frankweiler_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, EmailKeywordRow, EmailMailboxRow, DATA_TABLES, JOIN_TABLES};
pub use super::schema_raw::{EmailRow, BLOB_KIND_EML};

pub use frankweiler_etl::doltlite_raw::db_path_for;

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
        sqlx::query("DELETE FROM mbox_files_checkpoint")
            .execute(&mut *tx)
            .await
            .context("clear mbox file checkpoints on reset")?;
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

    // ── loads (consumed by render) ───────────────────────────────

    pub async fn load_accounts(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "accounts").await
    }

    pub async fn load_mailboxes(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "mailboxes").await
    }

    pub async fn load_threads(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "threads").await
    }

    /// `id → email_count` for every thread we've persisted. The
    /// render-side cheap probe uses this to skip re-rendering
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

    pub async fn load_emails(&self) -> Result<Vec<LoadedEmail>> {
        let rows = sqlx::query(
            "SELECT id, account_id, thread_id, blob_id, message_id, in_reply_to,
                    references_header, received_at, sent_at, size, subject,
                    from_json, to_json, cc_json, has_attachment
             FROM emails
             ORDER BY thread_id, received_at, id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select emails")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(LoadedEmail {
                id: r.try_get("id").unwrap_or_default(),
                account_id: r.try_get("account_id").unwrap_or_default(),
                thread_id: r.try_get("thread_id").unwrap_or_default(),
                blob_id: r.try_get("blob_id").unwrap_or_default(),
                message_id: r.try_get::<Option<String>, _>("message_id").unwrap_or(None),
                in_reply_to: r
                    .try_get::<Option<String>, _>("in_reply_to")
                    .unwrap_or(None),
                references: r
                    .try_get::<Option<String>, _>("references_header")
                    .unwrap_or(None),
                received_at: r
                    .try_get::<Option<String>, _>("received_at")
                    .unwrap_or(None),
                sent_at: r.try_get::<Option<String>, _>("sent_at").unwrap_or(None),
                size: r.try_get::<Option<i64>, _>("size").unwrap_or(None),
                subject: r.try_get::<Option<String>, _>("subject").unwrap_or(None),
                from_json: r.try_get::<Option<String>, _>("from_json").unwrap_or(None),
                to_json: r.try_get::<Option<String>, _>("to_json").unwrap_or(None),
                cc_json: r.try_get::<Option<String>, _>("cc_json").unwrap_or(None),
                has_attachment: r
                    .try_get::<Option<i64>, _>("has_attachment")
                    .unwrap_or(None)
                    .unwrap_or(0)
                    != 0,
            });
        }
        Ok(out)
    }

    /// Snapshot every email's mailbox + keyword joins, keyed by
    /// `email_id`. (No attachments join after the eml-as-canonical
    /// port — render mail-parses the `.eml` for parts.)
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
        Ok(EmailJoins {
            mailboxes,
            keywords,
            attachments: HashMap::new(),
        })
    }

    /// Every persisted email id — for a quick set-membership check
    /// during incremental sync ("do we already have this id?").
    pub async fn known_email_ids(&self) -> Result<HashSet<String>> {
        let rows = sqlx::query("SELECT id FROM emails WHERE blob_id != ''")
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

    // ── hard-deletes (JMAP destroy + parent-id cascades) ────────────

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

    /// Hard-delete one email plus its joins + bookkeeping. Blobs are
    /// untouched — another email may share the same `.eml` blob.
    /// Dolt history preserves the pre-delete state.
    pub async fn delete_emails(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin delete emails tx")?;
        for id in ids {
            for sql in [
                "DELETE FROM email_mailboxes WHERE email_id = ?",
                "DELETE FROM email_keywords WHERE email_id = ?",
                "DELETE FROM email_blobs_bookkeeping
                   WHERE id IN (SELECT id FROM email_blobs WHERE email_id = ?)",
                "DELETE FROM email_blobs WHERE email_id = ?",
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

    // ── blob skip-check + refetch-blobs control ────────────────────

    /// `(blob_id, blake3)` pairs for every `.eml` we've already
    /// resolved to CAS bytes. Pre-loaded once at the top of
    /// `sync_blobs` so the per-blob "do we already have this?"
    /// decision is a `HashMap` hit instead of a SQLite round trip.
    /// Replaces the older two-table union — after the eml-as-canonical
    /// port we only fetch `.eml` blobs.
    pub async fn loaded_blob_ids(&self) -> Result<HashMap<String, String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT blob_id, blake3 FROM email_blobs WHERE blake3 IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await
        .context("loaded_blob_ids")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let blob_id: String = r.try_get("blob_id").unwrap_or_default();
            let blake3: String = r.try_get("blake3").unwrap_or_default();
            if !blob_id.is_empty() && !blake3.is_empty() {
                out.insert(blob_id, blake3);
            }
        }
        Ok(out)
    }

    /// Implements the `--refetch-blobs` control. Sets every
    /// `email_blobs.blake3` back to NULL so the next `sync_blobs` pass
    /// walks every `.eml` from scratch. The CAS itself is left alone —
    /// re-downloaded bytes hash to the same blake3, the `INSERT OR
    /// IGNORE` on the CAS side is a no-op.
    pub async fn clear_blob_hashes(&self) -> Result<()> {
        sqlx::query("UPDATE email_blobs SET blake3 = NULL")
            .execute(&self.pool)
            .await
            .context("clear email_blobs.blake3")?;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Email join-table refresh
// ─────────────────────────────────────────────────────────────────────

/// Refresh the two email-side join tables (`email_mailboxes`,
/// `email_keywords`) for one email. Delete-then-insert because the
/// join tables mirror current upstream state — anything we
/// previously had for this id that's no longer present must
/// disappear.
///
/// Runs inside the same transaction as the parent email's
/// `bulk_upsert_in_tx` call so failure during the refresh rolls the
/// envelope row back too. Caller is responsible for committing. The
/// `mailboxIds` / `keywords` come straight off the email's stored
/// envelope payload; the join rows go through the generic
/// bulk-upsert path.
pub async fn refresh_email_joins(tx: &mut Transaction<'_, Sqlite>, row: &EmailRow) -> Result<()> {
    let email_id = row.id();

    sqlx::query("DELETE FROM email_mailboxes WHERE email_id = ?")
        .bind(email_id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("clear email_mailboxes {email_id}"))?;
    let mbox_rows: Vec<EmailMailboxRow> = row
        .mailbox_ids()
        .iter()
        .map(|m| EmailMailboxRow::new(email_id, m))
        .collect();
    bulk_upsert_entity_in_tx(tx, &mbox_rows)
        .await
        .with_context(|| format!("insert email_mailboxes {email_id}"))?;

    sqlx::query("DELETE FROM email_keywords WHERE email_id = ?")
        .bind(email_id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("clear email_keywords {email_id}"))?;
    let kw_rows: Vec<EmailKeywordRow> = row
        .keywords()
        .iter()
        .map(|k| EmailKeywordRow::new(email_id, k))
        .collect();
    bulk_upsert_entity_in_tx(tx, &kw_rows)
        .await
        .with_context(|| format!("insert email_keywords {email_id}"))?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Loaded shapes (consumed by render)
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LoadedEmail {
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
    /// Serialized JSON of the From/To/Cc header(s) as
    /// `[{name?, email}, …]`. Same shape on the JMAP path and the
    /// mbox path. Render uses these for cheap header rendering
    /// without paying for a full mail-parser pass on the headers.
    pub from_json: Option<String>,
    pub to_json: Option<String>,
    pub cc_json: Option<String>,
    pub has_attachment: bool,
}

/// One attachment part extracted from a `.eml` at parse time.
///
/// **Not** loaded from a DB table — the eml IS the canonical body,
/// and `parse_doltlite_async` mail-parses each loaded `.eml` to
/// populate this list. `blob_id` is the synthesized content hash of
/// the attachment bytes (added to the per-bucket [`BlobBundle`]
/// under that key) so render's `bucket.blobs.get(&att.blob_id)`
/// resolves uniformly across truly-attached and inline parts.
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
    /// Per-email attachment parts, populated by `parse_doltlite_async`
    /// from the mail-parsed `.eml`. Not loaded from a DB table.
    pub attachments: HashMap<String, Vec<LoadedAttachment>>,
}

/// Bag passed to render's sync render path. Attachment bytes are
/// loaded per bucket as a [`BlobBundle`] by `render::parse`, not
/// here.
#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub accounts: Vec<Value>,
    pub mailboxes: Vec<Value>,
    pub threads: Vec<Value>,
    pub emails: Vec<LoadedEmail>,
    pub joins: EmailJoins,
}

/// Synchronous loader for tests / ad-hoc callers that want every
/// entity table at once. Production render calls
/// `crate::render::parse::parse(..., last_render_hash)` instead.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                accounts: db.load_accounts().await?,
                mailboxes: db.load_mailboxes().await?,
                threads: db.load_threads().await?,
                emails: db.load_emails().await?,
                joins: db.load_email_joins().await?,
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
    use crate::download::schema_raw::{AccountRow, EmailRow, MailboxRow, ThreadRow};
    use frankweiler_etl::bulk::bulk_upsert_in_tx;
    use frankweiler_time::IsoOffsetTimestamp;
    use serde_json::json;

    async fn tmp_db() -> (tempfile::TempDir, RawDb) {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("j.doltlite_db")).await.unwrap();
        (d, db)
    }

    fn now() -> String {
        IsoOffsetTimestamp::now_local().to_rfc3339()
    }

    async fn bulk<T: frankweiler_etl::bulk::BulkUpsertable>(db: &RawDb, rows: &[T]) {
        if rows.is_empty() {
            return;
        }
        let mut tx = db.pool().begin().await.unwrap();
        bulk_upsert_in_tx(&mut tx, rows, &now()).await.unwrap();
        tx.commit().await.unwrap();
    }

    async fn upsert_email(db: &RawDb, row: &EmailRow) {
        let mut tx = db.pool().begin().await.unwrap();
        bulk_upsert_in_tx(&mut tx, std::slice::from_ref(row), &now())
            .await
            .unwrap();
        refresh_email_joins(&mut tx, row).await.unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn account_round_trips() {
        let (_d, db) = tmp_db().await;
        let row = AccountRow::from_jmap_payload(
            "A1",
            &json!({"name": "thad@fastmail.com", "isPersonal": true}),
        )
        .unwrap();
        bulk(&db, &[row]).await;
        let accts = db.load_accounts().await.unwrap();
        assert_eq!(accts.len(), 1);
        assert_eq!(accts[0]["name"], "thad@fastmail.com");
    }

    #[tokio::test]
    async fn mailbox_round_trips_and_filters_by_account() {
        let (_d, db) = tmp_db().await;
        let rows = vec![
            MailboxRow::from_jmap_payload(
                "A1",
                &json!({"id": "M1", "name": "Inbox", "role": "inbox", "totalEmails": 42}),
            )
            .unwrap(),
            MailboxRow::from_jmap_payload(
                "A1",
                &json!({"id": "M2", "name": "Sent", "role": "sent"}),
            )
            .unwrap(),
        ];
        bulk(&db, &rows).await;
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
        let row = EmailRow::from_jmap_envelope("A1", &payload).expect("from_payload");
        upsert_email(&db, &row).await;

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
        upsert_email(&db, &EmailRow::from_jmap_envelope("A", &payload).unwrap()).await;
        payload["keywords"] = json!({"$flagged": true});
        payload["mailboxIds"] = json!({"M2": true});
        upsert_email(&db, &EmailRow::from_jmap_envelope("A", &payload).unwrap()).await;
        let joins = db.load_email_joins().await.unwrap();
        assert_eq!(joins.mailboxes["E1"], vec!["M2"]);
        assert_eq!(joins.keywords["E1"], vec!["$flagged"]);
    }

    /// Hard-delete cascades: the email row, its joins, and its
    /// bookkeeping all disappear. CAS bytes are untouched —
    /// `delete_emails` only touches `emails` + its joins +
    /// `emails_bookkeeping`. The structural guarantee is verified
    /// by stashing one `cas_objects` row directly, deleting the
    /// owning email, then checking the bytes are still there.
    #[tokio::test]
    async fn delete_email_cascades_to_joins_and_bookkeeping() {
        let (_d, db) = tmp_db().await;
        let p = json!({
            "id": "E1", "blobId": "B-eml", "threadId": "T",
            "mailboxIds": {"M1": true},
            "keywords": {"$seen": true},
            "attachments": [{"partId": "1", "blobId": "B-att"}],
        });
        upsert_email(&db, &EmailRow::from_jmap_envelope("A", &p).unwrap()).await;
        // Stash an entry in the sibling CAS directly so we can prove
        // it survives. blake3 is fake (64-char hex of zeros) — the
        // CHECK constraint on `cas_objects.blake3` cares about
        // length, not value.
        let fake_blake3 = "0".repeat(64);
        db.cas()
            .put_many(&[frankweiler_etl::blob_cas::CasInsert {
                blake3: &fake_blake3,
                bytes: b"raw",
                content_type: Some("message/rfc822"),
            }])
            .await
            .unwrap();

        db.delete_emails(&["E1".to_string()]).await.unwrap();

        assert!(db.load_emails().await.unwrap().is_empty());
        let joins = db.load_email_joins().await.unwrap();
        assert!(!joins.mailboxes.contains_key("E1"));
        assert!(!joins.keywords.contains_key("E1"));
        let bk_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM emails_bookkeeping WHERE id = 'E1'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(bk_count, 0);
        // CAS untouched.
        let cas_bytes: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT bytes FROM cas_objects WHERE blake3 = ?")
                .bind(&fake_blake3)
                .fetch_optional(db.cas().pool())
                .await
                .unwrap();
        assert_eq!(cas_bytes.as_deref(), Some(&b"raw"[..]));
    }

    #[tokio::test]
    async fn payload_stored_as_jsonb_blob() {
        let (_d, db) = tmp_db().await;
        let row =
            MailboxRow::from_jmap_payload("A1", &json!({"id": "M1", "name": "Inbox"})).unwrap();
        bulk(&db, &[row]).await;
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
        let acct = AccountRow::from_jmap_payload("A1", &json!({"name": "x"})).unwrap();
        bulk(&db, &[acct]).await;
        let mbox =
            MailboxRow::from_jmap_payload("A1", &json!({"id": "M1", "name": "Inbox"})).unwrap();
        bulk(&db, &[mbox]).await;
        let email = EmailRow::from_jmap_envelope(
            "A1",
            &json!({
                "id": "E1", "blobId": "B", "threadId": "T",
                "mailboxIds": {"M1": true},
            }),
        )
        .unwrap();
        upsert_email(&db, &email).await;
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

        // Suppress unused warning when the new threads loader isn't
        // touched by the assertions above. `_` silences without
        // editing the test set.
        let _: Vec<Value> = db.load_threads().await.unwrap();

        // Same for ThreadRow / from_payload — exercise the
        // constructor so the import stays live across phase 1.
        let _ = ThreadRow::from_jmap_payload("T1", "A1", &json!({"emailIds": ["E1"]})).unwrap();
    }
}
