//! Doltlite-backed raw store for the CardDAV provider.

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_macros::RawStoreHandle;
use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::{Row, Sqlite, Transaction};

use datalib_etl::bulk::{bulk_upsert_entity_in_tx, bulk_upsert_in_tx};
use datalib_etl::doltlite_raw::{self as dr};

pub use datalib_etl::doltlite_raw::db_path_for;

pub use super::schema_raw::{addressbook_pk, contact_pk, AccountRow, AddressbookRow, ContactRow};
use super::schema_raw::{full_ddl, ContactCategoryRow, GroupMemberRow, LADDER};

#[derive(Clone, Debug, RawStoreHandle)]
pub struct RawDb {
    pool: SqlitePool,
    /// The commit a reader's connection reads, or `None` for the download
    /// step reading back what it just wrote. Set once, at open.
    pin: Option<datalib_etl::pin::Pin>,
}

/// One contact row served to the render pass. The vCard text is
/// pulled out of the `{"vcard": …}` payload envelope on the SQL side
/// so the render path doesn't have to know about the envelope.
#[derive(Debug, Clone)]
pub struct LoadedRawContact {
    /// The `contacts` row, and the `addressbooks` row its label came
    /// from — what every card in it declares it read.
    pub id: String,
    pub addressbook_id: Option<String>,
    pub uid: String,
    pub href: String,
    pub addressbook_label: String,
    pub vcard: String,
}

impl RawDb {
    /// Open this store to *read* it, for the render pass.
    ///
    /// The download step owns this store; render only reads it. An ordinary
    /// [`Self::open`] would discard a dirty working set, reconcile the schema
    /// and commit on the way in — three writes to a file this caller does not
    /// own, and with producers committing incrementally, a way to throw away
    /// the downloader's batch in flight. See
    /// `datalib_etl::doltlite_raw::open_reader`.
    ///
    /// No DDL, so a store the current downloader has not touched keeps
    /// whatever columns it has; probe with `column_exists` and fall back
    /// where that matters.
    /// **`None` means the store cannot be read**, not that it holds no
    /// contacts — no commit to pin, or a build without the dolt
    /// extensions. The caller sweeps every document this pass did not
    /// name, so handing back an empty read here would delete every
    /// contact the source has. See the plan's "The sink contract".
    ///
    /// Pinned at `commit` — the one the render driver diffed against —
    /// or at HEAD when there is none.
    pub async fn open_reader(db_path: &Path, commit: Option<&str>) -> Result<Option<Self>> {
        // Pinned at open, views installed: a reader cannot read the
        // working set by forgetting to.
        let Some(reader) = datalib_etl::doltlite_raw::open_reader(db_path, commit).await? else {
            return Ok(None);
        };
        let pin = reader.pin().clone();
        let pool = reader.pool().clone();
        Ok(Some(Self {
            pool,
            pin: Some(pin),
        }))
    }

    /// The commit a reader is pinned at; `None` for the writer.
    pub fn pin(&self) -> Option<&datalib_etl::pin::Pin> {
        self.pin.as_ref()
    }

    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open_migrating(db_path, &slices, LADDER).await?;
        Ok(Self { pool, pin: None })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Release every store this handle opened, and wait for the
    /// connections to go away. Dropping only schedules that.
    pub async fn close(self) {
        self.close_all().await;
    }

