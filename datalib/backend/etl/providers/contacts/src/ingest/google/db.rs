//! The raw store's Google tables: what a page of the listing writes, and
//! what render reads back.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::CasEdgeRow;
use datalib_etl::bulk::{bulk_upsert_entity_in_tx, bulk_upsert_in_tx};
use datalib_etl::doltlite_raw as dr;
use datalib_etl::prune;
use serde_json::Value;
use sqlx::{Row, Sqlite, Transaction};

use super::super::db::RawDb;
use super::super::schema_raw::{GoogleContactGroupRow, GooglePersonRow, GooglePhotoRow};
use super::super::FetchSummary;
use super::{photos, Page};

const PEOPLE: &str = "google_people";
const GROUPS: &str = "google_contact_groups";
pub const PHOTOS: &str = "google_photos";

/// One `google_people` row, for render.
#[derive(Debug, Clone)]
pub struct LoadedGooglePerson {
    pub id: String,
    pub addressbook_id: String,
    pub resource_name: String,
    /// As Google sent it, its volatile fields back in place.
    pub person: Value,
    /// The photo's `google_photos` row, and its bytes once fetched.
    pub photo: Option<(String, Option<String>)>,
}

/// One `google_contact_groups` row, for render.
#[derive(Debug, Clone)]
pub struct LoadedGoogleGroup {
    pub id: String,
    pub addressbook_id: String,
    pub resource_name: String,
    pub group: Value,
}

