//! The contacts app's store: contacts a person made, and the handles they
//! linked to them. The one store under a data root that cannot be rebuilt
//! from anything, so it refuses a schema it cannot reach rather than
//! rebuilding, and every write is a commit a person can undo.
//!
//! Its one writer is the `datalib_contacts` applet. Nothing in the core
//! opens it; the core knows handles, never contacts.
//! `docs/dev/contacts.md` is the reference; what is still to build is
//! `docs/dev/plans/contact_linking.md` and `contact_editing.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
pub use datalib_contact_schema::ContactKind;
use datalib_contact_schema::{ContactHandle, Medium, NormalizedContact};
use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::doltlite_raw;
use datalib_handle::{Handle, HandleKind};
use datalib_store_meta::{Migration, StoreKind};
use datalib_time::IsoOffsetTimestamp;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use strum::{EnumString, IntoStaticStr, VariantArray};

/// Under the data root: one directory per app whose state a person
/// curates, so each can be managed or deleted on its own.
pub const CURATED_DIR: &str = datalib_runtime::layout::CURATED_DIR;
pub const APP_DIR: &str = "datalib_contacts";
pub const STORE_FILE: &str = "contacts.doltlite_db";

/// The `source_id` of every contact this app answers with: a contact is
/// one more account of a person, ranked above every source's.
pub const SOURCE_ID: &str = "datalib_contacts";

pub fn store_path(data_root: &Path) -> PathBuf {
    data_root.join(CURATED_DIR).join(APP_DIR).join(STORE_FILE)
}

pub mod drafts;
pub mod schema;

use schema::contacts::ContactRow;
use schema::handles::HandleRow;
use schema::photos::PhotoRow;
use schema::DDL;

/// Where the applet serves a contact's photo, relative to the app's
/// origin; what [`Store::contact`] answers as `photo_url`.
pub fn photo_url(contact_id: &str) -> String {
    format!("/applet/datalib_contacts/photo/{contact_id}")
}

/// The image types a photo may be: those a browser draws in an `<img>`.
/// Anything else is refused rather than stored.
pub const PHOTO_CONTENT_TYPES: &[&str] = datalib_contact_schema::DRAWABLE_PHOTO_TYPES;
/// Well under the gateway's body limit, and more than a profile photo
/// needs.
pub const PHOTO_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Whether a photo may be stored: its type and its size.
pub fn check_photo(content_type: &str, len: usize) -> Result<()> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !PHOTO_CONTENT_TYPES.contains(&ct.as_str()) {
        bail!(
            "{content_type:?} is not a photo: send one of {}",
            PHOTO_CONTENT_TYPES.join(", ")
        );
    }
    if len == 0 {
        bail!("the photo is empty");
    }
    if len > PHOTO_MAX_BYTES {
        bail!("the photo is {len} bytes; the most a contact's photo can be is {PHOTO_MAX_BYTES}");
    }
    Ok(())
}

/// The store's migration ladder (etl/README.md §"The migration ladder").
/// A link is a handle a person chose, so when the handle rules move the
/// links move with them: each rules change adds a rung that runs
/// [`rebuild_handles`], and [`HANDLE_RULES_OF_LADDER`] names the rules
/// the last such rung brought the links to.
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "every linked handle respelled under handle rules 1",
    apply: |conn| Box::pin(rebuild_handles(conn)),
}];

/// `datalib_handle::RULES_VERSION` as of the ladder's last handle rung.
#[cfg(test)]
const HANDLE_RULES_OF_LADDER: u32 = 1;

/// What a stored link comes to under this build's handle rules.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rebuilt {
    /// The rules spell it another way now, and nobody holds that spelling.
    Respell { from: String, to: String },
    /// Its contact already holds the new spelling; this row is a copy.
    Duplicate { from: String },
    /// Another contact holds the new spelling. Kept as it was.
    HeldElsewhere {
        from: String,
        to: String,
        holder: String,
    },
    /// The rules no longer make a handle of it. Kept as it was.
    Unmapped { from: String },
}