    // ── accounts ────────────────────────────────────────────────────

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
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = self.pool.begin().await.context("begin account tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit account tx")?;
        Ok(())
    }

    // ── addressbooks ────────────────────────────────────────────────

    /// The `sync_token` column is the listing's, kept by
    /// [`Self::set_sync_token`] in the transaction that stores what the
    /// token covers, so the upsert leaves it alone.
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
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = self.pool.begin().await.context("begin addressbook tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit addressbook tx")?;
        Ok(())
    }

    /// The token the last `sync-collection` page of this address book
    /// left; `None` lists it whole.
    pub async fn sync_token(&self, addressbook_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT sync_token FROM addressbooks WHERE id = ?")
            .bind(addressbook_id)
            .fetch_optional(&self.pool)
            .await
            .context("select sync_token")?;
        Ok(row.and_then(|r| r.try_get::<Option<String>, _>("sync_token").ok().flatten()))
    }

    pub async fn set_sync_token(
        tx: &mut Transaction<'_, Sqlite>,
        addressbook_id: &str,
        token: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE addressbooks SET sync_token = ? WHERE id = ?")
            .bind(token)
            .bind(addressbook_id)
            .execute(&mut **tx)
            .await
            .context("update sync_token")?;
        Ok(())
    }

    // ── contacts ────────────────────────────────────────────────────

    /// Upsert one vCard. Idempotent.
    pub async fn upsert_contact(&self, row: &ContactRow) -> Result<()> {
        self.upsert_contacts(std::slice::from_ref(row)).await
    }

    /// Upsert a whole page of cards in a single transaction. One `fsync`
    /// per page instead of per row.
    pub async fn upsert_contacts(&self, rows: &[ContactRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin contacts batch tx")?;
        Self::upsert_contacts_in_tx(&mut tx, &rows.iter().collect::<Vec<_>>()).await?;
        tx.commit().await.context("commit contacts batch tx")?;
        Ok(())
    }

    pub async fn upsert_contacts_in_tx(
        tx: &mut Transaction<'_, Sqlite>,
        rows: &[&ContactRow],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let owned: Vec<ContactRow> = rows.iter().map(|r| (*r).clone()).collect();
        bulk_upsert_in_tx(tx, &owned, &now).await?;
        // Delete-then-insert: the members and categories are whatever the
        // card names now.
        let mut members: Vec<GroupMemberRow> = Vec::new();
        let mut categories: Vec<ContactCategoryRow> = Vec::new();
        for row in rows {
            let id = &row.id_and_payload.id;
            for sql in [
                "DELETE FROM contact_group_members WHERE group_id = ?",
                "DELETE FROM contact_categories WHERE contact_id = ?",
            ] {
                sqlx::query(sql)
                    .bind(id)
                    .execute(&mut **tx)
                    .await
                    .context("clear what a card derives")?;
            }
            members.extend(GroupMemberRow::for_contact(row));
            categories.extend(ContactCategoryRow::for_contact(row));
        }
        bulk_upsert_entity_in_tx(tx, &members)
            .await
            .context("insert group members")?;
        bulk_upsert_entity_in_tx(tx, &categories)
            .await
            .context("insert categories")?;
        Ok(())
    }

    /// Drop the contact at `href` with its sidecar row and what the card
    /// derives: what `sync-collection` reports gone, or a whole listing
    /// never named. Returns how many went. Idempotent.
    pub async fn delete_contact(
        tx: &mut Transaction<'_, Sqlite>,
        addressbook_id: &str,
        href: &str,
    ) -> Result<u64> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM contacts WHERE addressbook_id = ? AND href = ?")
                .bind(addressbook_id)
                .bind(href)
                .fetch_all(&mut **tx)
                .await
                .context("select contact id for delete")?;
        for id in &ids {
            for sql in [
                "DELETE FROM contacts WHERE id = ?",
                "DELETE FROM contacts_bookkeeping WHERE id = ?",
                "DELETE FROM contact_group_members WHERE group_id = ?",
                "DELETE FROM contact_categories WHERE contact_id = ?",
            ] {
                sqlx::query(sql)
                    .bind(id)
                    .execute(&mut **tx)
                    .await
                    .context("delete contact")?;
            }
        }
        Ok(ids.len() as u64)
    }

    /// Drop the contacts of one address book whose uid is in `uids`, with
    /// their sidecar rows: what a re-read `.vcf` file no longer carries.
    /// Idempotent.
    pub async fn delete_contacts_by_uid(
        &self,
        addressbook_id: &str,
        uids: &[String],
    ) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin delete contacts tx")?;
        for uid in uids {
            let id = contact_pk(addressbook_id, uid);
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
            for sql in [
                "DELETE FROM contact_group_members WHERE group_id = ?",
                "DELETE FROM contact_categories WHERE contact_id = ?",
            ] {
                sqlx::query(sql)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await
                    .context("delete what a card derives")?;
            }
        }
        tx.commit().await.context("commit delete contacts tx")?;
        Ok(())
    }

    /// Drop the address book a `.vcf` file was, with its contacts and
    /// their sidecar rows, and forget the file's cursor entry — in
    /// one transaction, so a crash leaves the file stamped and the next run
    /// retries. Returns how many contacts went.
    pub async fn delete_file_addressbook(
        &self,
        addressbook_id: &str,
        checkpoint_scope: &str,
        rel: &str,
    ) -> Result<usize> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin delete addressbook tx")?;
        let contacts = Self::delete_addressbook(&mut tx, addressbook_id).await?;
        datalib_etl_files::file_checkpoint::forget_file(&mut tx, checkpoint_scope, rel).await?;
        tx.commit().await.context("commit delete addressbook tx")?;
        Ok(contacts)
    }

    /// The address books of `account_id` the server no longer lists,
    /// with everything stored for them: a home listing is one PROPFIND,
    /// whole by nature, so absence from it is deletion. Returns how many
    /// contacts went.
    pub async fn delete_addressbooks_not_in(
        &self,
        account_id: &str,
        listed: &[String],
    ) -> Result<usize> {
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT id FROM addressbooks WHERE account_id = ?")
                .bind(account_id)
                .fetch_all(&self.pool)
                .await
                .context("select addressbooks")?;
        let gone: Vec<&String> = stored.iter().filter(|id| !listed.contains(id)).collect();
        if gone.is_empty() {
            return Ok(0);
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin delete addressbooks tx")?;
        let mut contacts = 0;
        for id in gone {
            contacts += Self::delete_addressbook(&mut tx, id).await?;
            datalib_etl_web::dav::state::forget_collection(&mut tx, id).await?;
        }
        tx.commit().await.context("commit delete addressbooks tx")?;
        Ok(contacts)
    }

    /// The address book, its contacts, their sidecars and what the cards
    /// derive. Returns how many contacts went.
    async fn delete_addressbook(
        tx: &mut Transaction<'_, Sqlite>,
        addressbook_id: &str,
    ) -> Result<usize> {
        for sql in [
            "DELETE FROM contact_group_members WHERE addressbook_id = ?",
            "DELETE FROM contact_categories WHERE addressbook_id = ?",
            "DELETE FROM contacts_bookkeeping WHERE id IN \
             (SELECT id FROM contacts WHERE addressbook_id = ?)",
        ] {
            sqlx::query(sql)
                .bind(addressbook_id)
                .execute(&mut **tx)
                .await
                .context("delete the addressbook's contact edges")?;
        }
        let contacts = sqlx::query("DELETE FROM contacts WHERE addressbook_id = ?")
            .bind(addressbook_id)
            .execute(&mut **tx)
            .await
            .context("delete the addressbook's contacts")?
            .rows_affected();
        for sql in [
            "DELETE FROM addressbooks WHERE id = ?",
            "DELETE FROM addressbooks_bookkeeping WHERE id = ?",
        ] {
            sqlx::query(sql)
                .bind(addressbook_id)
                .execute(&mut **tx)
                .await
                .context("delete addressbook")?;
        }
        Ok(contacts as usize)
    }

    /// Snapshot every contact row for the render pass, joined
    /// against `addressbooks` so the caller gets the display-name
    /// label without a second query. Same shape regardless of
    /// whether the row landed via CardDAV sync-collection or
    /// [`super::vcf_dir::fetch`].
    pub async fn load_all_for_render_and_index_md(&self) -> Result<Vec<LoadedRawContact>> {
        let rows = sqlx::query(
            "SELECT c.id AS id,
                    c.addressbook_id AS addressbook_id,
                    c.uid AS uid,
                    c.href AS href,
                    json_extract(c.payload, '$.vcard') AS vcard,
                    COALESCE(a.display_name, a.href) AS addressbook_label
             FROM contacts c
             LEFT JOIN addressbooks a ON a.id = c.addressbook_id
             ORDER BY c.addressbook_id, c.id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select contacts for render")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let vcard: Option<String> = r.try_get("vcard").ok();
            let Some(vcard) = vcard else { continue };
            out.push(LoadedRawContact {
                id: r.try_get("id").unwrap_or_default(),
                addressbook_id: r
                    .try_get::<Option<String>, _>("addressbook_id")
                    .ok()
                    .flatten(),
                uid: r.try_get("uid").unwrap_or_default(),
                href: r.try_get("href").unwrap_or_default(),
                addressbook_label: r
                    .try_get::<Option<String>, _>("addressbook_label")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "default".to_string()),
                vcard,
            });
        }
        Ok(out)
    }

    /// Every `uid` the addressbook holds: how the `.vcf` path tells a
    /// new contact from an update, since a file carries no etag.
    pub async fn contact_uids(&self, addressbook_id: &str) -> Result<HashSet<String>> {
        let rows = sqlx::query("SELECT uid FROM contacts WHERE addressbook_id = ?")
            .bind(addressbook_id)
            .fetch_all(&self.pool)
            .await
            .context("select contact uids")?;
        let mut out = HashSet::with_capacity(rows.len());
        for r in rows {
            let uid: String = r.try_get("uid").unwrap_or_default();
            if !uid.is_empty() {
                out.insert(uid);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::schema_raw::DATA_TABLES;

    #[tokio::test]
    async fn open_creates_data_and_bookkeeping_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.doltlite_db");
        let db = RawDb::open(&path).await.unwrap();
        for t in DATA_TABLES {
            let bk = format!("{t}_bookkeeping");
            // Test: `bk` derives from the `DATA_TABLES` const.
            let row = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT name FROM sqlite_master WHERE type='table' AND name = '{bk}'"
            )))
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
            "SELECT attempt_count, fetched_at_utc, last_error FROM contacts_bookkeeping WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let n: i64 = r.try_get("attempt_count").unwrap();
        let fa: Option<String> = r.try_get("fetched_at_utc").unwrap_or(None);
        let le: Option<String> = r.try_get("last_error").unwrap_or(None);
        assert_eq!(n, 1, "attempt_count");
        assert!(fa.is_some(), "fetched_at_utc = {fa:?}");
        assert!(le.is_none(), "last_error = {le:?}");
    }

    async fn delete(db: &RawDb, book: &str, href: &str) -> u64 {
        let mut tx = db.pool().begin().await.unwrap();
        let n = RawDb::delete_contact(&mut tx, book, href).await.unwrap();
        tx.commit().await.unwrap();
        n
    }

    #[tokio::test]
    async fn delete_contact_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.doltlite_db");
        let db = RawDb::open(&path).await.unwrap();
        assert_eq!(delete(&db, "ab1", "/cards/X.vcf").await, 0);
    }

    fn group_row(members: &[&str]) -> ContactRow {
        let lines: String = members
            .iter()
            .map(|m| format!("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:{m}\n"))
            .collect();
        ContactRow::new(
            "ab".into(),
            "bridge".into(),
            "/cards/bridge.vcf".into(),
            None,
            Some("Bridge".into()),
            None,
            &format!(
                "BEGIN:VCARD\nVERSION:3.0\nUID:bridge\nFN:Bridge\n\
                 X-ADDRESSBOOKSERVER-KIND:group\n{lines}END:VCARD\n"
            ),
        )
    }

    async fn members(db: &RawDb) -> Vec<(String, Option<String>)> {
        sqlx::query_as(
            "SELECT group_id, member_id FROM contact_group_members ORDER BY group_id, member_id",
        )
        .fetch_all(db.pool())
        .await
        .unwrap()
    }

    /// The membership table follows the group card: a member dropped from
    /// the card is a row gone, and a deleted group takes its rows with it.
    #[tokio::test]
    async fn group_members_follow_the_group_card() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("contacts.doltlite_db"))
            .await
            .unwrap();
        db.upsert_contact(&group_row(&["tng-picard", "tng-riker"]))
            .await
            .unwrap();
        assert_eq!(
            members(&db).await,
            vec![
                ("ab#bridge".to_string(), Some("ab#tng-picard".to_string())),
                ("ab#bridge".to_string(), Some("ab#tng-riker".to_string())),
            ]
        );

        db.upsert_contact(&group_row(&["tng-picard"]))
            .await
            .unwrap();
        assert_eq!(
            members(&db).await,
            vec![("ab#bridge".to_string(), Some("ab#tng-picard".to_string()))]
        );

        assert_eq!(delete(&db, "ab", "/cards/bridge.vcf").await, 1);
        assert!(members(&db).await.is_empty());
        db.close().await;
    }

    async fn categories(db: &RawDb) -> Vec<String> {
        sqlx::query_scalar("SELECT category FROM contact_categories ORDER BY category")
            .fetch_all(db.pool())
            .await
            .unwrap()
    }

    fn labelled(categories: &str) -> ContactRow {
        ContactRow::new(
            "ab".into(),
            "tng-picard".into(),
            "/cards/picard.vcf".into(),
            None,
            Some("Picard".into()),
            None,
            &format!("BEGIN:VCARD\nVERSION:3.0\nUID:tng-picard\nFN:Picard\nCATEGORIES:{categories}\nEND:VCARD\n"),
        )
    }

    /// The categories table follows the card: a label taken off is a row
    /// gone, and a deleted card takes its rows with it.
    #[tokio::test]
    async fn categories_follow_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("contacts.doltlite_db"))
            .await
            .unwrap();
        db.upsert_contact(&labelled("myContacts,starred,Bridge"))
            .await
            .unwrap();
        assert_eq!(
            categories(&db).await,
            vec!["Bridge", "myContacts", "starred"]
        );

        db.upsert_contact(&labelled("myContacts")).await.unwrap();
        assert_eq!(categories(&db).await, vec!["myContacts"]);

        assert_eq!(delete(&db, "ab", "/cards/picard.vcf").await, 1);
        assert!(categories(&db).await.is_empty());
        db.close().await;
    }

    /// A store an older build wrote has cards, neither derived table, and
    /// the retired `contact_photos`; its first open under this build fills
    /// both from the cards already there, because an address book's
    /// sync-token says nothing changed and the download would never fill
    /// it, and drops `contact_photos`.
    #[tokio::test]
    async fn the_rungs_fill_the_derived_tables_from_the_cards_already_stored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.doltlite_db");
        {
            let ddl: Vec<String> = full_ddl()
                .into_iter()
                .filter(|d| {
                    !d.contains("contact_group_members") && !d.contains("contact_categories")
                })
                .chain([
                    "CREATE TABLE contact_photos (id TEXT PRIMARY KEY, owner_id TEXT NOT NULL, \
                         source_url TEXT NOT NULL, blake3 TEXT NULL)"
                        .to_string(),
                ])
                .collect();
            let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
            let pool = dr::open(&path, &ddl).await.unwrap();
            let now = datalib_time::IsoOffsetTimestamp::now_local();
            let mut tx = pool.begin().await.unwrap();
            bulk_upsert_in_tx(
                &mut tx,
                &[group_row(&["tng-picard"]), labelled("myContacts,Away Team")],
                &now,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            dr::commit_run(&pool, "an older build's rows")
                .await
                .unwrap();
            pool.close().await;
        }

        let db = RawDb::open(&path).await.expect("the rungs carry it");
        assert_eq!(
            members(&db).await,
            vec![("ab#bridge".to_string(), Some("ab#tng-picard".to_string()))]
        );
        assert_eq!(categories(&db).await, vec!["Away Team", "myContacts"]);
        let photo_tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'contact_photos'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(photo_tables, 0, "contact_photos survived the open");
        db.close().await;
    }
}