impl RawDb {
    /// Every label, and, when the listing named them all, the deletion of
    /// what it did not. A label whose content has not moved is not
    /// written again.
    pub async fn store_google_groups(
        &self,
        book: &str,
        rows: Vec<(GoogleContactGroupRow, Option<Value>)>,
        whole: bool,
    ) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin labels")?;
        let listed: HashSet<String> = rows
            .iter()
            .map(|(r, _)| r.id_and_payload.id.clone())
            .collect();
        let held = payloads(&mut tx, GROUPS, &listed).await?;
        let (rows, volatile): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .filter(|(r, _)| !is_held(&held, &r.id_and_payload))
            .unzip();
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
        set_volatile(
            &mut tx,
            GROUPS,
            rows.iter().map(|r| &r.id_and_payload),
            &volatile,
        )
        .await?;
        if whole {
            let gone =
                prune::prune_scope_in_tx(&mut tx, GROUPS, &[("addressbook_id", book)], &listed)
                    .await?;
            prune::record("labels", listed.len() + gone.len(), gone.len());
        }
        tx.commit().await.context("commit labels")
    }

    /// One page: the contacts on it, each one's photo as listed, and the
    /// deletions. Returns how many rows it wrote or removed.
    pub async fn store_google_page(
        tx: &mut Transaction<'static, Sqlite>,
        page: &Page,
        summary: &mut FetchSummary,
    ) -> Result<u64> {
        let (rows, volatile): (Vec<GooglePersonRow>, Vec<Option<Value>>) =
            page.rows.iter().cloned().unzip();
        let ids: HashSet<String> = rows.iter().map(|r| r.id_and_payload.id.clone()).collect();
        let held = payloads(tx, PEOPLE, &ids).await?;
        for row in &rows {
            if !held.contains_key(&row.id_and_payload.id) {
                summary.contacts_new += 1;
            } else if !is_held(&held, &row.id_and_payload) {
                summary.contacts_updated += 1;
            }
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        bulk_upsert_in_tx(tx, &rows, &now).await?;
        set_volatile(
            tx,
            PEOPLE,
            rows.iter().map(|r| &r.id_and_payload),
            &volatile,
        )
        .await?;
        list_photos(tx, &ids, &page.photos).await?;
        let gone = Self::delete_google_people(tx, &page.gone).await?;
        summary.contacts_deleted += gone;
        Ok((rows.len() + gone) as u64)
    }

    /// The contacts of `book` a whole listing did not name.
    pub async fn prune_google_people(
        tx: &mut Transaction<'static, Sqlite>,
        book: &str,
        listed: &HashSet<String>,
    ) -> Result<usize> {
        let gone =
            prune::prune_scope_in_tx(tx, PEOPLE, &[("addressbook_id", book)], listed).await?;
        prune::delete_owned_in_tx(tx, PHOTOS, GooglePhotoRow::OWNING_COLUMN, &gone).await?;
        prune::record("contacts", listed.len() + gone.len(), gone.len());
        Ok(gone.len())
    }

    async fn delete_google_people(
        tx: &mut Transaction<'static, Sqlite>,
        ids: &[String],
    ) -> Result<usize> {
        prune::delete_owned_in_tx(tx, PHOTOS, GooglePhotoRow::OWNING_COLUMN, ids).await?;
        Ok(prune::delete_owned_in_tx(tx, PEOPLE, "id", ids).await? as usize)
    }

    /// An address book's contacts, labels and photos. Returns how many
    /// contacts went.
    pub(in crate::ingest) async fn delete_google_addressbook(
        tx: &mut Transaction<'_, Sqlite>,
        book: &str,
    ) -> Result<usize> {
        let scope = [("addressbook_id", book)];
        let none = HashSet::new();
        let people = prune::prune_scope_in_tx(tx, PEOPLE, &scope, &none).await?;
        prune::delete_owned_in_tx(tx, PHOTOS, GooglePhotoRow::OWNING_COLUMN, &people).await?;
        prune::prune_scope_in_tx(tx, GROUPS, &scope, &none).await?;
        Ok(people.len())
    }

    /// Every photo `book` lists: its key, and the link to it.
    pub async fn google_photos_listed(&self, book: &str) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT ph.id, json(p.payload) AS payload,
                    json(b.volatile_payload) AS volatile
               FROM google_photos ph
               JOIN google_people p ON p.id = ph.person_id
               LEFT JOIN google_people_bookkeeping b ON b.id = p.id
              WHERE p.addressbook_id = ? ORDER BY ph.id",
        )
        .bind(book)
        .fetch_all(self.pool())
        .await
        .context("select the listed photos")?;
        rows.iter()
            .map(|r| {
                let id: String = r.try_get("id")?;
                let link = photos::photo_url(&overlaid(r)?)
                    .with_context(|| format!("{id} is listed with no link"))?;
                Ok((id, link))
            })
            .collect()
    }

    pub async fn set_google_photo(
        tx: &mut Transaction<'static, Sqlite>,
        key: &str,
        blake3: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE google_photos SET blake3 = ? WHERE id = ?")
            .bind(blake3)
            .bind(key)
            .execute(&mut **tx)
            .await
            .context("name a photo's bytes")?;
        Ok(())
    }

    /// The signed-in account's address.
    pub async fn google_login(&self) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar::<_, Option<String>>("SELECT login FROM accounts WHERE id = ?")
                .bind(super::ACCOUNT_ID)
                .fetch_optional(self.pool())
                .await
                .context("select the account")?
                .flatten(),
        )
    }

    pub async fn load_google_people(&self) -> Result<Vec<LoadedGooglePerson>> {
        let rows = sqlx::query(
            "SELECT p.id, p.addressbook_id, p.resource_name, json(p.payload) AS payload,
                    json(b.volatile_payload) AS volatile,
                    ph.id AS photo_id, ph.blake3 AS photo_blake3
               FROM google_people p
               LEFT JOIN google_people_bookkeeping b ON b.id = p.id
               LEFT JOIN google_photos ph ON ph.person_id = p.id
              ORDER BY p.id",
        )
        .fetch_all(self.pool())
        .await
        .context("select google people")?;
        rows.iter()
            .map(|r| {
                let photo: Option<String> = r.try_get("photo_id")?;
                Ok(LoadedGooglePerson {
                    id: r.try_get("id")?,
                    addressbook_id: r.try_get("addressbook_id")?,
                    resource_name: r.try_get("resource_name")?,
                    person: overlaid(r)?,
                    photo: match photo {
                        Some(id) => Some((id, r.try_get("photo_blake3")?)),
                        None => None,
                    },
                })
            })
            .collect()
    }

    pub async fn load_google_groups(&self) -> Result<Vec<LoadedGoogleGroup>> {
        let rows = sqlx::query(
            "SELECT g.id, g.addressbook_id, g.resource_name, json(g.payload) AS payload,
                    json(b.volatile_payload) AS volatile
               FROM google_contact_groups g
               LEFT JOIN google_contact_groups_bookkeeping b ON b.id = g.id
              ORDER BY g.id",
        )
        .fetch_all(self.pool())
        .await
        .context("select google contact groups")?;
        rows.iter()
            .map(|r| {
                Ok(LoadedGoogleGroup {
                    id: r.try_get("id")?,
                    addressbook_id: r.try_get("addressbook_id")?,
                    resource_name: r.try_get("resource_name")?,
                    group: overlaid(r)?,
                })
            })
            .collect()
    }
}