/// `links` is every `(handle, contact_id)` row; the answer names only
/// the rows that change or cannot.
fn plan_rebuild(links: &[(String, String)]) -> Vec<Rebuilt> {
    let mut holder: HashMap<String, String> = links.iter().cloned().collect();
    let mut out = Vec::new();
    for (from, contact) in links {
        let Some(to) = Handle::rebuild(from) else {
            out.push(Rebuilt::Unmapped { from: from.clone() });
            continue;
        };
        let to = to.as_str().to_string();
        if &to == from {
            continue;
        }
        match holder.get(&to) {
            None => {
                holder.remove(from);
                holder.insert(to.clone(), contact.clone());
                out.push(Rebuilt::Respell {
                    from: from.clone(),
                    to,
                });
            }
            Some(h) if h == contact => {
                holder.remove(from);
                out.push(Rebuilt::Duplicate { from: from.clone() });
            }
            Some(h) => out.push(Rebuilt::HeldElsewhere {
                from: from.clone(),
                to,
                holder: h.clone(),
            }),
        }
    }
    out
}

/// Respell every link under this build's handle rules. A link the rules
/// no longer read, or whose new spelling someone else holds, is kept as
/// written and logged; [`Store::contact`] shows it without a handle.
async fn rebuild_handles(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let links: Vec<(String, String)> =
        sqlx::query_as("SELECT handle, contact_id FROM handles ORDER BY handle")
            .fetch_all(&mut *conn)
            .await
            .context("read the linked handles")?;
    for step in plan_rebuild(&links) {
        match step {
            Rebuilt::Respell { from, to } => {
                sqlx::query("UPDATE handles SET handle = ? WHERE handle = ?")
                    .bind(&to)
                    .bind(&from)
                    .execute(&mut *conn)
                    .await
                    .with_context(|| format!("respell {from} as {to}"))?;
                tracing::info!(%from, %to, "contacts: a linked handle respelled");
            }
            Rebuilt::Duplicate { from } => {
                sqlx::query("DELETE FROM handles WHERE handle = ?")
                    .bind(&from)
                    .execute(&mut *conn)
                    .await
                    .with_context(|| format!("drop the copy {from}"))?;
                tracing::info!(%from, "contacts: a linked handle's contact already holds its new spelling");
            }
            Rebuilt::HeldElsewhere { from, to, holder } => tracing::warn!(
                %from,
                %to,
                %holder,
                "contacts: a linked handle's new spelling belongs to another contact; kept as written"
            ),
            Rebuilt::Unmapped { from } => tracing::warn!(
                %from,
                "contacts: a linked handle is no handle under this build's rules; kept as written"
            ),
        }
    }
    Ok(())
}

/// A link as the store holds it. One the handle rules no longer read is
/// shown as written, with no handle, rather than left out.
fn stored_link(stored: &str, stopped_working_by: Option<String>) -> ContactHandle {
    let mut link = match Handle::parse(stored) {
        Some(h) => ContactHandle::of(h),
        None => {
            tracing::warn!(
                handle = stored,
                "contacts: a linked handle this build does not read; shown as written"
            );
            let kind = stored
                .split_once(':')
                .and_then(|(k, _)| HandleKind::parse(k));
            ContactHandle {
                medium: kind.map_or(Medium::Other, Medium::of_kind),
                label: None,
                value: stored.to_string(),
                handle: None,
                stopped_working_by: None,
            }
        }
    };
    link.stopped_working_by = stopped_working_by;
    link
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, EnumString, IntoStaticStr, VariantArray)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum LinkedHow {
    Manual,
}

impl LinkedHow {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// What a contact's field says: a vCard property, more or less.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum FieldKind {
    Email,
    Phone,
    Organization,
    Title,
    Birthday,
    Address,
    Url,
    Other,
}

impl FieldKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContactSummary {
    pub contact_id: String,
    pub name: String,
    pub kind: String,
}

/// What linking a handle to a contact comes to, given who holds it now.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkPlan {
    Insert,
    AlreadyLinked,
    HeldBy(String),
}

fn plan_link(holder: Option<&str>, target: &str) -> LinkPlan {
    match holder {
        None => LinkPlan::Insert,
        Some(h) if h == target => LinkPlan::AlreadyLinked,
        Some(h) => LinkPlan::HeldBy(h.to_string()),
    }
}

