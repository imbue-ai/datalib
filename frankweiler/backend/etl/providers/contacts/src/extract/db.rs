//! Doltlite-backed raw store for the CardDAV provider.
//!
//! Shared bookkeeping tables (`sync_runs`) plus the open / read
//! plumbing live in [`frankweiler_etl::doltlite_raw`]. Tables, row
//! types, and PK recipes live next door in [`super::schema_raw`];
//! this file is the manipulation layer — open, reset, the
//! sync-token cursor, deletes, and the etag-map probe.
//!
//! Contacts doesn't open a sibling CAS file: vCard `PHOTO` bytes
//! ride inline (base64) in the payload column rather than being
//! lifted out, so there's no `*_attachments` edge table either.
//! See `super::schema_raw` for the per-table docstrings.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

pub use super::schema_raw::{addressbook_pk, contact_pk, AccountRow, AddressbookRow, ContactRow};
use super::schema_raw::{full_ddl, DATA_TABLES};

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Wipe every per-row table so the next fetch re-downloads every
    /// contact from the server. Also clears any persisted sync
    /// tokens / ctags so the server gives us a full enumeration
    /// rather than a one-row delta against a stale cursor.
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    // ── accounts ────────────────────────────────────────────────────

    /// Upsert the account row. `principal_href` and
    /// `addressbook_home_set` are filled in from PROPFIND results;
    /// either can be NULL on the first insert and back-filled later.
    pub async fn upsert_account(
        &self,
        account_id: &str,
        server_url: &str,
        principal_href: Option<&str>,
        addressbook_home_set: Option<&str>,
    ) -> Result<()> {
        let row = AccountRow {
            id: account_id.to_string(),
            server_url: Some(server_url.to_string()),
            principal_href: principal_href.map(String::from),
            addressbook_home_set: addressbook_home_set.map(String::from),
        };
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin account tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit account tx")?;
        Ok(())
    }

    // ── addressbooks ────────────────────────────────────────────────

    /// Upsert addressbook metadata harvested from PROPFIND. The
    /// `sync_token` column is bumped separately via
    /// [`Self::set_sync_token`] after a successful sync-collection
    /// REPORT, so a failed write pass doesn't advance the cursor.
    pub async fn upsert_addressbook(
        &self,
        account_id: &str,
        href: &str,
        display_name: Option<&str>,
        description: Option<&str>,
        ctag: Option<&str>,
    ) -> Result<()> {
        let row = AddressbookRow {
            id: addressbook_pk(account_id, href),
            account_id: account_id.to_string(),
            href: href.to_string(),
            display_name: display_name.map(String::from),
            description: description.map(String::from),
            ctag: ctag.map(String::from),
        };
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin addressbook tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit addressbook tx")?;
        Ok(())
    }

    /// Read the sync-token we persisted from the last
    /// `sync-collection` REPORT against this addressbook. `None`
    /// means we've never completed a sync-collection cycle, so the
    /// next request should send an empty token to enumerate
    /// everything.
    pub async fn sync_token(&self, addressbook_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT sync_token FROM addressbooks WHERE id = ?")
            .bind(addressbook_id)
            .fetch_optional(&self.pool)
            .await
            .context("select sync_token")?;
        Ok(row.and_then(|r| r.try_get::<Option<String>, _>("sync_token").ok().flatten()))
    }

    /// Persist the sync-token returned from the most recent
    /// `sync-collection` REPORT. Called only after every contact in
    /// the response has been upserted, so an interrupted batch
    /// doesn't poison the cursor.
    pub async fn set_sync_token(&self, addressbook_id: &str, token: &str) -> Result<()> {
        sqlx::query("UPDATE addressbooks SET sync_token = ? WHERE id = ?")
            .bind(token)
            .bind(addressbook_id)
            .execute(&self.pool)
            .await
            .context("update sync_token")?;
        Ok(())
    }

    /// `(id, href, display_name)` for every addressbook we've ever
    /// recorded under this account, optionally filtered by display
    /// name.
    pub async fn addressbooks_for_fetch(
        &self,
        account_id: &str,
        only_named: Option<&[String]>,
    ) -> Result<Vec<(String, String, Option<String>)>> {
        let rows = sqlx::query(
            "SELECT id, href, display_name FROM addressbooks
             WHERE account_id = ? ORDER BY id",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .context("select addressbooks_for_fetch")?;
        let want_names =
            only_named.map(|names| names.iter().collect::<std::collections::HashSet<_>>());
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let id: String = r.try_get("id").ok()?;
                let href: String = r.try_get("href").ok()?;
                let dn: Option<String> = r.try_get("display_name").ok();
                if let Some(names) = &want_names {
                    if !dn.as_ref().is_some_and(|d| names.contains(d)) {
                        return None;
                    }
                }
                Some((id, href, dn))
            })
            .collect())
    }

    // ── contacts ────────────────────────────────────────────────────

    /// Upsert one vCard. Idempotent.
    pub async fn upsert_contact(&self, row: &ContactRow) -> Result<()> {
        self.upsert_contacts(std::slice::from_ref(row)).await
    }

    /// Upsert a whole sync-collection page (or REPORT result set) in
    /// a single transaction. One `fsync` per page instead of per row.
    pub async fn upsert_contacts(&self, rows: &[ContactRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin contacts batch tx")?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit contacts batch tx")?;
        Ok(())
    }

    /// Drop a contact + its sidecar row. Used when sync-collection
    /// reports `<status>HTTP/1.1 404 Not Found</status>` (or `410
    /// Gone`) for an href, meaning the contact was deleted upstream.
    /// Idempotent.
    pub async fn delete_contact(&self, addressbook_id: &str, href: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin delete contact tx")?;
        let row =
            sqlx::query("SELECT id FROM contacts WHERE addressbook_id = ? AND href = ? LIMIT 1")
                .bind(addressbook_id)
                .bind(href)
                .fetch_optional(&mut *tx)
                .await
                .context("select contact id for delete")?;
        if let Some(row) = row {
            let id: String = row.try_get("id").context("read contact id")?;
            sqlx::query("DELETE FROM contacts WHERE id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await
                .context("delete contact")?;
            sqlx::query("DELETE FROM contacts_bookkeeping WHERE id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await
                .context("delete contact bookkeeping")?;
        }
        tx.commit().await.context("commit delete contact tx")?;
        Ok(())
    }

    /// `{href -> etag}` for every contact we already have in the
    /// addressbook. Used by the etag-walk fallback to decide which
    /// hrefs are stale and need re-fetching.
    pub async fn contact_etags_by_href(
        &self,
        addressbook_id: &str,
    ) -> Result<HashMap<String, String>> {
        let rows = sqlx::query(
            "SELECT href, etag FROM contacts WHERE addressbook_id = ? AND etag IS NOT NULL",
        )
        .bind(addressbook_id)
        .fetch_all(&self.pool)
        .await
        .context("select contact etags")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let href: String = r.try_get("href").unwrap_or_default();
            let etag: String = r.try_get("etag").unwrap_or_default();
            if !href.is_empty() && !etag.is_empty() {
                out.insert(href, etag);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_creates_data_and_bookkeeping_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.doltlite_db");
        let db = RawDb::open(&path).await.unwrap();
        for t in DATA_TABLES {
            let bk = format!("{t}_bookkeeping");
            let row = sqlx::query(&format!(
                "SELECT name FROM sqlite_master WHERE type='table' AND name = '{bk}'"
            ))
            .fetch_optional(db.pool())
            .await
            .unwrap();
            assert!(row.is_some(), "expected sidecar {bk} after open");
        }
    }

    #[tokio::test]
    async fn upsert_contact_round_trips_with_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.doltlite_db");
        let db = RawDb::open(&path).await.unwrap();
        db.upsert_account(
            "contacts.icloud.com",
            "https://contacts.icloud.com/",
            None,
            None,
        )
        .await
        .unwrap();
        db.upsert_addressbook(
            "contacts.icloud.com",
            "/123/carddavhome/card/",
            Some("Home"),
            None,
            Some("ctag-1"),
        )
        .await
        .unwrap();
        let ab_id = addressbook_pk("contacts.icloud.com", "/123/carddavhome/card/");
        let row = ContactRow::new(
            ab_id.clone(),
            "8a4d-7c1f".into(),
            "/123/carddavhome/card/8a4d-7c1f.vcf".into(),
            Some("\"abc\"".into()),
            Some("Pat Q".into()),
            Some("20260603T120000Z".into()),
            "BEGIN:VCARD\nVERSION:3.0\nUID:8a4d-7c1f\nFN:Pat Q\nEND:VCARD\n",
        );
        db.upsert_contact(&row).await.unwrap();

        let id = contact_pk(&ab_id, "8a4d-7c1f");
        let r = sqlx::query("SELECT display_name FROM contacts WHERE id = ?")
            .bind(&id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let dn: String = r.try_get("display_name").unwrap();
        assert_eq!(dn, "Pat Q");

        let r = sqlx::query(
            "SELECT attempt_count, fetched_at, last_error FROM contacts_bookkeeping WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let n: i64 = r.try_get("attempt_count").unwrap();
        let fa: Option<String> = r.try_get("fetched_at").unwrap_or(None);
        let le: Option<String> = r.try_get("last_error").unwrap_or(None);
        assert_eq!(n, 1, "attempt_count");
        assert!(fa.is_some(), "fetched_at = {fa:?}");
        assert!(le.is_none(), "last_error = {le:?}");
    }

    #[tokio::test]
    async fn delete_contact_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.doltlite_db");
        let db = RawDb::open(&path).await.unwrap();
        db.delete_contact("ab1", "/cards/X.vcf").await.unwrap();
    }

    #[tokio::test]
    async fn reset_truncates_data_tables_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.doltlite_db");
        let db = RawDb::open(&path).await.unwrap();
        db.upsert_account("h", "https://h/", None, None)
            .await
            .unwrap();
        let _ = frankweiler_etl::doltlite_raw::start_run(db.pool(), &serde_json::json!({"k": "v"}))
            .await
            .unwrap();
        db.reset().await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 0);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_runs")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 1, "sync_runs preserved on reset");
    }
}
