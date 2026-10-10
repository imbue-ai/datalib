//! Open + non-DDL data-manipulation for the JMAP raw store.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{Row, Sqlite, Transaction};

use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, EmailKeywordRow, EmailMailboxRow, EmlBlobRow, LADDER};
pub use super::schema_raw::{EmailRow, BLOB_KIND_EML};

pub use datalib_etl::doltlite_raw::db_path_for;

// State-token namespacing

pub fn state_scope(account_id: &str, type_name: &str) -> String {
    format!("jmap:{account_id}:state:{type_name}")
}

// RawDb

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

impl RawDb {
    // ── state tokens ────────────────────────────────────────────────

    pub async fn load_state(&self, account_id: &str, type_name: &str) -> Result<Option<String>> {
        self.load_scope(&state_scope(account_id, type_name)).await
    }

    pub async fn save_state(&self, account_id: &str, type_name: &str, token: &str) -> Result<()> {
        self.save_scope(&state_scope(account_id, type_name), token)
            .await
    }

    /// Read a cursor under a caller-supplied scope key.
    pub async fn load_scope(&self, scope: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT last_seen_at_utc FROM sync_scope_state WHERE scope = ?")
            .bind(scope)
            .fetch_optional(self.pool())
            .await
            .context("select state token")?;
        row.map(|r| r.try_get::<String, _>("last_seen_at_utc"))
            .transpose()
            .context("sync_scope_state last_seen_at_utc")
    }

    pub async fn save_scope(&self, scope: &str, token: &str) -> Result<()> {
        dr::upsert_scope_state(self.pool(), scope, token).await
    }

    // ── loads (consumed by render) ───────────────────────────────

    pub async fn load_accounts(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "accounts").await
    }

    pub async fn load_mailboxes(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "mailboxes").await
    }

    pub async fn load_threads(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "threads").await
    }

    pub async fn load_emails(&self) -> Result<Vec<LoadedEmail>> {
        let rows = sqlx::query(
            "SELECT id, account_id, thread_id, blob_id, message_id, in_reply_to,
                    references_header, received_at, sent_at, size, subject,
                    from_json, to_json, cc_json, has_attachment
             FROM emails
             ORDER BY thread_id, received_at, id",
        )
        .fetch_all(self.pool())
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

    pub async fn load_email_joins(&self) -> Result<EmailJoins> {
        let mut mailboxes: HashMap<String, Vec<String>> = HashMap::new();
        for r in sqlx::query("SELECT email_id, mailbox_id FROM email_mailboxes")
            .fetch_all(self.pool())
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
            .fetch_all(self.pool())
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

    /// This account's mailbox rows: id → name.
    pub async fn mailbox_names(&self, account_id: &str) -> Result<BTreeMap<String, String>> {
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id, name FROM mailboxes WHERE account_id = ?")
                .bind(account_id)
                .fetch_all(self.pool())
                .await
                .context("select the account's mailboxes")?;
        Ok(rows
            .into_iter()
            .map(|(id, name)| (id, name.unwrap_or_default()))
            .collect())
    }

    /// Up to `limit` emails filed under `mailbox_id` with an id after
    /// `after`, in id order: each one's id, account and payload.
    pub async fn emails_filed_under(
        &self,
        mailbox_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, Value)>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT e.id, e.account_id, json(e.payload) FROM email_mailboxes m
             JOIN emails e ON e.id = m.email_id
             WHERE m.mailbox_id = ? AND e.id > ?
             ORDER BY e.id
             LIMIT ?",
        )
        .bind(mailbox_id)
        .bind(after)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await
        .with_context(|| format!("select the emails filed under {mailbox_id}"))?;
        rows.into_iter()
            .map(|(id, account, payload)| {
                let v = serde_json::from_str(&payload)
                    .with_context(|| format!("parse the payload of email {id}"))?;
                Ok((id, account, v))
            })
            .collect()
    }

    /// Delete this account's emails an authoritative enumeration did not
    /// name, and the threads left with none. Only for a walk that listed
    /// every message the account has. Returns how many emails went.
    pub async fn prune_emails_to(
        &self,
        account_id: &str,
        seen: &BTreeSet<String>,
    ) -> Result<usize> {
        let held: Vec<String> = sqlx::query_scalar("SELECT id FROM emails WHERE account_id = ?")
            .bind(account_id)
            .fetch_all(self.pool())
            .await
            .context("list the account's emails")?;
        let gone: Vec<String> = held
            .iter()
            .filter(|id| !seen.contains(id.as_str()))
            .cloned()
            .collect();
        self.delete_emails(&gone).await?;
        let mut tx = self.pool().begin().await.context("begin thread prune tx")?;
        for sql in [
            "DELETE FROM threads_bookkeeping WHERE id IN (SELECT id FROM threads \
             WHERE account_id = ? AND id NOT IN (SELECT thread_id FROM emails))",
            "DELETE FROM threads WHERE account_id = ? AND id NOT IN (SELECT thread_id FROM emails)",
        ] {
            sqlx::query(sql)
                .bind(account_id)
                .execute(&mut *tx)
                .await
                .context("delete threads with no emails")?;
        }
        tx.commit().await.context("commit thread prune tx")?;
        datalib_etl::prune::record("emails", held.len(), gone.len());
        Ok(gone.len())
    }

    // ── hard-deletes (JMAP destroy + parent-id cascades) ────────────

    pub async fn delete_mailboxes(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool()
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

    pub async fn delete_emails(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool()
            .begin()
            .await
            .context("begin delete emails tx")?;
        delete_emails_in_tx(&mut tx, ids).await?;
        tx.commit().await.context("commit delete emails tx")?;
        Ok(())
    }

    // ── blob skip-check ────────────────────────────────────────────

    /// `(blob_id, blake3)` for every `.eml` already resolved to CAS bytes.
    /// Pre-loaded once at the top of `sync_blobs`, so the per-blob "do we have
    /// this?" decision is a `HashMap` hit rather than a SQLite round trip.
    pub async fn loaded_blob_ids(&self) -> Result<HashMap<String, String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT blob_id, blake3 FROM email_blobs WHERE blake3 IS NOT NULL",
        )
        .fetch_all(self.pool())
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
}

