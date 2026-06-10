//! Doltlite-backed raw store for the CardDAV provider.
//!
//! Shared bookkeeping tables (`blob_refs`, `sync_runs`) plus the
//! open/blob plumbing live in
//! [`frankweiler_etl::doltlite_raw`]. The primary-key policy that
//! governs every object table here is documented in that module's
//! header — read it before adding new tables.
//!
//! Note that contacts doesn't populate `blob_refs` / the sibling CAS
//! file: vCard `PHOTO` bytes ride inline in the vCard payload column,
//! so there's no separate fetch + ref-by-id pattern.
//!
//! ## Tables
//!
//! - `accounts` — one row per configured CardDAV server. PK is the
//!   URL host (`contacts.icloud.com`, `carddav.fastmail.com`, …).
//!   We only ever expect one row per file in practice (the data
//!   root maps 1:1 with a `<name>.doltlite_db`), but model it as a
//!   table for symmetry with the other providers and so a future
//!   "merge two address-book backends into one file" path stays
//!   trivial.
//!
//! - `addressbooks` — PK is `"<account_id>!<href>"`. The CardDAV
//!   href (e.g. `/dav/addressbooks/user/default/`) is stable per
//!   server and known before the first detail fetch, satisfying
//!   the doltlite_raw PK guide. `sync_token` carries the
//!   `<sync-token>` value the server returned on the previous
//!   `sync-collection` REPORT; on next sync we hand it back and
//!   the server replies with deltas only. `ctag` is the cheaper
//!   "has anything changed" fallback when the server doesn't
//!   honor sync tokens.
//!
//! - `contacts` — PK is `"<addressbook_id>#<UID>"` where UID is the
//!   `UID:` field from the vCard. RFC 6350 mandates a non-empty
//!   UID; if a server ever emits one without, we fall back to a
//!   UUIDv5 derived from `(addressbook_id, href)` and log a warning.
//!   The two upstream-stable handles a CardDAV server gives us are
//!   `href` (server-assigned URL slot) and `etag` (opaque version
//!   string); we keep both alongside `payload` (the raw vCard
//!   bytes) and a couple of promoted columns (`display_name`,
//!   `revision`) for cheap predicate queries.
//!
//! The vCard payload is stored verbatim as JSON-wrapped text — the
//! `payload` column is `{"vcard": "<raw text>"}` rather than a
//! parsed object, because we want JSONB normalization at the dolt
//! layer to leave the bytes alone. Translate parses it lazily.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, DATA_TABLES};

pub use frankweiler_etl::doltlite_raw::db_path_for;

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
}

/// One row's worth of vCard input for [`RawDb::upsert_contact`].
///
/// `payload_vcard` is the raw vCard text as the server returned it —
/// stored byte-for-byte so re-fetches and translate stay
/// deterministic.
#[derive(Debug, Clone)]
pub struct ContactRow {
    pub addressbook_id: String,
    pub uid: String,
    pub href: String,
    pub etag: Option<String>,
    pub display_name: Option<String>,
    pub revision: Option<String>,
    pub payload_vcard: String,
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
    /// rather than a one-row delta against a stale cursor. See
    /// [`frankweiler_etl::doltlite_raw::truncate_data_tables`] for
    /// the canonical truncate path.
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await?;
        Ok(())
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
        let payload = serde_json::json!({
            "server_url": server_url,
            "principal_href": principal_href,
            "addressbook_home_set": addressbook_home_set,
        });
        let payload_str = serde_json::to_string(&payload).context("serialize account payload")?;
        let mut tx = self.pool.begin().await.context("begin account tx")?;
        sqlx::query(
            "INSERT INTO accounts
                (id, server_url, principal_href, addressbook_home_set, payload)
             VALUES (?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                server_url = COALESCE(excluded.server_url, accounts.server_url),
                principal_href = COALESCE(excluded.principal_href, accounts.principal_href),
                addressbook_home_set = COALESCE(excluded.addressbook_home_set, accounts.addressbook_home_set),
                payload = excluded.payload",
        )
        .bind(account_id)
        .bind(server_url)
        .bind(principal_href)
        .bind(addressbook_home_set)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .context("upsert account")?;
        dr::record_object_attempt(&mut tx, "accounts", account_id, None).await?;
        tx.commit().await.context("commit account tx")?;
        Ok(())
    }

    // ── addressbooks ────────────────────────────────────────────────

    /// PK derivation for an addressbook row. Exposed so callers can
    /// compute the same id from `(account_id, href)` without going
    /// through an upsert.
    pub fn addressbook_pk(account_id: &str, href: &str) -> String {
        format!("{account_id}!{href}")
    }