/// Each person's photo as just listed; a row for any other goes.
async fn list_photos(
    tx: &mut Transaction<'static, Sqlite>,
    people: &HashSet<String>,
    listed: &[GooglePhotoRow],
) -> Result<()> {
    let keep: HashSet<&str> = listed.iter().map(|p| p.id.as_str()).collect();
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM google_photos INDEXED BY google_photos_by_person_id \
         WHERE person_id IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(people)?)
    .fetch_all(&mut **tx)
    .await
    .context("select the people's photos")?;
    let gone: Vec<String> = stored
        .iter()
        .filter(|id| !keep.contains(id.as_str()))
        .cloned()
        .collect();
    prune::delete_owned_in_tx(tx, PHOTOS, "id", &gone).await?;
    let stored: HashSet<String> = stored.into_iter().collect();
    let new: Vec<GooglePhotoRow> = listed
        .iter()
        .filter(|p| !stored.contains(&p.id))
        .cloned()
        .collect();
    bulk_upsert_entity_in_tx(tx, &new).await
}

/// The stored content of `table`'s rows with these ids.
async fn payloads(
    tx: &mut Transaction<'_, Sqlite>,
    table: &'static str,
    ids: &HashSet<String>,
) -> Result<HashMap<String, Value>> {
    let ids: Vec<&String> = ids.iter().collect();
    // Audited: `table` is a `&'static str` of this module; the ids are bound.
    let rows: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, json(payload) FROM {table} WHERE id IN (SELECT value FROM json_each(?))"
    )))
    .bind(serde_json::to_string(&ids)?)
    .fetch_all(&mut **tx)
    .await
    .with_context(|| format!("select {table} payloads"))?;
    rows.into_iter()
        .map(|(id, payload)| {
            let payload = serde_json::from_str(&payload)
                .with_context(|| format!("parse the stored {table} {id}"))?;
            Ok((id, payload))
        })
        .collect()
}

fn is_held(held: &HashMap<String, Value>, row: &dr::WirePayload) -> bool {
    held.get(&row.id)
        .is_some_and(|held| serde_json::from_str::<Value>(&row.payload).ok().as_ref() == Some(held))
}

async fn set_volatile<'a>(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    rows: impl Iterator<Item = &'a dr::WirePayload>,
    volatile: &[Option<Value>],
) -> Result<()> {
    let volatile: Vec<(&str, &Value)> = rows
        .zip(volatile)
        .filter_map(|(row, v)| Some((row.id.as_str(), v.as_ref()?)))
        .collect();
    dr::set_volatile_payloads_in_tx(tx, table, &volatile).await
}

fn overlaid(r: &sqlx::sqlite::SqliteRow) -> Result<Value> {
    let payload: String = r.try_get("payload")?;
    let payload: Value = serde_json::from_str(&payload).context("parse a stored payload")?;
    let volatile: Option<String> = r.try_get("volatile")?;
    Ok(match volatile {
        Some(v) => dr::overlay(
            &payload,
            &serde_json::from_str(&v).context("parse a stored volatile payload")?,
        ),
        None => payload,
    })
}