/// `2019`, `2019-06` or `2019-06-14`: as precise as the person knows.
fn is_partial_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    let digits = |p: &str, n: usize| p.len() == n && p.chars().all(|c| c.is_ascii_digit());
    match parts.as_slice() {
        [y] => digits(y, 4),
        [y, m] => digits(y, 4) && digits(m, 2) && ("01"..="12").contains(m),
        [y, m, d] => {
            digits(y, 4)
                && digits(m, 2)
                && digits(d, 2)
                && ("01"..="12").contains(m)
                && ("01"..="31").contains(d)
        }
        _ => false,
    }
}

/// The open store. Holds the file's writer lock for as long as it lives;
/// [`Store::close`] before dropping it.
pub struct Store {
    pool: SqlitePool,
    /// Held across each draft operation: a save is several steps on the
    /// one connection (commit the draft, merge it, drop it), and an
    /// autosave landing between them would be dropped with the branch.
    drafts: tokio::sync::Mutex<()>,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        let pool = doltlite_raw::open_curated(path, DDL, StoreKind::Contacts, LADDER).await?;
        Ok(Self {
            pool,
            drafts: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn close(self) {
        self.pool.close().await;
    }

    /// The contact holding each of `handles`, by handle; a handle no
    /// contact holds is absent.
    pub async fn resolve(&self, handles: &[Handle]) -> Result<HashMap<String, NormalizedContact>> {
        let mut out = HashMap::new();
        for h in handles {
            let holder: Option<String> =
                sqlx::query_scalar("SELECT contact_id FROM handles WHERE handle = ?")
                    .bind(h.as_str())
                    .fetch_optional(&self.pool)
                    .await
                    .context("resolve a handle")?;
            if let Some(contact) = match holder {
                Some(id) => self.contact(&id).await?,
                None => None,
            } {
                out.insert(h.as_str().to_string(), contact);
            }
        }
        Ok(out)
    }

    /// Contacts whose name contains `q`, ignoring case; every contact
    /// for an empty `q`. Merged-away contacts are left out.
    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<ContactSummary>> {
        let pattern = format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let rows = sqlx::query(
            "SELECT contact_id, name, kind FROM contacts \
              WHERE merged_into IS NULL AND name LIKE ? ESCAPE '\\' \
              ORDER BY name COLLATE NOCASE LIMIT ?",
        )
        .bind(pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("search contacts")?;
        Ok(rows
            .iter()
            .map(|r| ContactSummary {
                contact_id: r.get("contact_id"),
                name: r.get("name"),
                kind: r.get("kind"),
            })
            .collect())
    }

    pub async fn contact(&self, contact_id: &str) -> Result<Option<NormalizedContact>> {
        let Some(r) = sqlx::query(
            "SELECT contact_id, name, kind, note, created_at_utc, updated_at_utc \
               FROM contacts WHERE contact_id = ?",
        )
        .bind(contact_id)
        .fetch_optional(&self.pool)
        .await
        .context("read a contact")?
        else {
            return Ok(None);
        };
        let kind: String = r.get("kind");
        let mut contact = NormalizedContact::new(
            SOURCE_ID,
            contact_id,
            ContactKind::parse(&kind).unwrap_or(ContactKind::Person),
        );
        contact.names = vec![r.get("name")];
        contact.note = r.get("note");
        contact.created_at = r.get("created_at_utc");
        contact.modified_at = r.get("updated_at_utc");
        let has_photo: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM photos WHERE contact_id = ?")
                .bind(contact_id)
                .fetch_optional(&self.pool)
                .await
                .context("read whether a contact has a photo")?;
        contact.photo_url = has_photo.map(|_| photo_url(contact_id));
        contact.handles = sqlx::query(
            "SELECT handle, stopped_working_by FROM handles WHERE contact_id = ? \
              ORDER BY stopped_working_by IS NOT NULL, handle",
        )
        .bind(contact_id)
        .fetch_all(&self.pool)
        .await
        .context("read a contact's handles")?
        .iter()
        .map(|h| stored_link(h.get("handle"), h.get("stopped_working_by")))
        .collect();
        Ok(Some(contact))
    }

    /// A new contact holding `handles`. Refused, with nothing written, if
    /// any of them already belongs to someone.
    pub async fn create(
        &self,
        name: &str,
        kind: ContactKind,
        handles: &[Handle],
    ) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a contact needs a name");
        }
        let contact_id = uuid::Uuid::new_v4().to_string();
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        let row = ContactRow {
            contact_id: contact_id.clone(),
            kind,
            name: name.to_string(),
            note: None,
            merged_into: None,
            created_at_utc: now.clone(),
            updated_at_utc: now.clone(),
            tz_offset: tz.clone(),
        };
        bulk_upsert_entity_in_tx(&mut tx, &[row])
            .await
            .context("insert a contact")?;
        for h in handles {
            match plan_link(holder(&mut tx, h).await?.as_deref(), &contact_id) {
                LinkPlan::Insert => insert_handle(&mut tx, h, &contact_id, &now, &tz).await?,
                LinkPlan::AlreadyLinked => {}
                LinkPlan::HeldBy(other) => {
                    bail!("{h} already belongs to {}", name_of(&mut tx, &other).await?)
                }
            }
        }
        tx.commit().await?;
        self.seal(&format!("contacts: new {} {name:?}", kind.as_str()))
            .await?;
        Ok(contact_id)
    }

    /// Link `handle` to `contact_id`. Linking it where it already is
    /// does nothing; linking a handle someone else holds is refused —
    /// unlink it there first.
    pub async fn link(&self, handle: &Handle, contact_id: &str) -> Result<()> {
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        let name: Option<String> =
            sqlx::query_scalar("SELECT name FROM contacts WHERE contact_id = ?")
                .bind(contact_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(name) = name else {
            bail!("no contact {contact_id}");
        };
        match plan_link(holder(&mut tx, handle).await?.as_deref(), contact_id) {
            LinkPlan::AlreadyLinked => return Ok(()),
            LinkPlan::HeldBy(other) => {
                bail!(
                    "{handle} already belongs to {}",
                    name_of(&mut tx, &other).await?
                )
            }
            LinkPlan::Insert => insert_handle(&mut tx, handle, contact_id, &now, &tz).await?,
        }
        tx.commit().await?;
        self.seal(&format!("contacts: link {handle} to {name:?}"))
            .await
    }

    /// Returns whether the handle was linked to anyone.
    pub async fn unlink(&self, handle: &Handle) -> Result<bool> {
        let done = sqlx::query("DELETE FROM handles WHERE handle = ?")
            .bind(handle.as_str())
            .execute(&self.pool)
            .await
            .context("unlink a handle")?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        self.seal(&format!("contacts: unlink {handle}")).await?;
        Ok(true)
    }

    /// Mark a linked handle as no longer working by `by` (a partial
    /// date), or as working again with `None`.
    pub async fn set_stopped_working(&self, handle: &Handle, by: Option<&str>) -> Result<bool> {
        if let Some(by) = by {
            if !is_partial_date(by) {
                bail!("{by:?} is not a date: write 2019, 2019-06 or 2019-06-14");
            }
        }
        let done = sqlx::query("UPDATE handles SET stopped_working_by = ? WHERE handle = ?")
            .bind(by)
            .bind(handle.as_str())
            .execute(&self.pool)
            .await
            .context("mark a handle")?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        let what = by.map_or("works again".to_string(), |d| {
            format!("stopped working by {d}")
        });
        self.seal(&format!("contacts: {handle} {what}")).await?;
        Ok(true)
    }

    pub async fn rename(&self, contact_id: &str, name: &str) -> Result<bool> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a contact needs a name");
        }
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let done = sqlx::query(
            "UPDATE contacts SET name = ?, updated_at_utc = ?, tz_offset = ? WHERE contact_id = ?",
        )
        .bind(name)
        .bind(&now)
        .bind(&tz)
        .bind(contact_id)
        .execute(&self.pool)
        .await
        .context("rename a contact")?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        self.seal(&format!("contacts: rename {contact_id} to {name:?}"))
            .await?;
        Ok(true)
    }

