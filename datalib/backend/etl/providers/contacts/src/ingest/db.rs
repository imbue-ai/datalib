//! Doltlite-backed raw store for the CardDAV provider.

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_macros::RawStoreHandle;
use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::{Row, Sqlite, Transaction};

use datalib_etl::blob_cas::{cas_path_for, BlobCas, CasInsert};
use datalib_etl::bulk::{bulk_upsert_entity_in_tx, bulk_upsert_in_tx};
use datalib_etl::doltlite_raw::{self as dr};

pub use datalib_etl::doltlite_raw::db_path_for;

use super::photo::{self, CardPhoto};
pub use super::schema_raw::{addressbook_pk, contact_pk, AccountRow, AddressbookRow, ContactRow};
use datalib_sql::{sql, sql_id, Sql};

/// Photos per blob-store transaction when copying the cards a store
/// already holds: a few MB at a time, rather than every photo at once.
const PHOTOS_PER_PUT: usize = 100;
use super::schema_raw::{full_ddl, ContactCategoryRow, GroupMemberRow, LADDER};

#[derive(Clone, Debug, RawStoreHandle)]
pub struct RawDb {
    pool: SqlitePool,
    /// Where the download keeps each card's inline photo, so a page can
    /// link it there (`photo`). `Some` on the download's handle; `None`
    /// on a reader whose store has no blob store yet, since opening a
    /// missing file read-only is an error and creating it would be a
    /// write render does not own.
    cas: Option<BlobCas>,
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
    /// **`None` means the store cannot be read**, not that it holds no
    /// contacts — no commit to pin, or a build without the dolt
    /// extensions. The caller sweeps every document this pass did not
    /// name, so handing back an empty read here would delete every
    /// contact the source has.
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
        let cas = BlobCas::open_for_render(db_path).await?;
        Ok(Some(Self {
            pool,
            cas,
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
        let cas = match BlobCas::open(&cas_path_for(db_path)).await {
            Ok(cas) => cas,
            Err(e) => {
                pool.close().await;
                return Err(e);
            }
        };
        Ok(Self {
            pool,
            cas: Some(cas),
            pin: None,
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// `None` only on a reader whose store has no blob store — see the
    /// field.
    pub fn cas(&self) -> Option<&BlobCas> {
        self.cas.as_ref()
    }

    /// Release every store this handle opened, and wait for the
    /// connections to go away. Dropping only schedules that.
    pub async fn close(self) {
        self.close_all().await;
    }

    // ── accounts ────────────────────────────────────────────────────

    pub async fn upsert_account(&self, row: &AccountRow) -> Result<()> {
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = self.pool.begin().await.context("begin account tx")?;
        bulk_upsert_in_tx(&mut tx, std::slice::from_ref(row), &now).await?;
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
        let row = sql!(
            "contacts.read_addressbook_sync_token",
            "SELECT sync_token FROM addressbooks WHERE id = ?"
        )
        .query()
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
        sql!(
            "contacts.update_addressbook_sync_token",
            "UPDATE addressbooks SET sync_token = ? WHERE id = ?"
        )
        .query()
        .bind(token)
        .bind(addressbook_id)
        .execute(&mut **tx)
        .await
        .context("update sync_token")?;
        Ok(())
    }

    // ── photos ──────────────────────────────────────────────────────

    /// Keep the inline photo of every card in `rows` in the blob store.
    /// It commits before the cards do, so a stored card's photo is
    /// always there to link.
    pub async fn put_photos(&self, rows: &[&ContactRow]) -> Result<()> {
        let photos: Vec<CardPhoto> = rows
            .iter()
            .flat_map(|r| photo::inline_photos(&r.vcard()))
            .collect();
        self.put_card_photos(&photos).await
    }

    /// The photos of every card the store holds, for a store an older
    /// build filled before the download kept them. `--prepare` runs it on
    /// a store an older release wrote; a photo already there is not
    /// written again.
    /// Returns how many photos the cards carry.
    pub async fn put_photos_of_held_cards(&self) -> Result<usize> {
        let mut rows = sql!(
            "contacts.list_stored_vcards",
            "SELECT json_extract(payload, '$.vcard') FROM contacts ORDER BY id"
        )
        .query_scalar::<Option<String>>()
        .fetch(&self.pool);
        let (mut batch, mut total) = (Vec::new(), 0);
        while let Some(vcard) = futures::TryStreamExt::try_next(&mut rows)
            .await
            .context("read the stored cards")?
        {
            batch.extend(photo::inline_photos(&vcard.unwrap_or_default()));
            if batch.len() >= PHOTOS_PER_PUT {
                total += batch.len();
                self.put_card_photos(&std::mem::take(&mut batch)).await?;
            }
        }
        total += batch.len();
        self.put_card_photos(&batch).await?;
        Ok(total)
    }

    async fn put_card_photos(&self, photos: &[CardPhoto]) -> Result<()> {
        if photos.is_empty() {
            return Ok(());
        }
        let cas = self
            .cas
            .as_ref()
            .context("a reader's handle cannot keep photos")?;
        cas.put_many(
            photos
                .iter()
                .enumerate()
                .map(|(id, p)| CasInsert {
                    id,
                    bytes: &p.bytes,
                    content_type: Some(&p.content_type),
                })
                .collect(),
        )
        .await
        .context("keep the cards' photos")?;
        Ok(())
    }

    // ── contacts ────────────────────────────────────────────────────

    /// Upsert one vCard. Idempotent.
    pub async fn upsert_contact(&self, row: &ContactRow) -> Result<()> {
        self.upsert_contacts(std::slice::from_ref(row)).await
    }

    /// Upsert a whole page of cards in a single transaction, after their
    /// photos. One `fsync` per page instead of per row.
    pub async fn upsert_contacts(&self, rows: &[ContactRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let rows: Vec<&ContactRow> = rows.iter().collect();
        self.put_photos(&rows).await?;
        let mut tx = self.pool.begin().await.context("begin contacts batch tx")?;
        Self::upsert_contacts_in_tx(&mut tx, &rows).await?;
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
                sql!(
                    "contacts.upsert_contacts_in_tx.delete_contact_group_members",
                    "DELETE FROM contact_group_members WHERE group_id = ?"
                ),
                sql!(
                    "contacts.upsert_contacts_in_tx.delete_contact_categories",
                    "DELETE FROM contact_categories WHERE contact_id = ?"
                ),
            ] {
                sql.query()
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
        let ids: Vec<String> = sql!(
            "contacts.find_contact_ids_by_href",
            "SELECT id FROM contacts WHERE addressbook_id = ? AND href = ?"
        )
        .query_scalar()
        .bind(addressbook_id)
        .bind(href)
        .fetch_all(&mut **tx)
        .await
        .context("select contact id for delete")?;
        for id in &ids {
            for sql in [
                sql!(
                    "contacts.delete_contact.delete_contacts",
                    "DELETE FROM contacts WHERE id = ?"
                ),
                sql!(
                    "contacts.delete_contact.delete_contacts_bookkeeping",
                    "DELETE FROM contacts_bookkeeping WHERE id = ?"
                ),
                sql!(
                    "contacts.delete_contact.delete_contact_group_members",
                    "DELETE FROM contact_group_members WHERE group_id = ?"
                ),
                sql!(
                    "contacts.delete_contact.delete_contact_categories",
                    "DELETE FROM contact_categories WHERE contact_id = ?"
                ),
            ] {
                sql.query()
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
            sql!(
                "contacts.delete_contact_by_uid",
                "DELETE FROM contacts WHERE id = ?"
            )
            .query()
            .bind(&id)
            .execute(&mut *tx)
            .await
            .context("delete contact")?;
            sql!(
                "contacts.delete_contact_bookkeeping_by_uid",
                "DELETE FROM contacts_bookkeeping WHERE id = ?"
            )
            .query()
            .bind(&id)
            .execute(&mut *tx)
            .await
            .context("delete contact bookkeeping")?;
            for sql in [
                sql!(
                    "contacts.delete_contacts_by_uid.delete_contact_group_members",
                    "DELETE FROM contact_group_members WHERE group_id = ?"
                ),
                sql!(
                    "contacts.delete_contacts_by_uid.delete_contact_categories",
                    "DELETE FROM contact_categories WHERE contact_id = ?"
                ),
            ] {
                sql.query()
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
    /// their sidecar rows, and delete the file's `ingested_files` row —
    /// in one transaction, so a crash leaves that row in place and the
    /// next run retries. Returns how many contacts went.
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
        datalib_etl_files::file_checkpoint::delete_file_stamp(&mut tx, checkpoint_scope, rel)
            .await?;
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
        let stored: Vec<String> = sql!(
            "contacts.list_account_addressbook_ids",
            "SELECT id FROM addressbooks WHERE account_id = ?"
        )
        .query_scalar()
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
            datalib_etl_web::dav::state::delete_collection(&mut tx, id).await?;
        }
        tx.commit().await.context("commit delete addressbooks tx")?;
        Ok(contacts)
    }

    /// The address book, its contacts of either shape, their sidecars and
    /// what the cards derive. Returns how many contacts went.
    async fn delete_addressbook(
        tx: &mut Transaction<'_, Sqlite>,
        addressbook_id: &str,
    ) -> Result<usize> {
        for sql in [
            ADDRESSBOOK_GROUP_MEMBERS,
            ADDRESSBOOK_CATEGORIES,
            "DELETE FROM contacts_bookkeeping WHERE id IN \
             (SELECT id FROM contacts WHERE addressbook_id = ?)",
        ] {
            Sql::dynamic(sql_id!("contacts.delete_addressbook_derived_rows"), sql)
                .query()
                .bind(addressbook_id)
                .execute(&mut **tx)
                .await
                .context("delete the addressbook's contact edges")?;
        }
        let contacts = sql!(
            "contacts.delete_addressbook_contacts",
            "DELETE FROM contacts WHERE addressbook_id = ?"
        )
        .query()
        .bind(addressbook_id)
        .execute(&mut **tx)
        .await
        .context("delete the addressbook's contacts")?
        .rows_affected();
        let people = Self::delete_google_addressbook(tx, addressbook_id).await?;
        for sql in [
            sql!(
                "contacts.delete_addressbook.delete_addressbooks",
                "DELETE FROM addressbooks WHERE id = ?"
            ),
            sql!(
                "contacts.delete_addressbook.delete_addressbooks_bookkeeping",
                "DELETE FROM addressbooks_bookkeeping WHERE id = ?"
            ),
        ] {
            sql.query()
                .bind(addressbook_id)
                .execute(&mut **tx)
                .await
                .context("delete addressbook")?;
        }
        Ok(contacts as usize + people)
    }

    /// Snapshot every contact row for the render pass, joined
    /// against `addressbooks` so the caller gets the display-name
    /// label without a second query. Same shape regardless of
    /// whether the row landed via CardDAV sync-collection or
    /// [`super::vcf_dir::fetch`].
    pub async fn load_all_for_render_and_index_md(&self) -> Result<Vec<LoadedRawContact>> {
        let rows = sql!(
            "contacts.load_contacts_with_addressbook_label",
            "SELECT c.id AS id,
                    c.addressbook_id AS addressbook_id,
                    c.uid AS uid,
                    c.href AS href,
                    json_extract(c.payload, '$.vcard') AS vcard,
                    COALESCE(a.display_name, a.href) AS addressbook_label
             FROM contacts c
             LEFT JOIN addressbooks a ON a.id = c.addressbook_id
             ORDER BY c.addressbook_id, c.id"
        )
        .query()
        .fetch_all(&self.pool)
        .await
        .context("select contacts for render")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let vcard: Option<String> = r.try_get("vcard").context("contacts vcard")?;
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
        let rows = sql!(
            "contacts.list_addressbook_contact_uids",
            "SELECT uid FROM contacts WHERE addressbook_id = ?"
        )
        .query()
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

/// An address book's derived rows, found through its cards: neither table
/// has an index on `addressbook_id`, and each address book a `.vcf` folder
/// lost would walk both.
const ADDRESSBOOK_GROUP_MEMBERS: &str = "DELETE FROM contact_group_members \
     INDEXED BY contact_group_members_by_group WHERE group_id IN \
     (SELECT id FROM contacts INDEXED BY contacts_by_addressbook WHERE addressbook_id = ?)";
const ADDRESSBOOK_CATEGORIES: &str = "DELETE FROM contact_categories \
     INDEXED BY contact_categories_by_contact WHERE contact_id IN \
     (SELECT id FROM contacts INDEXED BY contacts_by_addressbook WHERE addressbook_id = ?)";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::schema_raw::DATA_TABLES;

    use datalib_sql::{test_sql, test_sql_id, Sql};
    /// Deleting an address book deletes its cards' derived rows through
    /// the cards' index, so a folder of `.vcf` files that lost many walks
    /// neither table once per file.
    #[tokio::test]
    async fn an_addressbooks_derived_rows_are_deleted_by_index() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("contacts.doltlite_db"))
            .await
            .unwrap();
        for sql in [ADDRESSBOOK_GROUP_MEMBERS, ADDRESSBOOK_CATEGORIES] {
            // Audited: a constant statement of this module.
            let plan: Vec<String> =
                Sql::dynamic(test_sql_id!(), format!("EXPLAIN QUERY PLAN {sql}"))
                    .query()
                    .bind("ab")
                    .fetch_all(db.pool())
                    .await
                    .unwrap()
                    .iter()
                    .map(|r| r.get::<String, _>("detail"))
                    .collect();
            assert!(
                plan.iter().all(|d| !d.starts_with("SCAN")),
                "{sql} scans a table: {plan:?}"
            );
            assert!(
                plan.iter().any(|d| d.starts_with("SEARCH contacts USING ")
                    && d.ends_with("INDEX contacts_by_addressbook (addressbook_id=?)")),
                "{sql}: {plan:?}"
            );
        }
        db.close().await;
    }

    /// An address book that goes takes its cards' memberships and labels,
    /// and leaves another book's.
    #[tokio::test]
    async fn a_deleted_addressbook_takes_its_members_and_categories() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("contacts.doltlite_db"))
            .await
            .unwrap();
        db.upsert_contact(&group_row(&["tng-picard"]))
            .await
            .unwrap();
        db.upsert_contact(&labelled("Bridge")).await.unwrap();
        db.upsert_contact(&ContactRow::new(
            "away".into(),
            "tng-ro".into(),
            "/cards/ro.vcf".into(),
            None,
            Some("Ro".into()),
            None,
            "BEGIN:VCARD\nVERSION:3.0\nUID:tng-ro\nFN:Ro\nCATEGORIES:Away\nEND:VCARD\n",
        ))
        .await
        .unwrap();

        assert_eq!(
            db.delete_file_addressbook("ab", "contacts/test", "ab.vcf")
                .await
                .unwrap(),
            2
        );
        assert!(members(&db).await.is_empty());
        assert_eq!(categories(&db).await, vec!["Away"]);
        db.close().await;
    }

    #[tokio::test]
    async fn open_creates_data_and_bookkeeping_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.doltlite_db");
        let db = RawDb::open(&path).await.unwrap();
        for t in DATA_TABLES {
            let bk = format!("{t}_bookkeeping");
            // Test: `bk` derives from the `DATA_TABLES` const.
            let row = Sql::dynamic(
                test_sql_id!(),
                format!("SELECT name FROM sqlite_master WHERE type='table' AND name = '{bk}'"),
            )
            .query()
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
        db.upsert_account(&AccountRow {
            id: "contacts.icloud.com".into(),
            server_url: Some("https://contacts.icloud.com/".into()),
            ..Default::default()
        })
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
        let r = test_sql!("SELECT display_name FROM contacts WHERE id = ?")
            .query()
            .bind(&id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let dn: String = r.try_get("display_name").unwrap();
        assert_eq!(dn, "Pat Q");

        let r = test_sql!("SELECT attempt_count, fetched_at_utc, last_error FROM contacts_bookkeeping WHERE id = ?").query()
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
        test_sql!(
            "SELECT group_id, member_id FROM contact_group_members ORDER BY group_id, member_id"
        )
        .query_as()
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
        test_sql!("SELECT category FROM contact_categories ORDER BY category")
            .query_scalar()
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
        let photo_tables: i64 = test_sql!(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'contact_photos'"
        )
        .query_scalar()
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(photo_tables, 0, "contact_photos survived the open");
        db.close().await;
    }
}