    /// Upsert addressbook metadata harvested from PROPFIND
    /// (display-name, description, ctag). `sync_token` is left alone
    /// here — it's bumped separately via [`Self::set_sync_token`]
    /// after a successful sync-collection REPORT, so a failed write
    /// pass doesn't advance the cursor.
    pub async fn upsert_addressbook(
        &self,
        account_id: &str,
        href: &str,
        display_name: Option<&str>,
        description: Option<&str>,
        ctag: Option<&str>,
    ) -> Result<()> {
        let id = Self::addressbook_pk(account_id, href);
        let payload = serde_json::json!({
            "account_id": account_id,
            "href": href,
            "display_name": display_name,
            "description": description,
            "ctag": ctag,
        });
        let payload_str =
            serde_json::to_string(&payload).context("serialize addressbook payload")?;
        let mut tx = self.pool.begin().await.context("begin addressbook tx")?;
        sqlx::query(
            "INSERT INTO addressbooks
                (id, account_id, href, display_name, description, ctag, payload)
             VALUES (?, ?, ?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                href = excluded.href,
                display_name = COALESCE(excluded.display_name, addressbooks.display_name),
                description = COALESCE(excluded.description, addressbooks.description),
                ctag = COALESCE(excluded.ctag, addressbooks.ctag),
                payload = excluded.payload",
        )
        .bind(&id)
        .bind(account_id)
        .bind(href)
        .bind(display_name)
        .bind(description)
        .bind(ctag)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .context("upsert addressbook")?;
        dr::record_object_attempt(&mut tx, "addressbooks", &id, None).await?;
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
    /// name. Used by [`super::fetch`] to drive its per-addressbook
    /// loop after discovery.
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
                    let matches = dn.as_ref().map(|d| names.contains(d)).unwrap_or(false);
                    if !matches {
                        return None;
                    }
                }
                Some((id, href, dn))
            })
            .collect())
    }

    // ── contacts ────────────────────────────────────────────────────

    /// PK derivation for a contact row. Same formula across providers
    /// using this crate so a row's id is reproducible from its
    /// `(addressbook_id, uid)` pair without consulting the DB.
    pub fn contact_pk(addressbook_id: &str, uid: &str) -> String {
        format!("{addressbook_id}#{uid}")
    }

    /// Upsert one vCard. Idempotent; an unchanged etag is still a
    /// valid call because callers may reuse this path during a
    /// non-incremental "rebuild from scratch" pass.
    pub async fn upsert_contact(&self, row: &ContactRow) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin contact tx")?;
        upsert_contact_in(&mut tx, row).await?;
        tx.commit().await.context("commit contact tx")?;
        Ok(())
    }

    /// Upsert a whole sync-collection page (or REPORT result set) in
    /// a single transaction. One `fsync` per page instead of per row
    /// — same pattern the slack/anthropic/gitlab extracts adopted in
    /// commit 1205aaf.
    pub async fn upsert_contacts(&self, rows: &[ContactRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin contacts batch tx")?;
        for row in rows {
            upsert_contact_in(&mut tx, row).await?;
        }
        tx.commit().await.context("commit contacts batch tx")?;
        Ok(())
    }

    /// Drop a contact + its sidecar row. Used when sync-collection
    /// reports `<status>HTTP/1.1 404 Not Found</status>` (or `410
    /// Gone`) for an href, meaning the contact was deleted upstream.
    ///
    /// Idempotent: deleting a row that doesn't exist is a no-op.
    pub async fn delete_contact(&self, addressbook_id: &str, href: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin delete contact tx")?;
        // Resolve href → id within the addressbook scope, then drop
        // both the data row and its bookkeeping sidecar.
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
    ) -> Result<std::collections::HashMap<String, String>> {
        let rows = sqlx::query(
            "SELECT href, etag FROM contacts WHERE addressbook_id = ? AND etag IS NOT NULL",
        )
        .bind(addressbook_id)
        .fetch_all(&self.pool)
        .await
        .context("select contact etags")?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
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

// ── private row-level upsert (shared by single + batch APIs) ───────

async fn upsert_contact_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    row: &ContactRow,
) -> Result<()> {
    let id = RawDb::contact_pk(&row.addressbook_id, &row.uid);
    // Wrap the raw vCard text in a JSON envelope so the column is
    // valid JSONB. Translate reads `vcard` out and parses it.
    let envelope = serde_json::json!({ "vcard": row.payload_vcard });
    let payload_str = serde_json::to_string(&envelope).context("serialize contact payload")?;
    sqlx::query(
        "INSERT INTO contacts
            (id, addressbook_id, uid, href, etag, display_name, revision, payload)
         VALUES (?, ?, ?, ?, ?, ?, ?, jsonb(?))
         ON CONFLICT(id) DO UPDATE SET
            addressbook_id = excluded.addressbook_id,
            uid = excluded.uid,
            href = excluded.href,
            etag = COALESCE(excluded.etag, contacts.etag),
            display_name = COALESCE(excluded.display_name, contacts.display_name),
            revision = COALESCE(excluded.revision, contacts.revision),
            payload = excluded.payload",
    )
    .bind(&id)
    .bind(&row.addressbook_id)
    .bind(&row.uid)
    .bind(&row.href)
    .bind(row.etag.as_deref())
    .bind(row.display_name.as_deref())
    .bind(row.revision.as_deref())
    .bind(&payload_str)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert contact {id}"))?;
    dr::record_object_attempt(&mut *tx, "contacts", &id, None).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_creates_data_and_bookkeeping_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.doltlite_db");
        let db = RawDb::open(&path).await.unwrap();
        // Sanity: every data table has a sidecar.
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
        let ab_id = RawDb::addressbook_pk("contacts.icloud.com", "/123/carddavhome/card/");
        let row = ContactRow {
            addressbook_id: ab_id.clone(),
            uid: "8a4d-7c1f".into(),
            href: "/123/carddavhome/card/8a4d-7c1f.vcf".into(),
            etag: Some("\"abc\"".into()),
            display_name: Some("Pat Q".into()),
            revision: Some("20260603T120000Z".into()),
            payload_vcard: "BEGIN:VCARD\nVERSION:3.0\nUID:8a4d-7c1f\nFN:Pat Q\nEND:VCARD\n".into(),
        };
        db.upsert_contact(&row).await.unwrap();

        // Data row landed at the expected PK.
        let id = RawDb::contact_pk(&ab_id, "8a4d-7c1f");
        let r = sqlx::query("SELECT display_name FROM contacts WHERE id = ?")
            .bind(&id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let dn: String = r.try_get("display_name").unwrap();
        assert_eq!(dn, "Pat Q");

        // Bookkeeping sidecar advanced (attempt_count = 1, fetched_at set).
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
        // Delete on an empty DB is a no-op.
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
        // Start a sync_runs row — should survive reset.
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