    /// The photo on a contact, as `(content_type, bytes)`; `None` where
    /// there is none.
    pub async fn photo(&self, contact_id: &str) -> Result<Option<(String, Vec<u8>)>> {
        let row = sqlx::query("SELECT content_type, bytes FROM photos WHERE contact_id = ?")
            .bind(contact_id)
            .fetch_optional(&self.pool)
            .await
            .context("read a contact's photo")?;
        Ok(row.map(|r| (r.get("content_type"), r.get("bytes"))))
    }

    /// Put a photo on a contact, replacing any it had. Refused for a
    /// contact that does not exist, or for bytes [`check_photo`] will
    /// not take.
    pub async fn set_photo(
        &self,
        contact_id: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<()> {
        check_photo(content_type, bytes.len())?;
        let ct = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        let name = name_of_existing(&mut tx, contact_id).await?;
        let row = PhotoRow {
            contact_id: contact_id.to_string(),
            content_type: ct,
            bytes: bytes.to_vec(),
            set_at_utc: now,
            tz_offset: tz,
        };
        bulk_upsert_entity_in_tx(&mut tx, &[row])
            .await
            .context("store a contact's photo")?;
        tx.commit().await?;
        self.seal(&format!("contacts: photo for {name:?}")).await
    }

    /// Returns whether the contact had a photo to take off.
    pub async fn clear_photo(&self, contact_id: &str) -> Result<bool> {
        let name = name_of_existing(&mut *self.pool.acquire().await?, contact_id).await?;
        let done = sqlx::query("DELETE FROM photos WHERE contact_id = ?")
            .bind(contact_id)
            .execute(&self.pool)
            .await
            .context("take a contact's photo off")?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        self.seal(&format!("contacts: no photo for {name:?}"))
            .await?;
        Ok(true)
    }

    async fn seal(&self, msg: &str) -> Result<()> {
        doltlite_raw::commit_run(&self.pool, msg).await?;
        Ok(())
    }
}

async fn holder(tx: &mut sqlx::SqliteConnection, h: &Handle) -> Result<Option<String>> {
    sqlx::query_scalar("SELECT contact_id FROM handles WHERE handle = ?")
        .bind(h.as_str())
        .fetch_optional(&mut *tx)
        .await
        .context("read who holds a handle")
}

/// A contact's name, or a refusal naming the id when there is no such
/// contact.
async fn name_of_existing(conn: &mut sqlx::SqliteConnection, contact_id: &str) -> Result<String> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM contacts WHERE contact_id = ?")
        .bind(contact_id)
        .fetch_optional(&mut *conn)
        .await
        .context("read a contact's name")?;
    name.ok_or_else(|| anyhow::anyhow!("no contact {contact_id}"))
}