/// Delete these email rows with their joins, `.eml` edges and sidecars.
pub async fn delete_emails_in_tx(tx: &mut Transaction<'_, Sqlite>, ids: &[String]) -> Result<()> {
    for id in ids {
        // The `.eml`'s fetch problem goes with it: an email upstream no
        // longer has cannot fail to download.
        sqlx::query(
            "DELETE FROM problems WHERE scope_kind = ? AND scope_key IN \
             (SELECT 'email_blobs:' || id FROM email_blobs WHERE email_id = ?)",
        )
        .bind(datalib_problems::ScopeKind::Entity.as_str())
        .bind(id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("forget the problems of email {id}"))?;
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
                .execute(&mut **tx)
                .await
                .with_context(|| format!("delete email {id}"))?;
        }
    }
    Ok(())
}

/// Write `.eml` edges. One that holds bytes is upserted; one without (a
/// body skipped or failed) is added only where no row holds bytes for
/// it already, since a failed read is not news that the bytes changed.
pub async fn write_eml_edges_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[EmlBlobRow],
) -> Result<()> {
    let holding: Vec<EmlBlobRow> = rows
        .iter()
        .filter(|r| r.blake3.is_some())
        .cloned()
        .collect();
    bulk_upsert_entity_in_tx(tx, &holding).await?;
    for row in rows.iter().filter(|r| r.blake3.is_none()) {
        sqlx::query(
            "INSERT INTO email_blobs (id, email_id, blob_id, blake3) VALUES (?, ?, ?, NULL)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(&row.id)
        .bind(&row.email_id)
        .bind(&row.blob_id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("write the empty .eml edge {}", row.id))?;
    }
    Ok(())
}

// Email join-table refresh

/// Refresh the two email-side join tables (`email_mailboxes`,
/// `email_keywords`) for one email. Delete-then-insert because the
/// join tables mirror current upstream state — anything we
/// previously had for this id that's no longer present must
/// disappear.
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

// Loaded shapes (consumed by render)

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
/// `datalib_etl_email_render::render::parse::parse(..., last_render_hash)` instead.
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

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::schema_raw::{AccountRow, EmailRow, MailboxRow};
    use datalib_etl::bulk::bulk_upsert_in_tx;
    use datalib_time::IsoOffsetTimestamp;
    use serde_json::json;

    async fn tmp_db() -> (tempfile::TempDir, RawDb) {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("j.doltlite_db")).await.unwrap();
        (d, db)
    }

    fn now() -> IsoOffsetTimestamp {
        IsoOffsetTimestamp::now_local()
    }

    async fn bulk<T: datalib_etl::bulk::BulkUpsertable>(db: &RawDb, rows: &[T]) {
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
                &json!({"id": "M1", "name": "Inbox", "role": "inbox"}),
            )
            .unwrap(),
            MailboxRow::from_jmap_payload(
                "A1",
                &json!({"id": "M2", "name": "Sent", "role": "sent"}),
            )
            .unwrap(),
            MailboxRow::from_jmap_payload("A2", &json!({"id": "M3", "name": "Inbox"})).unwrap(),
        ];
        bulk(&db, &rows).await;
        assert_eq!(db.load_mailboxes().await.unwrap().len(), 3);
        let a1 = db.mailbox_names("A1").await.unwrap();
        assert_eq!(
            a1.into_iter().collect::<Vec<_>>(),
            vec![
                ("M1".to_string(), "Inbox".to_string()),
                ("M2".to_string(), "Sent".to_string())
            ]
        );
    }

    /// A store an older build wrote, with the mailbox counts as columns
    /// and in the content payload, opens on the first rung: the columns
    /// are gone, the payload keeps everything but the counts, and the
    /// rest of the store is untouched.
    #[tokio::test]
    async fn the_first_rung_takes_the_counts_out_of_the_mailbox_row() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("j.doltlite_db");
        {
            let ddl = full_ddl();
            let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
            let pool = dr::open(&path, &ddl).await.unwrap();
            for sql in [
                "ALTER TABLE mailboxes ADD COLUMN total_emails INTEGER NULL",
                "ALTER TABLE mailboxes ADD COLUMN unread_emails INTEGER NULL",
                "INSERT INTO mailboxes (id, payload, account_id, name, role, total_emails, unread_emails)
                 VALUES ('M1', jsonb('{\"id\":\"M1\",\"name\":\"Inbox\",\"totalEmails\":42,\"unreadEmails\":3,\"totalThreads\":40}'),
                         'A1', 'Inbox', 'inbox', 42, 3)",
            ] {
                sqlx::query(sql).execute(&pool).await.unwrap();
            }
            dr::commit_run(&pool, "an older build's rows")
                .await
                .unwrap();
            pool.close().await;
        }

        let db = RawDb::open(&path).await.expect("the rung carries it");
        let payload: String = sqlx::query_scalar("SELECT json(payload) FROM mailboxes")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&payload).unwrap(),
            json!({"id": "M1", "name": "Inbox"})
        );
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('mailboxes')")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert!(!columns.iter().any(|c| c.contains("emails")), "{columns:?}");
        let version: String =
            sqlx::query_scalar("SELECT value FROM _datalib_meta WHERE key = 'schema_version'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(version, super::LADDER.len().to_string());
        db.close().await;
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

    /// Hard-delete cascades: the email row, its joins and its bookkeeping all
    /// go. CAS bytes are untouched, verified by stashing a `cas_objects` row
    /// directly, deleting the owning email, and checking the bytes survive.
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
        // it survives.
        let stashed = db.cas().put(b"raw", Some("message/rfc822")).await.unwrap();

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
                .bind(&stashed)
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
}