async fn name_of(tx: &mut sqlx::SqliteConnection, contact_id: &str) -> Result<String> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM contacts WHERE contact_id = ?")
        .bind(contact_id)
        .fetch_optional(&mut *tx)
        .await
        .context("read a contact's name")?;
    Ok(name.unwrap_or_else(|| format!("contact {contact_id}")))
}

async fn insert_handle(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    h: &Handle,
    contact_id: &str,
    now: &str,
    tz: &str,
) -> Result<()> {
    let row = HandleRow {
        handle: h.as_str().to_string(),
        contact_id: contact_id.to_string(),
        linked_how: LinkedHow::Manual,
        linked_at_utc: now.to_string(),
        tz_offset: tz.to_string(),
        stopped_working_by: None,
    };
    bulk_upsert_entity_in_tx(tx, &[row])
        .await
        .context("link a handle")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email(s: &str) -> Handle {
        Handle::email(s).unwrap()
    }

    #[test]
    fn a_handle_is_linked_once_and_never_taken_silently() {
        assert_eq!(plan_link(None, "a"), LinkPlan::Insert);
        assert_eq!(plan_link(Some("a"), "a"), LinkPlan::AlreadyLinked);
        assert_eq!(plan_link(Some("b"), "a"), LinkPlan::HeldBy("b".into()));
    }

    #[test]
    fn stopped_working_takes_only_as_much_date_as_a_person_knows() {
        for ok in ["2019", "2019-06", "2019-06-14"] {
            assert!(is_partial_date(ok), "{ok}");
        }
        for bad in [
            "",
            "19",
            "2019-6",
            "2019-13",
            "2019-06-32",
            "2019-06-14T00:00",
            "June 2019",
        ] {
            assert!(!is_partial_date(bad), "{bad}");
        }
    }

    /// A rules change that the ladder has not caught up with leaves
    /// every link a person made spelled the old way, and the chips that
    /// carry the new spelling stop finding them.
    #[test]
    fn the_ladder_has_a_rung_for_the_current_handle_rules() {
        assert_eq!(
            HANDLE_RULES_OF_LADDER,
            datalib_handle::RULES_VERSION,
            "the handle rules moved: add a rung to LADDER that runs rebuild_handles, \
             and set HANDLE_RULES_OF_LADDER to the new RULES_VERSION"
        );
    }

    #[test]
    fn a_rebuild_respells_merges_and_keeps_what_it_cannot_place() {
        let links = |rows: &[(&str, &str)]| -> Vec<(String, String)> {
            rows.iter()
                .map(|(h, c)| (h.to_string(), c.to_string()))
                .collect()
        };
        assert_eq!(
            plan_rebuild(&links(&[
                ("email:mailto:troi@enterprise.org", "troi"),
                ("email:mailto:riker@enterprise.org", "riker"),
                ("email:riker@enterprise.org", "riker"),
                ("email:mailto:worf@enterprise.org", "worf"),
                ("email:worf@enterprise.org", "alexander"),
                ("tel:+1123456", "q"),
                ("tel:+12025550101", "picard"),
            ])),
            vec![
                Rebuilt::Respell {
                    from: "email:mailto:troi@enterprise.org".into(),
                    to: "email:troi@enterprise.org".into(),
                },
                Rebuilt::Duplicate {
                    from: "email:mailto:riker@enterprise.org".into(),
                },
                Rebuilt::HeldElsewhere {
                    from: "email:mailto:worf@enterprise.org".into(),
                    to: "email:worf@enterprise.org".into(),
                    holder: "alexander".into(),
                },
                Rebuilt::Unmapped {
                    from: "tel:+1123456".into(),
                },
            ]
        );
        assert_eq!(
            plan_rebuild(&links(&[
                ("email:mailto:data@enterprise.org", "data"),
                ("email:mailto:Data@enterprise.org", "lore"),
            ])),
            vec![
                Rebuilt::Respell {
                    from: "email:mailto:data@enterprise.org".into(),
                    to: "email:data@enterprise.org".into(),
                },
                Rebuilt::HeldElsewhere {
                    from: "email:mailto:Data@enterprise.org".into(),
                    to: "email:data@enterprise.org".into(),
                    holder: "data".into(),
                },
            ],
            "two old spellings of one new handle: the first takes it"
        );
    }

    /// A store from before the ladder (schema version 0), holding links
    /// an older build's rules spelled, is respelled on open; the links
    /// the rules no longer read are still on their contact.
    #[tokio::test]
    async fn a_store_from_before_the_ladder_is_respelled_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        let store = Store::open(&path).await.unwrap();
        let picard = store
            .create("Jean-Luc Picard", ContactKind::Person, &[])
            .await
            .unwrap();
        for old in ["email:mailto:picard@enterprise.org", "tel:+1123456"] {
            sqlx::query(
                "INSERT INTO handles (handle, contact_id, linked_how, linked_at_utc, tz_offset) \
                 VALUES (?, ?, 'manual', '2364-03-01T09:00:00.000000Z', '+00:00')",
            )
            .bind(old)
            .bind(&picard)
            .execute(&store.pool)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE _datalib_meta SET value = '0' WHERE key = 'schema_version'")
            .execute(&store.pool)
            .await
            .unwrap();
        store.seal("an older build's links").await.unwrap();
        store.close().await;

        let store = Store::open(&path).await.unwrap();
        let email = email("picard@enterprise.org");
        let got = store.resolve(std::slice::from_ref(&email)).await.unwrap();
        assert_eq!(
            got[email.as_str()].key,
            picard,
            "the respelled link resolves"
        );
        let c = store.contact(&picard).await.unwrap().unwrap();
        let shown: Vec<(String, Option<Handle>)> = c
            .handles
            .iter()
            .map(|h| (h.value.clone(), h.handle.clone()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("picard@enterprise.org".to_string(), Some(email)),
                ("tel:+1123456".to_string(), None),
            ],
            "the link the rules no longer read is shown as written, not dropped"
        );
        assert_eq!(c.handles[1].medium, Medium::Phone);
        store.close().await;
    }

    /// The tables as they were written by hand before they were derived
    /// from `schema.rs`. A store made with them has to open under the
    /// derived DDL unchanged: it is a person's own data, and a shape the
    /// open cannot reach additively is refused.
    const HAND_WRITTEN_DDL: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS contacts (
            contact_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            name TEXT NOT NULL,
            note TEXT,
            merged_into TEXT,
            created_at_utc TEXT NOT NULL,
            updated_at_utc TEXT NOT NULL,
            tz_offset TEXT NOT NULL
        )",
        "CREATE TABLE IF NOT EXISTS handles (
            handle TEXT PRIMARY KEY,
            contact_id TEXT NOT NULL,
            linked_how TEXT NOT NULL,
            linked_at_utc TEXT NOT NULL,
            tz_offset TEXT NOT NULL,
            stopped_working_by TEXT
        )",
        "CREATE INDEX IF NOT EXISTS handles_by_contact ON handles (contact_id)",
        "CREATE TABLE IF NOT EXISTS members (
            group_id TEXT NOT NULL,
            member_id TEXT NOT NULL,
            added_at_utc TEXT NOT NULL,
            tz_offset TEXT NOT NULL,
            PRIMARY KEY (group_id, member_id)
        )",
        "CREATE TABLE IF NOT EXISTS photos (
            contact_id TEXT PRIMARY KEY,
            content_type TEXT NOT NULL,
            bytes BLOB NOT NULL,
            set_at_utc TEXT NOT NULL,
            tz_offset TEXT NOT NULL
        )",
    ];

    #[tokio::test]
    async fn a_store_made_with_the_hand_written_tables_opens_under_the_derived_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let pool = doltlite_raw::open_curated(&path, HAND_WRITTEN_DDL, StoreKind::Contacts, LADDER)
            .await
            .unwrap();
        // A row in every table: an empty table is rebuilt whatever its
        // shape, which would hide a break.
        for sql in [
            "INSERT INTO contacts VALUES ('riker', 'person', 'William Riker', 'x', NULL, \
             '2364-03-01T09:00:00Z', '2364-03-01T09:00:00Z', '+00:00')",
            "INSERT INTO contacts VALUES ('away', 'group', 'Away team', NULL, NULL, \
             '2364-03-01T09:00:00Z', '2364-03-01T09:00:00Z', '+00:00')",
            "INSERT INTO handles VALUES ('email:riker@enterprise.org', 'riker', 'manual', \
             '2364-03-01T09:00:00Z', '+00:00', NULL)",
            "INSERT INTO members VALUES ('away', 'riker', '2364-03-01T09:00:00Z', '+00:00')",
            "INSERT INTO photos VALUES ('riker', 'image/png', x'89504e47', \
             '2364-03-01T09:00:00Z', '+00:00')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        doltlite_raw::commit_run(&pool, "a store from the hand-written tables")
            .await
            .unwrap();
        pool.close().await;

        let store = Store::open(&path).await.unwrap();
        let riker = store.contact("riker").await.unwrap().unwrap();
        assert_eq!(riker.names, ["William Riker"]);
        assert_eq!(riker.handles.len(), 1);
        assert_eq!(
            store.photo("riker").await.unwrap(),
            Some(("image/png".to_string(), vec![0x89, 0x50, 0x4e, 0x47]))
        );
        store.close().await;
    }

    #[test]
    fn strum_spellings_round_trip() {
        for k in LinkedHow::VARIANTS {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
    }

    /// The whole loop a chip drives: unresolved, created, resolved, and
    /// back to unresolved — each step a commit, on a real doltlite file.
    #[tokio::test]
    async fn create_link_resolve_unlink_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let riker = email("riker@enterprise.org");
        let tel = Handle::tel("+15550123456").unwrap();

        assert!(store
            .resolve(std::slice::from_ref(&riker))
            .await
            .unwrap()
            .is_empty());
        let id = store
            .create(
                "Will Riker",
                ContactKind::Person,
                std::slice::from_ref(&riker),
            )
            .await
            .unwrap();
        store.link(&tel, &id).await.unwrap();
        store.link(&tel, &id).await.unwrap();

        let got = store.resolve(&[riker.clone(), tel.clone()]).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[riker.as_str()].name(), Some("Will Riker"));
        assert_eq!(got[tel.as_str()].key, id);
        assert_eq!(got[tel.as_str()].source_id, SOURCE_ID);

        let other = store
            .create("Thomas Riker", ContactKind::Person, &[])
            .await
            .unwrap();
        let err = store.link(&tel, &other).await.unwrap_err();
        assert!(
            err.to_string().ends_with("already belongs to Will Riker"),
            "{err}"
        );
        let err = store
            .create("Twin", ContactKind::Person, std::slice::from_ref(&riker))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already belongs"), "{err}");
        let names: Vec<String> = store
            .search("riker", 10)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(
            names,
            ["Thomas Riker", "Will Riker"],
            "the refused create left nothing behind"
        );

        assert!(store
            .set_stopped_working(&tel, Some("2019-06"))
            .await
            .unwrap());
        let c = store.contact(&id).await.unwrap().unwrap();
        assert_eq!(
            c.handles[0].handle.as_ref(),
            Some(&riker),
            "working handles first"
        );
        assert_eq!(c.handles[1].stopped_working_by.as_deref(), Some("2019-06"));

        assert!(store.unlink(&tel).await.unwrap());
        assert!(!store.unlink(&tel).await.unwrap());
        assert!(store.resolve(&[tel]).await.unwrap().is_empty());
        store.close().await;
    }

    #[test]
    fn a_photo_is_an_image_of_a_size_a_contact_can_carry() {
        assert!(check_photo("image/png", 10).is_ok());
        assert!(check_photo("Image/JPEG; charset=binary", 10).is_ok());
        for (ct, len) in [
            ("text/html", 10),
            ("image/svg+xml", 10),
            ("", 10),
            ("image/png", 0),
            ("image/png", PHOTO_MAX_BYTES + 1),
        ] {
            assert!(check_photo(ct, len).is_err(), "{ct:?} {len}");
        }
    }

    /// A contact's photo: put on, served back as given, answered as a
    /// URL on the contact, taken off again — each a commit.
    #[tokio::test]
    async fn a_photo_rides_on_the_contact_until_it_is_taken_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let id = store
            .create("Will Riker", ContactKind::Person, &[])
            .await
            .unwrap();
        assert_eq!(store.contact(&id).await.unwrap().unwrap().photo_url, None);
        assert!(store.photo(&id).await.unwrap().is_none());
        let png = b"\x89PNG\r\n\x1a\n not really".to_vec();
        store.set_photo(&id, "image/png", &png).await.unwrap();
        assert_eq!(
            store.photo(&id).await.unwrap(),
            Some(("image/png".to_string(), png.clone()))
        );
        let c = store.contact(&id).await.unwrap().unwrap();
        assert_eq!(c.photo_url.as_deref(), Some(photo_url(&id).as_str()));
        let riker = email("riker@enterprise.org");
        store.link(&riker, &id).await.unwrap();
        let got = store.resolve(std::slice::from_ref(&riker)).await.unwrap();
        assert_eq!(
            got[riker.as_str()].photo_url,
            c.photo_url,
            "resolve answers it too"
        );

        store
            .set_photo(&id, "image/jpeg", b"\xff\xd8 replaced")
            .await
            .unwrap();
        assert_eq!(store.photo(&id).await.unwrap().unwrap().0, "image/jpeg");
        let err = store
            .set_photo(&id, "text/plain", b"hi")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a photo"), "{err}");
        let err = store
            .set_photo("nobody", "image/png", &png)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no contact nobody"), "{err}");

        assert!(store.clear_photo(&id).await.unwrap());
        assert!(!store.clear_photo(&id).await.unwrap());
        assert_eq!(store.contact(&id).await.unwrap().unwrap().photo_url, None);
        let log: Vec<String> = sqlx::query_scalar("SELECT message FROM dolt_log() LIMIT 4")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            log,
            [
                "contacts: no photo for \"Will Riker\"",
                "contacts: photo for \"Will Riker\"",
                "contacts: link email:riker@enterprise.org to \"Will Riker\"",
                "contacts: photo for \"Will Riker\"",
            ],
            "each change to a photo is a commit, and a refused one is none"
        );
        store.close().await;
    }

    /// A reader sees every edit as soon as it returns: each write is
    /// sealed onto `main`, not left on the writer's branch.
    #[tokio::test]
    async fn every_edit_reaches_main() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        let store = Store::open(&path).await.unwrap();
        store
            .create(
                "Deanna Troi",
                ContactKind::Person,
                &[email("troi@enterprise.org")],
            )
            .await
            .unwrap();
        let reader = doltlite_raw::open_reader(&path, None)
            .await
            .unwrap()
            .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM handles")
            .fetch_one(reader.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);
        reader.close().await;
        store.close().await;
    }
}
