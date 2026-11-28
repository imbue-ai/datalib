//! The contacts app's store: contacts a person made, and the handles they
//! linked to them. The one store under a data root that cannot be rebuilt
//! from anything, so it refuses a schema it cannot reach rather than
//! rebuilding, and every write is a commit a person can undo.
//!
//! Its one writer is the `datalib_contacts` applet. Nothing in the core
//! opens it; the core knows handles, never contacts.
//! `docs/dev/contacts.md` is the reference; what is still to build is
//! `docs/dev/plans/contact_linking.md` and `contact_editing.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
pub use datalib_contact_schema::ContactKind;

pub mod read;
use datalib_contact_schema::{ContactHandle, Medium, NormalizedContact};
use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::{doltlite_raw, draft};
use datalib_handle::{Handle, HandleKind};
use datalib_sql::{sql, sql_id, Sql};
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

pub mod add_people;
pub mod drafts;
pub mod labels;
pub mod review;
pub mod schema;

use schema::contacts::ContactRow;
use schema::handles::HandleRow;
use schema::me::MeRow;
use schema::photos::PhotoRow;
use schema::DDL;

/// Where the applet serves a contact's photo, relative to the app's
/// origin; what [`Store::contact`] answers as `photo_url`. `set_at`, the
/// photo's `set_at_utc`, makes a new photo a new URL: a page draws an
/// image it has already loaded under the same URL again from memory, so
/// a changed photo would not show until a reload. The route ignores it.
pub fn photo_url(contact_id: &str, set_at: &str) -> String {
    let version: String = set_at.chars().filter(char::is_ascii_digit).collect();
    format!("/applet/datalib_contacts/photo/{contact_id}?v={version}")
}

/// The URI a chip names one of your contacts by, whether or not it holds
/// a handle; `contactFromUri` in `ui/src/cards/chipLinks.js` reads it.
pub fn contact_uri(contact_id: &str) -> String {
    format!("datalib:contact/{contact_id}")
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
pub const LADDER: &[Migration] = &[
    Migration {
        version: 1,
        name: "every linked handle respelled under handle rules 1",
        apply: |conn| Box::pin(rebuild_handles(conn)),
    },
    Migration {
        version: 2,
        name: "every linked handle respelled under handle rules 2",
        apply: |conn| Box::pin(rebuild_handles(conn)),
    },
    Migration {
        version: 3,
        name: "a handle may be linked to more than one contact",
        apply: |conn| Box::pin(key_handles_by_contact_too(conn)),
    },
    Migration {
        version: 4,
        name: "a group is an ordinary contact, and its members carry its name as a label",
        apply: |conn| Box::pin(groups_become_labels(conn)),
    },
];

/// Rung 4: a contact is anyone the person added, so a group is an
/// ordinary contact, and what it grouped is a label: each member gets the
/// group's name as a label, as the person's own, and `members` goes. A
/// store last opened before `labels` existed has no members to carry (no
/// route ever added one), and is refused if it has.
async fn groups_become_labels(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let has_labels: Option<i64> = sql!(
        "contacts_app.migrate.has_labels",
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'labels'"
    )
    .query_scalar()
    .fetch_optional(&mut *conn)
    .await
    .context("read whether the store has labels")?;
    let members: Vec<(String, String, String, String)> = sql!(
        "contacts_app.migrate.list_members",
        "SELECT m.member_id, g.name, m.added_at_utc, m.tz_offset \
           FROM members m JOIN contacts g ON g.contact_id = m.group_id \
          ORDER BY m.member_id, g.name"
    )
    .query_as()
    .fetch_all(&mut *conn)
    .await
    .context("read the group members")?;
    if !members.is_empty() && has_labels.is_none() {
        bail!("this contacts store has group members and no labels table to carry them in");
    }
    for (member, group, at, tz) in &members {
        let Some(label) = labels::normalize_label(group) else {
            bail!("the group {group:?} is no label: rename it, then open the store again");
        };
        let label = review::spelled_as_held(conn, &label).await?;
        sql!(
            "contacts_app.migrate.label_a_member",
            "INSERT OR IGNORE INTO labels (subject, label, applies, decided_how, decided_at_utc, \
               reviewed_at_utc, tz_offset) VALUES (?, ?, 1, 'manual', ?, ?, ?)"
        )
        .query()
        .bind(contact_subject(member))
        .bind(&label)
        .bind(at)
        .bind(at)
        .bind(tz)
        .execute(&mut *conn)
        .await
        .context("label a group's member with the group")?;
    }
    for (stmt, what) in [
        (
            sql!("contacts_app.migrate.drop_members", "DROP TABLE members"),
            "drop the members",
        ),
        (
            sql!(
                "contacts_app.migrate.drop_contact_kind",
                "ALTER TABLE contacts DROP COLUMN kind"
            ),
            "drop whether a contact is a group",
        ),
    ] {
        stmt.query().execute(&mut *conn).await.context(what)?;
    }
    Ok(())
}

/// The `handles` table as rung 3 writes it, keyed `(handle, contact_id)`.
/// Frozen here rather than read from `schema.rs`, so a later rung starts
/// from the shape this one left.
const HANDLES_KEYED_BY_CONTACT: &[(&str, &str)] = &[
    ("handle", "TEXT NOT NULL"),
    ("contact_id", "TEXT NOT NULL"),
    ("linked_how", "TEXT NOT NULL"),
    ("linked_at_utc", "TEXT NOT NULL"),
    ("tz_offset", "TEXT NOT NULL"),
    ("stopped_working_by", "TEXT"),
    ("agent", "TEXT"),
    ("reason", "TEXT"),
    ("reviewed_at_utc", "TEXT"),
    ("confidence", "REAL"),
    ("run", "TEXT"),
];

/// The links go into a table keyed by handle and contact, then take the
/// old one's name. A store last opened by an older build may lack the
/// agent's columns, so only the columns it has are copied; the rest are
/// empty, as that build's open would have added them.
async fn key_handles_by_contact_too(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let had: Vec<String> = sql!(
        "contacts_app.migrate.list_link_columns",
        "SELECT name FROM pragma_table_info('handles')"
    )
    .query_scalar()
    .fetch_all(&mut *conn)
    .await
    .context("read the links' columns")?;
    let declared: Vec<String> = HANDLES_KEYED_BY_CONTACT
        .iter()
        .map(|(c, t)| format!("{c} {t}"))
        .collect();
    let copied: Vec<&str> = HANDLES_KEYED_BY_CONTACT
        .iter()
        .map(|(c, _)| *c)
        .filter(|c| had.iter().any(|h| h == c))
        .collect();
    let copied = copied.join(", ");
    let statements = [
        format!(
            "CREATE TABLE handles_by_contact_too (\n    {},\n    PRIMARY KEY (handle, contact_id)\n)",
            declared.join(",\n    ")
        ),
        format!(
            "INSERT INTO handles_by_contact_too ({copied}) \
             SELECT {copied} FROM handles ORDER BY handle, contact_id"
        ),
        "DROP INDEX IF EXISTS handles_by_contact".to_string(),
        "DROP TABLE handles".to_string(),
        "ALTER TABLE handles_by_contact_too RENAME TO handles".to_string(),
        "CREATE INDEX IF NOT EXISTS handles_by_contact ON handles(contact_id)".to_string(),
    ];
    for stmt in &statements {
        // Audited: every name is a literal above, or one of them that the
        // store's catalog says it has.
        Sql::dynamic(sql_id!("contacts_app.migrate.key_handles_by_contact"), stmt)
            .query()
            .execute(&mut *conn)
            .await
            .with_context(|| format!("key the links by contact: {stmt}"))?;
    }
    Ok(())
}

/// `datalib_handle::RULES_VERSION` as of the ladder's last handle rung.
#[cfg(test)]
const HANDLE_RULES_OF_LADDER: u32 = 2;

/// What a stored link comes to under this build's handle rules.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rebuilt {
    /// The rules spell it another way now, and nobody holds that spelling.
    Respell {
        from: String,
        to: String,
        contact: String,
    },
    /// Its contact already holds the new spelling; this row is a copy.
    Duplicate { from: String, contact: String },
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
    let mut holders: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (h, c) in links {
        holders.entry(h.clone()).or_default().insert(c.clone());
    }
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
        let has_to = holders.get(&to).cloned().unwrap_or_default();
        if has_to.contains(contact) {
            holders.entry(from.clone()).or_default().remove(contact);
            out.push(Rebuilt::Duplicate {
                from: from.clone(),
                contact: contact.clone(),
            });
        } else if let Some(other) = has_to.iter().next() {
            out.push(Rebuilt::HeldElsewhere {
                from: from.clone(),
                to,
                holder: other.clone(),
            });
        } else {
            holders.entry(from.clone()).or_default().remove(contact);
            holders
                .entry(to.clone())
                .or_default()
                .insert(contact.clone());
            out.push(Rebuilt::Respell {
                from: from.clone(),
                to,
                contact: contact.clone(),
            });
        }
    }
    out
}

/// Respell every link under this build's handle rules. A link the rules
/// no longer read, or whose new spelling someone else holds, is kept as
/// written and logged; [`Store::contact`] shows it without a handle.
async fn rebuild_handles(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let links: Vec<(String, String)> = sql!(
        "contacts_app.list_handles_for_rebuild",
        "SELECT handle, contact_id FROM handles ORDER BY handle, contact_id"
    )
    .query_as()
    .fetch_all(&mut *conn)
    .await
    .context("read the linked handles")?;
    for step in plan_rebuild(&links) {
        match step {
            Rebuilt::Respell { from, to, contact } => {
                sql!(
                    "contacts_app.respell_handle",
                    "UPDATE handles SET handle = ? WHERE handle = ? AND contact_id = ?"
                )
                .query()
                .bind(&to)
                .bind(&from)
                .bind(&contact)
                .execute(&mut *conn)
                .await
                .with_context(|| format!("respell {from} as {to}"))?;
                tracing::info!(%from, %to, "contacts: a linked handle respelled");
            }
            Rebuilt::Duplicate { from, contact } => {
                sql!(
                    "contacts_app.delete_duplicate_handle",
                    "DELETE FROM handles WHERE handle = ? AND contact_id = ?"
                )
                .query()
                .bind(&from)
                .bind(&contact)
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
fn stored_link(
    stored: &str,
    stopped_working_by: Option<String>,
    label: Option<String>,
) -> ContactHandle {
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
    link.label = label;
    link
}

/// How a link or a label was made: by a person's hand, or
/// proposed by an agent, which waits for a person to confirm it.
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
pub enum DecidedHow {
    Manual,
    Agent,
}

impl DecidedHow {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// The one label agents curate, in when the person or sender is
/// not spam: someone the person deals with, or a machine they want (a
/// receipt, a notification). What the review shows a toggle for.
pub const REAL: &str = "real";

/// Who makes a link or a contact: a person, or an agent giving its
/// reason, how sure it is and which of its runs this is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Maker<'a> {
    Person,
    Agent {
        agent: &'a str,
        reason: Option<&'a str>,
        confidence: Option<f64>,
        run: Option<&'a str>,
    },
}

impl Maker<'_> {
    fn how(self) -> DecidedHow {
        match self {
            Maker::Person => DecidedHow::Manual,
            Maker::Agent { .. } => DecidedHow::Agent,
        }
    }

    fn agent(self) -> Option<String> {
        match self {
            Maker::Person => None,
            Maker::Agent { agent, .. } => Some(agent.to_string()),
        }
    }

    fn reason(self) -> Option<String> {
        match self {
            Maker::Person => None,
            Maker::Agent { reason, .. } => reason.map(str::to_string),
        }
    }

    fn confidence(self) -> Option<f64> {
        match self {
            Maker::Person => None,
            Maker::Agent { confidence, .. } => confidence,
        }
    }

    fn run(self) -> Option<String> {
        match self {
            Maker::Person => None,
            Maker::Agent { run, .. } => run.map(str::to_string),
        }
    }
}

/// The labels table's key for a contact; a handle's is the handle.
pub fn contact_subject(contact_id: &str) -> String {
    format!("contact:{contact_id}")
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
}

/// A contact as the store answers it, from its `contacts` row, its links
/// (working first), when its photo was set, if it has one, and whether
/// the person said it is them, which it answers as `account`.
fn assemble(
    r: &sqlx::sqlite::SqliteRow,
    links: Vec<ContactHandle>,
    photo_set_at: Option<&str>,
    me: bool,
) -> NormalizedContact {
    let contact_id: String = r.get("contact_id");
    let mut contact = NormalizedContact::new(SOURCE_ID, &contact_id, ContactKind::Person);
    contact.names = vec![r.get("name")];
    contact.note = r.get("note");
    contact.created_at = r.get("created_at_utc");
    contact.modified_at = r.get("updated_at_utc");
    contact.photo_url = photo_set_at.map(|at| photo_url(&contact_id, at));
    contact.handles = links;
    contact.account = me;
    contact
}

/// [`Store::search`], over any open of the store, the writer's or a
/// reader's ([`read::ContactsReader`]).
pub(crate) async fn search_in(
    pool: &SqlitePool,
    q: &str,
    limit: u32,
) -> Result<Vec<ContactSummary>> {
    let pattern = format!(
        "%{}%",
        q.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let rows = sql!(
        "contacts_app.search_contacts_by_name",
        "SELECT contact_id, name FROM contacts c \
          WHERE merged_into IS NULL AND name LIKE ? ESCAPE '\\' AND NOT (agent IS NOT NULL \
            AND EXISTS (SELECT 1 FROM handles h INDEXED BY handles_by_contact \
                         WHERE h.contact_id = c.contact_id \
                           AND h.linked_how = 'agent' AND h.reviewed_at_utc IS NULL) \
            AND NOT EXISTS (SELECT 1 FROM handles h INDEXED BY handles_by_contact \
                             WHERE h.contact_id = c.contact_id \
                               AND (h.linked_how = 'manual' OR h.reviewed_at_utc IS NOT NULL))) \
          ORDER BY name COLLATE NOCASE LIMIT ?"
    )
    .query()
    .bind(pattern)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("search contacts")?;
    Ok(rows
        .iter()
        .map(|r| ContactSummary {
            contact_id: r.get("contact_id"),
            name: r.get("name"),
        })
        .collect())
}

/// What linking a handle to a contact comes to, given who holds it now.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkPlan {
    Insert,
    AlreadyLinked,
    /// Other contacts have it, and the person has not said what to do.
    HeldBy(Vec<String>),
    /// Other contacts have it, and the person said both are theirs.
    Share,
    /// Other contacts have it, and the person said it is this one's only.
    TakeFrom(Vec<String>),
}

fn plan_link(holders: &[String], target: &str, held: WhenHeld) -> LinkPlan {
    if holders.iter().any(|h| h == target) {
        return LinkPlan::AlreadyLinked;
    }
    if holders.is_empty() {
        return LinkPlan::Insert;
    }
    match held {
        WhenHeld::Refuse => LinkPlan::HeldBy(holders.to_vec()),
        WhenHeld::Share => LinkPlan::Share,
        WhenHeld::Move => LinkPlan::TakeFrom(holders.to_vec()),
    }
}

/// What to do when a handle a person links is on another contact
/// already: refuse, so the person can be asked; link it to both, for an
/// address two people share; or move it, which is taking it off the
/// others as not theirs.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
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
pub enum WhenHeld {
    #[default]
    Refuse,
    Share,
    Move,
}

impl WhenHeld {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// What was said about a handle as it was added: the person's label for
/// it ("work"), and the source record it was copied from, where it was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandleFrom {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub copied_from: Option<drafts::CopiedFrom>,
}

impl HandleFrom {
    fn label(&self) -> Option<String> {
        self.label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
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

    /// The contacts holding each of `handles`, by handle, by name; a
    /// handle no contact holds is absent.
    pub async fn resolve(
        &self,
        handles: &[Handle],
    ) -> Result<HashMap<String, Vec<NormalizedContact>>> {
        let mut out = HashMap::new();
        for h in handles {
            let mut theirs = Vec::new();
            for id in holders(&self.pool, h).await? {
                if let Some(contact) = self.contact(&id).await? {
                    theirs.push(contact);
                }
            }
            if !theirs.is_empty() {
                out.insert(h.as_str().to_string(), theirs);
            }
        }
        Ok(out)
    }

    /// Contacts whose name contains `q`, ignoring case; every contact
    /// for an empty `q`. Merged-away contacts are left out.
    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<ContactSummary>> {
        search_in(&self.pool, q, limit).await
    }

    /// A merged-away contact answers as the one it was merged into.
    pub async fn contact(&self, contact_id: &str) -> Result<Option<NormalizedContact>> {
        let Some(contact_id) = sql!(
            "contacts_app.find_merge_survivor_id",
            "SELECT COALESCE(merged_into, contact_id) FROM contacts WHERE contact_id = ?"
        )
        .query_scalar::<String>()
        .bind(contact_id)
        .fetch_optional(&self.pool)
        .await
        .context("read whether a contact was merged")?
        else {
            return Ok(None);
        };
        let contact_id = contact_id.as_str();
        let Some(r) = sql!(
            "contacts_app.read_contact",
            "SELECT contact_id, name, note, created_at_utc, updated_at_utc \
               FROM contacts WHERE contact_id = ?"
        )
        .query()
        .bind(contact_id)
        .fetch_optional(&self.pool)
        .await
        .context("read a contact")?
        else {
            return Ok(None);
        };
        let photo_set_at: Option<String> = sql!(
            "contacts_app.read_photo_set_at",
            "SELECT set_at_utc FROM photos WHERE contact_id = ?"
        )
        .query_scalar()
        .bind(contact_id)
        .fetch_optional(&self.pool)
        .await
        .context("read whether a contact has a photo")?;
        let links = sql!(
            "contacts_app.list_handles_of_contact",
            "SELECT handle, stopped_working_by, label FROM handles INDEXED BY handles_by_contact \
              WHERE contact_id = ? AND (linked_how = 'manual' OR reviewed_at_utc IS NOT NULL) \
              ORDER BY stopped_working_by IS NOT NULL, handle"
        )
        .query()
        .bind(contact_id)
        .fetch_all(&self.pool)
        .await
        .context("read a contact's handles")?
        .iter()
        .map(|h| stored_link(h.get("handle"), h.get("stopped_working_by"), h.get("label")))
        .collect();
        let me: Option<i64> = sql!(
            "contacts_app.is_contact_me",
            "SELECT 1 FROM me WHERE contact_id = ?"
        )
        .query_scalar()
        .bind(contact_id)
        .fetch_optional(&self.pool)
        .await
        .context("read whether a contact is me")?;
        Ok(Some(assemble(
            &r,
            links,
            photo_set_at.as_deref(),
            me.is_some(),
        )))
    }

    /// Each contact's labels, by its id, sorted: those a person gave or
    /// said Yes to, in, but `REAL`, which is about whether they are spam.
    /// What the contacts list's *Labels* and the Library tree's labels read.
    pub async fn contact_labels(&self) -> Result<BTreeMap<String, Vec<String>>> {
        let rows: Vec<(String, String)> = sql!(
            "contacts_app.list_contact_labels",
            "SELECT substr(subject, length('contact:') + 1), label FROM labels \
              WHERE subject LIKE 'contact:%' AND applies = 1 AND label <> ? \
                AND (decided_how = 'manual' OR reviewed_at_utc IS NOT NULL) \
              ORDER BY 1, 2"
        )
        .query_as()
        .bind(REAL)
        .fetch_all(&self.pool)
        .await
        .context("list the contacts' labels")?;
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, label) in rows {
            out.entry(id).or_default().push(label);
        }
        Ok(out)
    }

    /// Every contact, by name, as [`Store::contact`] answers each: what the
    /// contacts app's own table lists. Merged-away contacts are left out.
    /// Four reads, whatever the count.
    pub async fn list(&self) -> Result<Vec<NormalizedContact>> {
        let rows = sql!(
            "contacts_app.list_unmerged_contacts",
            "SELECT contact_id, name, note, created_at_utc, updated_at_utc \
               FROM contacts c WHERE merged_into IS NULL AND NOT (agent IS NOT NULL \
                AND EXISTS (SELECT 1 FROM handles h INDEXED BY handles_by_contact \
                             WHERE h.contact_id = c.contact_id \
                               AND h.linked_how = 'agent' AND h.reviewed_at_utc IS NULL) \
                AND NOT EXISTS (SELECT 1 FROM handles h INDEXED BY handles_by_contact \
                                 WHERE h.contact_id = c.contact_id \
                                   AND (h.linked_how = 'manual' OR h.reviewed_at_utc IS NOT NULL))) \
              ORDER BY name COLLATE NOCASE, contact_id"
        )
        .query()
        .fetch_all(&self.pool)
        .await
        .context("list contacts")?;
        let mut links: HashMap<String, Vec<ContactHandle>> = HashMap::new();
        for h in sql!(
            "contacts_app.list_all_handles",
            "SELECT contact_id, handle, stopped_working_by, label FROM handles \
              WHERE linked_how = 'manual' OR reviewed_at_utc IS NOT NULL \
              ORDER BY contact_id, stopped_working_by IS NOT NULL, handle"
        )
        .query()
        .fetch_all(&self.pool)
        .await
        .context("list the linked handles")?
        {
            links
                .entry(h.get("contact_id"))
                .or_default()
                .push(stored_link(
                    h.get("handle"),
                    h.get("stopped_working_by"),
                    h.get("label"),
                ));
        }
        let photo_set_at: HashMap<String, String> = sql!(
            "contacts_app.list_photo_set_at",
            "SELECT contact_id, set_at_utc FROM photos"
        )
        .query_as()
        .fetch_all(&self.pool)
        .await
        .context("list the contacts with a photo")?
        .into_iter()
        .collect();
        let me: std::collections::HashSet<String> =
            sql!("contacts_app.list_me_contacts", "SELECT contact_id FROM me")
                .query_scalar()
                .fetch_all(&self.pool)
                .await
                .context("list the contacts that are me")?
                .into_iter()
                .collect();
        Ok(rows
            .iter()
            .map(|r| {
                let id: String = r.get("contact_id");
                let set_at = photo_set_at.get(&id).map(String::as_str);
                let links = links.remove(&id).unwrap_or_default();
                assemble(r, links, set_at, me.contains(&id))
            })
            .collect())
    }

    /// A new contact holding `handles`. Refused, with nothing written, if
    /// any of them already belongs to someone.
    pub async fn create(&self, name: &str, handles: &[Handle]) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a contact needs a name");
        }
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        let contact_id = insert_contact(&mut tx, name, Maker::Person, &now, &tz).await?;
        for h in handles {
            let held = holders(&mut *tx, h).await?;
            match plan_link(&held, &contact_id, WhenHeld::Refuse) {
                LinkPlan::Insert => {
                    let from = HandleFrom::default();
                    insert_handle(&mut tx, h, &contact_id, Maker::Person, &from, &now, &tz).await?
                }
                LinkPlan::AlreadyLinked => {}
                _ => bail!("{h} already belongs to {}", names_of(&mut tx, &held).await?),
            }
        }
        tx.commit().await?;
        self.seal(&format!("contacts: new {name:?}")).await?;
        Ok(contact_id)
    }

    /// Link `handle` to `contact_id`. Linking it where it already is
    /// does nothing. A handle other contacts have is refused unless `held`
    /// says to share it with them or to move it here, which takes it off
    /// them as not theirs. A person linking a pair they once turned down
    /// has changed their mind, so the turn-down goes. `from` is the
    /// label and the card it came with; a handle linked already keeps
    /// its own.
    pub async fn link(
        &self,
        handle: &Handle,
        contact_id: &str,
        held: WhenHeld,
        from: &HandleFrom,
    ) -> Result<()> {
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        let name: Option<String> = sql!(
            "contacts_app.read_name_to_link",
            "SELECT name FROM contacts WHERE contact_id = ?"
        )
        .query_scalar()
        .bind(contact_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(name) = name else {
            bail!("no contact {contact_id}");
        };
        let linked = link_in(&mut tx, handle, contact_id, &name, held, from, &now, &tz).await?;
        let Some(what) = linked else {
            return Ok(());
        };
        tx.commit().await?;
        self.seal(&format!("contacts: {what}")).await
    }

    /// Returns whether the handle was linked to the contact. Taking a
    /// handle off a contact is written down in `not_linked`, so no agent
    /// proposes it again; anyone else who has it keeps it.
    pub async fn unlink(&self, handle: &Handle, contact_id: &str) -> Result<bool> {
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        if !review::unlink_in(&mut tx, handle.as_str(), contact_id, &now, &tz).await? {
            return Ok(false);
        }
        tx.commit().await?;
        self.seal(&format!("contacts: unlink {handle}")).await?;
        Ok(true)
    }

    /// Mark a handle linked to `contact_id` as no longer working for them
    /// by `by` (a partial date), or as working again with `None`. Anyone
    /// else who has it is untouched: an address one person stopped using
    /// may still be another's.
    pub async fn set_stopped_working(
        &self,
        handle: &Handle,
        contact_id: &str,
        by: Option<&str>,
    ) -> Result<bool> {
        if let Some(by) = by {
            if !is_partial_date(by) {
                bail!("{by:?} is not a date: write 2019, 2019-06 or 2019-06-14");
            }
        }
        let done = sql!(
            "contacts_app.mark_handle_stopped_working",
            "UPDATE handles SET stopped_working_by = ? WHERE handle = ? AND contact_id = ?"
        )
        .query()
        .bind(by)
        .bind(handle.as_str())
        .bind(contact_id)
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

    /// Label a handle linked to `contact_id` ("work", "cell"), or clear
    /// its label with `None`. Returns whether it was linked to them.
    pub async fn set_handle_label(
        &self,
        handle: &Handle,
        contact_id: &str,
        label: Option<&str>,
    ) -> Result<bool> {
        let label = label.map(str::trim).filter(|l| !l.is_empty());
        let done = sql!(
            "contacts_app.label_handle",
            "UPDATE handles SET label = ? WHERE handle = ? AND contact_id = ?"
        )
        .query()
        .bind(label)
        .bind(handle.as_str())
        .bind(contact_id)
        .execute(&self.pool)
        .await
        .context("label a handle")?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        let what = label.map_or("has no label".to_string(), |l| format!("is {l:?}"));
        self.seal(&format!("contacts: {handle} {what}")).await?;
        Ok(true)
    }

    pub async fn rename(&self, contact_id: &str, name: &str) -> Result<bool> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a contact needs a name");
        }
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let done = sql!(
            "contacts_app.rename_contact",
            "UPDATE contacts SET name = ?, updated_at_utc = ?, tz_offset = ? WHERE contact_id = ?"
        )
        .query()
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
        let row = sql!(
            "contacts_app.read_photo",
            "SELECT content_type, bytes FROM photos WHERE contact_id = ?"
        )
        .query()
        .bind(contact_id)
        .fetch_optional(&self.pool)
        .await
        .context("read a contact's photo")?;
        Ok(row.map(|r| (r.get("content_type"), r.get("bytes"))))
    }

    /// Put a photo on a contact, replacing any it had, and answer the URL
    /// it is served at. Refused for a contact that does not exist, or for
    /// bytes [`check_photo`] will not take.
    pub async fn set_photo(
        &self,
        contact_id: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<String> {
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
        let url = photo_url(contact_id, &now);
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
        self.seal(&format!("contacts: photo for {name:?}")).await?;
        Ok(url)
    }

    /// Returns whether the contact had a photo to take off.
    pub async fn clear_photo(&self, contact_id: &str) -> Result<bool> {
        let name = name_of_existing(&mut *self.pool.acquire().await?, contact_id).await?;
        let done = sql!(
            "contacts_app.delete_photo",
            "DELETE FROM photos WHERE contact_id = ?"
        )
        .query()
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

    /// Say whether `contact_id` is the person whose library this is, or
    /// stop saying so. A merged-away contact marks its survivor. Returns
    /// whether anything changed.
    pub async fn set_me(&self, contact_id: &str, me: bool) -> Result<bool> {
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        let row = sql!(
            "contacts_app.read_survivor_for_me",
            "SELECT c.contact_id, c.name FROM contacts c \
               JOIN contacts asked ON c.contact_id = COALESCE(asked.merged_into, asked.contact_id) \
              WHERE asked.contact_id = ?"
        )
        .query()
        .bind(contact_id)
        .fetch_optional(&mut *tx)
        .await
        .context("read the contact to mark")?
        .ok_or_else(|| anyhow::anyhow!("no contact {contact_id}"))?;
        let (id, name): (String, String) = (row.get("contact_id"), row.get("name"));
        let done = if me {
            let row = MeRow {
                contact_id: id.clone(),
                marked_at_utc: now,
                tz_offset: tz,
            };
            let before: Option<i64> = sql!(
                "contacts_app.is_survivor_me",
                "SELECT 1 FROM me WHERE contact_id = ?"
            )
            .query_scalar()
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
            if before.is_none() {
                bulk_upsert_entity_in_tx(&mut tx, &[row])
                    .await
                    .context("mark a contact as me")?;
            }
            before.is_none()
        } else {
            sql!(
                "contacts_app.unmark_me",
                "DELETE FROM me WHERE contact_id = ?"
            )
            .query()
            .bind(&id)
            .execute(&mut *tx)
            .await
            .context("unmark a contact as me")?
            .rows_affected()
                > 0
        };
        tx.commit().await?;
        if done {
            let what = if me { "is me" } else { "is not me" };
            self.seal(&format!("contacts: {name:?} {what}")).await?;
        }
        Ok(done)
    }

    /// Delete a contact: its row, its links, fields, photo, draft and
    /// labels, and every contact merged into it, which
    /// were the same person. Its handles are nobody's afterwards. What a
    /// person turned down for it stays, as a record. The commit before it
    /// still holds all of it.
    pub async fn delete(&self, contact_id: &str) -> Result<()> {
        let _held = self.drafts.lock().await;
        let mut tx = self.pool.begin().await?;
        let name = name_of_existing(&mut tx, contact_id).await?;
        let mut gone: Vec<String> = sql!(
            "contacts_app.list_merged_into",
            "SELECT contact_id FROM contacts WHERE merged_into = ?"
        )
        .query_scalar()
        .bind(contact_id)
        .fetch_all(&mut *tx)
        .await
        .context("read the contacts merged into one")?;
        gone.push(contact_id.to_string());
        for id in &gone {
            for sql in [
                sql!(
                    "contacts_app.delete.delete_handles",
                    "DELETE FROM handles WHERE contact_id = ?1"
                ),
                sql!(
                    "contacts_app.delete.delete_fields",
                    "DELETE FROM fields WHERE contact_id = ?1"
                ),
                sql!(
                    "contacts_app.delete.delete_photos",
                    "DELETE FROM photos WHERE contact_id = ?1"
                ),
                sql!(
                    "contacts_app.delete.delete_me",
                    "DELETE FROM me WHERE contact_id = ?1"
                ),
                sql!(
                    "contacts_app.delete.delete_labels",
                    "DELETE FROM labels WHERE subject = 'contact:' || ?1"
                ),
                sql!(
                    "contacts_app.delete.delete_contacts",
                    "DELETE FROM contacts WHERE contact_id = ?1"
                ),
            ] {
                sql.query()
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .context("delete a contact")?;
            }
        }
        tx.commit().await?;
        self.seal(&format!("contacts: delete {name:?}")).await?;
        for id in &gone {
            draft::discard(&self.pool, &draft::branch_of(id)).await?;
        }
        Ok(())
    }

    /// Merge `from` into `into`, which survives: its links, fields,
    /// labels and turned-down links move over, and so does
    /// its mark as me; its photo and note fill in where the survivor has
    /// none, and `from` is kept as a row pointing at `into` so an id saved
    /// anywhere (a `contact:` search) still reaches the person. A contact
    /// already merged into `from` now points at `into`, so a chain is never
    /// more than one step.
    pub async fn merge(&self, from: &str, into: &str) -> Result<()> {
        if from == into {
            bail!("a contact cannot be merged into itself");
        }
        let _held = self.drafts.lock().await;
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        let (from_name, from_note) = merge_side(&mut tx, from).await?;
        let (into_name, into_note) = merge_side(&mut tx, into).await?;
        let after: i64 = sql!(
            "contacts_app.read_next_field_position",
            "SELECT COALESCE(MAX(position), -1) + 1 FROM fields WHERE contact_id = ?"
        )
        .query_scalar()
        .bind(into)
        .fetch_one(&mut *tx)
        .await
        .context("read where the survivor's fields end")?;
        for (sql, what) in [
            (
                "INSERT OR IGNORE INTO handles (handle, contact_id, linked_how, linked_at_utc, \
                   tz_offset, stopped_working_by, agent, reason, reviewed_at_utc, confidence, run, \
                   label, copied_from_source, copied_from_key) \
                 SELECT handle, ?2, linked_how, linked_at_utc, tz_offset, stopped_working_by, \
                   agent, reason, reviewed_at_utc, confidence, run, label, copied_from_source, \
                   copied_from_key FROM handles WHERE contact_id = ?1",
                "move the links, the survivor's own winning",
            ),
            ("DELETE FROM handles WHERE contact_id = ?1", "drop the old links"),
            (
                "UPDATE photos SET contact_id = ?2 WHERE contact_id = ?1 \
                  AND NOT EXISTS (SELECT 1 FROM photos WHERE contact_id = ?2)",
                "move the photo",
            ),
            ("DELETE FROM photos WHERE contact_id = ?1", "drop the photo"),
            (
                "INSERT OR IGNORE INTO me (contact_id, marked_at_utc, tz_offset) \
                 SELECT ?2, marked_at_utc, tz_offset FROM me WHERE contact_id = ?1",
                "move the mark that it is me",
            ),
            ("DELETE FROM me WHERE contact_id = ?1", "drop the mark"),
            (
                "INSERT OR IGNORE INTO not_linked (handle, contact_id, contact_name, agent, \
                   reason, decided_at_utc, tz_offset, contact_rejected) \
                 SELECT handle, ?2, contact_name, agent, reason, decided_at_utc, tz_offset, \
                   contact_rejected FROM not_linked WHERE contact_id = ?1",
                "move the turned-down links",
            ),
            (
                "DELETE FROM not_linked WHERE contact_id = ?1",
                "drop the old turned-down links",
            ),
            (
                "INSERT OR IGNORE INTO labels (subject, label, applies, decided_how, agent, reason, \
                   decided_at_utc, reviewed_at_utc, tz_offset, confidence, run) \
                 SELECT 'contact:' || ?2, label, applies, decided_how, agent, reason, \
                   decided_at_utc, reviewed_at_utc, tz_offset, confidence, run \
                   FROM labels WHERE subject = 'contact:' || ?1",
                "move the labels, the survivor's own winning",
            ),
            (
                "DELETE FROM labels WHERE subject = 'contact:' || ?1",
                "drop the old labels",
            ),
            (
                "UPDATE contacts SET merged_into = ?2 WHERE merged_into = ?1 OR contact_id = ?1",
                "point the merged-away contacts at the survivor",
            ),
        ] {
            Sql::dynamic(sql_id!("contacts_app.merge_move_rows"), sql).query()
                .bind(from)
                .bind(into)
                .execute(&mut *tx)
                .await
                .context(what)?;
        }
        sql!(
            "contacts_app.merge_move_fields",
            "UPDATE fields SET contact_id = ?, position = position + ? WHERE contact_id = ?"
        )
        .query()
        .bind(into)
        .bind(after)
        .bind(from)
        .execute(&mut *tx)
        .await
        .context("move the fields")?;
        sql!(
            "contacts_app.merge_update_survivor_note",
            "UPDATE contacts SET note = ?, updated_at_utc = ?, tz_offset = ? WHERE contact_id = ?"
        )
        .query()
        .bind(merged_note(into_note, from_note))
        .bind(&now)
        .bind(&tz)
        .bind(into)
        .execute(&mut *tx)
        .await
        .context("update the survivor")?;
        tx.commit().await?;
        self.seal(&format!("contacts: merge {from_name:?} into {into_name:?}"))
            .await?;
        draft::discard(&self.pool, &draft::branch_of(from)).await
    }

    async fn seal(&self, msg: &str) -> Result<()> {
        doltlite_raw::commit_run(&self.pool, msg).await?;
        Ok(())
    }
}

/// The contacts `h` is on, by name: each a person linked it to, or
/// confirmed an agent's link to. An agent's link nobody has confirmed is
/// a suggestion, and holds nothing (docs/dev/contacts.md §"Agents
/// suggest").
async fn holders<'e>(
    conn: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    h: &Handle,
) -> Result<Vec<String>> {
    sql!(
        "contacts_app.find_handle_holders",
        "SELECT h.contact_id FROM handles h JOIN contacts c USING (contact_id) \
          WHERE h.handle = ? AND (h.linked_how = 'manual' OR h.reviewed_at_utc IS NOT NULL) \
          ORDER BY c.name COLLATE NOCASE, h.contact_id"
    )
    .query_scalar()
    .bind(h.as_str())
    .fetch_all(conn)
    .await
    .context("read who holds a handle")
}

/// Every contact with a link to `h`, suggestions included: what an agent
/// may not propose over, since it never shares a handle.
pub(crate) async fn any_holders<'e>(
    conn: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    h: &Handle,
) -> Result<Vec<String>> {
    sql!(
        "contacts_app.find_handle_holders_and_suggestions",
        "SELECT h.contact_id FROM handles h JOIN contacts c USING (contact_id) \
          WHERE h.handle = ? ORDER BY c.name COLLATE NOCASE, h.contact_id"
    )
    .query_scalar()
    .bind(h.as_str())
    .fetch_all(conn)
    .await
    .context("read who holds or is suggested for a handle")
}

/// A person put `h` on `contact_id`: an agent's suggestion of it for that
/// contact is now confirmed, and its suggestions for anyone else are
/// withdrawn, as not theirs. Returns whether the contact had it as a
/// suggestion, which is then the link.
async fn settle_suggestions(
    tx: &mut sqlx::SqliteConnection,
    h: &Handle,
    contact_id: &str,
    now: &str,
    tz: &str,
) -> Result<bool> {
    let suggested: Vec<String> = sql!(
        "contacts_app.list_suggested_for_handle",
        "SELECT contact_id FROM handles \
          WHERE handle = ? AND linked_how = 'agent' AND reviewed_at_utc IS NULL"
    )
    .query_scalar()
    .bind(h.as_str())
    .fetch_all(&mut *tx)
    .await
    .context("read who an agent suggested a handle for")?;
    let mut confirmed = false;
    for id in suggested {
        if id == contact_id {
            sql!(
                "contacts_app.confirm_suggested_link",
                "UPDATE handles SET reviewed_at_utc = ?, tz_offset = ? \
                  WHERE handle = ? AND contact_id = ?"
            )
            .query()
            .bind(now)
            .bind(tz)
            .bind(h.as_str())
            .bind(&id)
            .execute(&mut *tx)
            .await
            .context("confirm a suggested link")?;
            confirmed = true;
        } else {
            review::unlink_in(tx, h.as_str(), &id, now, tz).await?;
            review::drop_if_emptied(tx, &id, false).await?;
        }
    }
    Ok(confirmed)
}

/// Several contacts' names, for a sentence: "Robert Picard and Marie Picard".
async fn names_of(tx: &mut sqlx::SqliteConnection, contact_ids: &[String]) -> Result<String> {
    let mut names = Vec::new();
    for id in contact_ids {
        names.push(name_of(&mut *tx, id).await?);
    }
    Ok(names.join(" and "))
}

/// A contact's name, or a refusal naming the id when there is no such
/// contact.
async fn name_of_existing(conn: &mut sqlx::SqliteConnection, contact_id: &str) -> Result<String> {
    let name: Option<String> = sql!(
        "contacts_app.read_name_or_refuse",
        "SELECT name FROM contacts WHERE contact_id = ?"
    )
    .query_scalar()
    .bind(contact_id)
    .fetch_optional(&mut *conn)
    .await
    .context("read a contact's name")?;
    name.ok_or_else(|| anyhow::anyhow!("no contact {contact_id}"))
}

/// One side of a merge: its name and note. Refused for a contact
/// the store lacks or one already merged away.
async fn merge_side(
    conn: &mut sqlx::SqliteConnection,
    contact_id: &str,
) -> Result<(String, Option<String>)> {
    let row = sql!(
        "contacts_app.read_merge_side",
        "SELECT name, note, merged_into FROM contacts WHERE contact_id = ?"
    )
    .query()
    .bind(contact_id)
    .fetch_optional(&mut *conn)
    .await
    .context("read a contact to merge")?
    .ok_or_else(|| anyhow::anyhow!("no contact {contact_id}"))?;
    let name: String = row.get("name");
    if row.get::<Option<String>, _>("merged_into").is_some() {
        bail!("{name:?} was already merged into another contact");
    }
    Ok((name, row.get("note")))
}

/// The survivor's note, then the merged-away one's, each kept whole.
fn merged_note(into: Option<String>, from: Option<String>) -> Option<String> {
    let keep = |n: Option<String>| n.filter(|n| !n.trim().is_empty());
    match (keep(into), keep(from)) {
        (Some(a), Some(b)) if a.trim() == b.trim() => Some(a),
        (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
        (a, b) => a.or(b),
    }
}

pub(crate) async fn name_of(tx: &mut sqlx::SqliteConnection, contact_id: &str) -> Result<String> {
    let name: Option<String> = sql!(
        "contacts_app.read_name_for_message",
        "SELECT name FROM contacts WHERE contact_id = ?"
    )
    .query_scalar()
    .bind(contact_id)
    .fetch_optional(&mut *tx)
    .await
    .context("read a contact's name")?;
    Ok(name.unwrap_or_else(|| format!("contact {contact_id}")))
}

/// A new contact's id.
async fn insert_contact(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    name: &str,
    by: Maker<'_>,
    now: &str,
    tz: &str,
) -> Result<String> {
    let contact_id = uuid::Uuid::new_v4().to_string();
    let row = ContactRow {
        contact_id: contact_id.clone(),
        name: name.to_string(),
        note: None,
        merged_into: None,
        agent: by.agent(),
        created_at_utc: now.to_string(),
        updated_at_utc: now.to_string(),
        tz_offset: tz.to_string(),
    };
    bulk_upsert_entity_in_tx(tx, &[row])
        .await
        .context("insert a contact")?;
    Ok(contact_id)
}

/// [`Store::link`] inside `tx`, to the contact called `name`: what the
/// commit should say it did, or `None` when it was linked already.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn link_in(
    tx: &mut sqlx::SqliteConnection,
    handle: &Handle,
    contact_id: &str,
    name: &str,
    held: WhenHeld,
    from: &HandleFrom,
    now: &str,
    tz: &str,
) -> Result<Option<String>> {
    let holding = holders(&mut *tx, handle).await?;
    let what = match plan_link(&holding, contact_id, held) {
        LinkPlan::AlreadyLinked => return Ok(None),
        LinkPlan::HeldBy(others) => {
            bail!(
                "{handle} already belongs to {}",
                names_of(&mut *tx, &others).await?
            )
        }
        LinkPlan::Insert => format!("link {handle} to {name:?}"),
        LinkPlan::Share => format!(
            "link {handle} to {name:?} too, beside {}",
            names_of(&mut *tx, &holding).await?
        ),
        LinkPlan::TakeFrom(others) => {
            let from = names_of(&mut *tx, &others).await?;
            for other in &others {
                review::unlink_in(tx, handle.as_str(), other, now, tz).await?;
            }
            format!("move {handle} from {from} to {name:?}")
        }
    };
    insert_handle(tx, handle, contact_id, Maker::Person, from, now, tz).await?;
    sql!(
        "contacts_app.delete_not_linked_on_link",
        "DELETE FROM not_linked WHERE handle = ? AND contact_id = ?"
    )
    .query()
    .bind(handle.as_str())
    .bind(contact_id)
    .execute(&mut *tx)
    .await
    .context("forget a turned-down link")?;
    Ok(Some(what))
}

async fn insert_handle(
    tx: &mut sqlx::SqliteConnection,
    h: &Handle,
    contact_id: &str,
    by: Maker<'_>,
    from: &HandleFrom,
    now: &str,
    tz: &str,
) -> Result<()> {
    if by == Maker::Person && settle_suggestions(tx, h, contact_id, now, tz).await? {
        return Ok(());
    }
    let row = HandleRow {
        handle: h.as_str().to_string(),
        contact_id: contact_id.to_string(),
        linked_how: by.how(),
        linked_at_utc: now.to_string(),
        tz_offset: tz.to_string(),
        stopped_working_by: None,
        agent: by.agent(),
        reason: by.reason(),
        reviewed_at_utc: None,
        confidence: by.confidence(),
        run: by.run(),
        label: from.label(),
        copied_from_source: from.copied_from.as_ref().map(|c| c.source_id.clone()),
        copied_from_key: from.copied_from.as_ref().map(|c| c.key.clone()),
    };
    // `handles` has a two-column key, which `PortableTable` derives no
    // upsert for.
    sql!(
        "contacts_app.insert_handle",
        "INSERT OR REPLACE INTO handles (handle, contact_id, linked_how, linked_at_utc, \
           tz_offset, stopped_working_by, agent, reason, reviewed_at_utc, confidence, run, \
           label, copied_from_source, copied_from_key) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .query()
    .bind(&row.handle)
    .bind(&row.contact_id)
    .bind(row.linked_how.as_str())
    .bind(&row.linked_at_utc)
    .bind(&row.tz_offset)
    .bind(&row.stopped_working_by)
    .bind(&row.agent)
    .bind(&row.reason)
    .bind(&row.reviewed_at_utc)
    .bind(row.confidence)
    .bind(&row.run)
    .bind(&row.label)
    .bind(&row.copied_from_source)
    .bind(&row.copied_from_key)
    .execute(&mut *tx)
    .await
    .context("link a handle")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use datalib_sql::{test_sql, test_sql_id};
    fn email(s: &str) -> Handle {
        Handle::email(s).unwrap()
    }

    #[test]
    fn a_handle_is_linked_once_and_never_taken_silently() {
        let held = |ids: &[&str]| -> Vec<String> { ids.iter().map(|s| s.to_string()).collect() };
        for when in [WhenHeld::Refuse, WhenHeld::Share, WhenHeld::Move] {
            assert_eq!(plan_link(&[], "a", when), LinkPlan::Insert);
            assert_eq!(
                plan_link(&held(&["b", "a"]), "a", when),
                LinkPlan::AlreadyLinked
            );
        }
        let b = held(&["b"]);
        assert_eq!(
            plan_link(&b, "a", WhenHeld::Refuse),
            LinkPlan::HeldBy(b.clone())
        );
        assert_eq!(plan_link(&b, "a", WhenHeld::Share), LinkPlan::Share);
        assert_eq!(plan_link(&b, "a", WhenHeld::Move), LinkPlan::TakeFrom(b));
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
                    contact: "troi".into(),
                },
                Rebuilt::Duplicate {
                    from: "email:mailto:riker@enterprise.org".into(),
                    contact: "riker".into(),
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
                    contact: "data".into(),
                },
                Rebuilt::HeldElsewhere {
                    from: "email:mailto:Data@enterprise.org".into(),
                    to: "email:data@enterprise.org".into(),
                    holder: "data".into(),
                },
            ],
            "two old spellings of one new handle: the first takes it"
        );
        assert_eq!(
            plan_rebuild(&links(&[
                ("email:mailto:vineyard@labarre.earth", "marie"),
                ("email:mailto:vineyard@labarre.earth", "robert"),
            ])),
            vec![
                Rebuilt::Respell {
                    from: "email:mailto:vineyard@labarre.earth".into(),
                    to: "email:vineyard@labarre.earth".into(),
                    contact: "marie".into(),
                },
                Rebuilt::HeldElsewhere {
                    from: "email:mailto:vineyard@labarre.earth".into(),
                    to: "email:vineyard@labarre.earth".into(),
                    holder: "marie".into(),
                },
            ],
            "a shared handle is respelled for one of them and kept as written for the other"
        );
    }

    /// The mark answers as `account` on the contact and in the list,
    /// follows a merge to the survivor, refuses a group, and goes with a
    /// deleted contact.
    #[tokio::test]
    async fn a_contact_marked_as_me_answers_as_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let picard = store
            .create("Jean-Luc Picard", &[email("jlp@e.org")])
            .await
            .unwrap();
        let old = store
            .create("J. Picard", &[email("old@e.org")])
            .await
            .unwrap();
        let is_me = |id: String| {
            let store = &store;
            async move { store.contact(&id).await.unwrap().unwrap().account }
        };

        assert!(!is_me(picard.clone()).await);
        assert!(store.set_me(&old, true).await.unwrap());
        assert!(!store.set_me(&old, true).await.unwrap(), "already marked");
        assert!(is_me(old.clone()).await);
        let listed: Vec<(String, bool)> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.names[0].clone(), c.account))
            .collect();
        assert_eq!(
            listed,
            [
                ("J. Picard".to_string(), true),
                ("Jean-Luc Picard".to_string(), false),
            ]
        );

        store.merge(&old, &picard).await.unwrap();
        assert!(is_me(picard.clone()).await, "the mark follows the merge");
        assert!(
            store.set_me(&old, false).await.unwrap(),
            "the merged-away id unmarks its survivor"
        );
        assert!(!is_me(picard.clone()).await);

        store.set_me(&picard, true).await.unwrap();
        store.delete(&picard).await.unwrap();
        let left: i64 = test_sql!("SELECT COUNT(*) FROM me")
            .query_scalar()
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "a deleted contact's mark goes with it");
        store.close().await;
    }

    /// A store from before the ladder (schema version 0), holding links
    /// an older build's rules spelled, is respelled on open; the links
    /// the rules no longer read are still on their contact.
    #[tokio::test]
    async fn a_store_from_before_the_ladder_is_respelled_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        let store = Store::open(&path).await.unwrap();
        let picard = store.create("Jean-Luc Picard", &[]).await.unwrap();
        for old in ["email:mailto:picard@enterprise.org", "tel:+1123456"] {
            test_sql!(
                "INSERT INTO handles (handle, contact_id, linked_how, linked_at_utc, tz_offset) \
                 VALUES (?, ?, 'manual', '2364-03-01T09:00:00.000000Z', '+00:00')"
            )
            .query()
            .bind(old)
            .bind(&picard)
            .execute(&store.pool)
            .await
            .unwrap();
        }
        back_to_tables_before_rung_4(&store.pool).await;
        test_sql!("UPDATE _datalib_meta SET value = '0' WHERE key = 'schema_version'")
            .query()
            .execute(&store.pool)
            .await
            .unwrap();
        store.seal("an older build's links").await.unwrap();
        store.close().await;

        let store = Store::open(&path).await.unwrap();
        let email = email("picard@enterprise.org");
        let got = store.resolve(std::slice::from_ref(&email)).await.unwrap();
        assert_eq!(
            got[email.as_str()][0].key,
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

    /// Handle rules 2 refuse a bidi control or an invisible character.
    /// A link a person made to such a handle under rules 1 stays on its
    /// contact, shown as written with no handle, rather than vanishing.
    #[tokio::test]
    async fn a_link_to_a_handle_that_hides_a_character_stays_on_its_contact() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        let store = Store::open(&path).await.unwrap();
        let riker = email("riker@enterprise.org");
        let troi = store
            .create("Deanna Troi", std::slice::from_ref(&riker))
            .await
            .unwrap();
        let hidden = "email:riker\u{202E}@enterprise.org";
        test_sql!(
            "INSERT INTO handles (handle, contact_id, linked_how, linked_at_utc, tz_offset) \
             VALUES (?, ?, 'manual', '2364-03-01T09:00:00.000000Z', '+00:00')"
        )
        .query()
        .bind(hidden)
        .bind(&troi)
        .execute(&store.pool)
        .await
        .unwrap();
        back_to_tables_before_rung_4(&store.pool).await;
        test_sql!("UPDATE _datalib_meta SET value = '1' WHERE key = 'schema_version'")
            .query()
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .seal("a link made under handle rules 1")
            .await
            .unwrap();
        store.close().await;

        let store = Store::open(&path).await.unwrap();
        let got = store.resolve(std::slice::from_ref(&riker)).await.unwrap();
        assert_eq!(
            got[riker.as_str()][0].key,
            troi,
            "the clean link still resolves"
        );
        let c = store.contact(&troi).await.unwrap().unwrap();
        let shown: Vec<(String, Option<Handle>)> = c
            .handles
            .iter()
            .map(|h| (h.value.clone(), h.handle.clone()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("riker@enterprise.org".to_string(), Some(riker)),
                (hidden.to_string(), None),
            ],
            "the link the rules no longer read is shown as written, not dropped"
        );
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
        // Rung 3 rekeyed `handles`, so these tables are from before it.
        let pool =
            doltlite_raw::open_curated(&path, HAND_WRITTEN_DDL, StoreKind::Contacts, &LADDER[..2])
                .await
                .unwrap();
        // A row in every table: an empty table is rebuilt whatever its
        // shape, which would hide a break.
        for sql in [
            test_sql!(
                "INSERT INTO contacts VALUES ('riker', 'person', 'William Riker', 'x', NULL, \
             '2364-03-01T09:00:00Z', '2364-03-01T09:00:00Z', '+00:00')"
            ),
            test_sql!(
                "INSERT INTO contacts VALUES ('away', 'group', 'Away team', NULL, NULL, \
             '2364-03-01T09:00:00Z', '2364-03-01T09:00:00Z', '+00:00')"
            ),
            test_sql!(
                "INSERT INTO handles VALUES ('email:riker@enterprise.org', 'riker', 'manual', \
             '2364-03-01T09:00:00Z', '+00:00', NULL)"
            ),
            test_sql!(
                "INSERT INTO photos VALUES ('riker', 'image/png', x'89504e47', \
             '2364-03-01T09:00:00Z', '+00:00')"
            ),
        ] {
            sql.query().execute(&pool).await.unwrap();
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

    /// `handles` as rung 2 left it: one contact per handle.
    const HANDLES_BEFORE_RUNG_3: &str = "CREATE TABLE IF NOT EXISTS handles (
    handle TEXT PRIMARY KEY,
    contact_id TEXT NOT NULL,
    linked_how TEXT NOT NULL,
    linked_at_utc TEXT NOT NULL,
    tz_offset TEXT NOT NULL,
    stopped_working_by TEXT,
    agent TEXT,
    reason TEXT,
    reviewed_at_utc TEXT,
    confidence REAL,
    run TEXT
)";

    /// Give a store this build made the tables every rung before 4 found,
    /// for a test that sets its version back to replay the ladder.
    async fn back_to_tables_before_rung_4(pool: &SqlitePool) {
        for sql in [
            test_sql!("ALTER TABLE contacts ADD COLUMN kind TEXT NOT NULL DEFAULT 'person'"),
            Sql::dynamic(test_sql_id!(), MEMBERS_BEFORE_RUNG_4),
        ] {
            sql.query().execute(pool).await.unwrap();
        }
    }

    /// `contacts` and `members` as every rung before 4 left them: a
    /// contact was a person or a group, and a group had members.
    const CONTACTS_BEFORE_RUNG_4: &str = "CREATE TABLE IF NOT EXISTS contacts (
    contact_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    note TEXT,
    merged_into TEXT,
    agent TEXT,
    created_at_utc TEXT NOT NULL,
    updated_at_utc TEXT NOT NULL,
    tz_offset TEXT NOT NULL,
    PRIMARY KEY (contact_id)
)";
    const MEMBERS_BEFORE_RUNG_4: &str = "CREATE TABLE IF NOT EXISTS members (
    group_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    added_at_utc TEXT NOT NULL,
    tz_offset TEXT NOT NULL,
    PRIMARY KEY (group_id, member_id)
)";

    /// This build's tables as they were before rung `rung`, which is 3 or
    /// 4.
    fn ddl_before(rung: u32) -> Vec<&'static str> {
        let mut out: Vec<&str> = DDL
            .iter()
            .map(|d| {
                if rung == 3 && d.starts_with("CREATE TABLE IF NOT EXISTS handles ") {
                    HANDLES_BEFORE_RUNG_3
                } else if d.starts_with("CREATE TABLE IF NOT EXISTS contacts ") {
                    CONTACTS_BEFORE_RUNG_4
                } else {
                    d
                }
            })
            .collect();
        out.insert(1, MEMBERS_BEFORE_RUNG_4);
        out
    }

    /// A store as rung 3 left it, with `rows` in it, committed.
    async fn store_at_rung_3(dir: &Path, rows: &[&'static str]) -> PathBuf {
        let path = store_path(dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let pool =
            doltlite_raw::open_curated(&path, &ddl_before(4), StoreKind::Contacts, &LADDER[..3])
                .await
                .unwrap();
        for row in rows {
            Sql::dynamic(sql_id!("contacts_app.test.fill_rung_3"), *row)
                .query()
                .execute(&pool)
                .await
                .unwrap();
        }
        doltlite_raw::commit_run(&pool, "a store at rung 3")
            .await
            .unwrap();
        pool.close().await;
        path
    }

    const AWAY_TEAM: &str = "INSERT INTO contacts (contact_id, kind, name, created_at_utc, \
        updated_at_utc, tz_offset) VALUES ('away', 'group', 'Away team', '2364-03-01T09:00:00Z', \
        '2364-03-01T09:00:00Z', '+00:00'), ('riker', 'person', 'Will Riker', \
        '2364-03-01T09:00:00Z', '2364-03-01T09:00:00Z', '+00:00'), ('troi', 'person', \
        'Deanna Troi', '2364-03-01T09:00:00Z', '2364-03-01T09:00:00Z', '+00:00')";
    const AWAY_MEMBERS: &str = "INSERT INTO members VALUES \
        ('away', 'riker', '2364-03-02T09:00:00Z', '+00:00'), \
        ('away', 'troi', '2364-03-02T09:00:00Z', '+00:00')";
    const TROI_IS_CREW: &str = "INSERT INTO labels (subject, label, applies, decided_how, \
        decided_at_utc, reviewed_at_utc, tz_offset) VALUES ('contact:troi', 'away team', 1, \
        'manual', '2364-03-01T09:00:00Z', '2364-03-01T09:00:00Z', '+00:00')";

    /// Rung 4: a group is an ordinary contact, and its members carry its
    /// name as a label of their own, in the spelling a label of that name
    /// already has ("away team" for the group "Away team"), which a member
    /// that had it already keeps as it was; `members` goes.
    #[tokio::test]
    async fn rung_4_makes_a_group_a_contact_and_its_members_a_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_at_rung_3(dir.path(), &[AWAY_TEAM, AWAY_MEMBERS, TROI_IS_CREW]).await;
        let store = Store::open(&path).await.unwrap();
        let names: Vec<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|c| format!("{} {:?}", c.names[0], c.kind))
            .collect();
        assert_eq!(
            names,
            [
                "Away team Person",
                "Deanna Troi Person",
                "Will Riker Person"
            ]
        );
        let labels: Vec<String> = test_sql!(
            "SELECT subject || ' ' || label || ' ' || decided_how || ' ' || decided_at_utc \
               FROM labels ORDER BY 1"
        )
        .query_scalar()
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            labels,
            [
                "contact:riker away team manual 2364-03-02T09:00:00Z",
                "contact:troi away team manual 2364-03-01T09:00:00Z",
            ]
        );
        let tables: Vec<String> =
            test_sql!("SELECT name FROM sqlite_master WHERE name = 'members'")
                .query_scalar()
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert!(tables.is_empty(), "members is gone");
        store.close().await;
    }

    /// Rung 3 rekeys the links by handle and contact. Every link comes
    /// through with all it said, a handle can then be shared, and a draft
    /// cut before the upgrade still saves.
    #[tokio::test]
    async fn rung_3_keeps_every_link_and_lets_a_handle_be_shared() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let before = ddl_before(3);
        let pool = doltlite_raw::open_curated(&path, &before, StoreKind::Contacts, &LADDER[..2])
            .await
            .unwrap();
        for sql in [
            test_sql!(
                "INSERT INTO contacts (contact_id, kind, name, created_at_utc, updated_at_utc, \
                   tz_offset) VALUES ('robert', 'person', 'Robert Picard', '2364-03-01T09:00:00Z', \
                   '2364-03-01T09:00:00Z', '+00:00'), ('marie', 'person', 'Marie Picard', \
                   '2364-03-01T09:00:00Z', '2364-03-01T09:00:00Z', '+00:00')"
            ),
            test_sql!(
                "INSERT INTO handles VALUES \
                 ('email:vineyard@labarre.earth', 'robert', 'manual', '2364-03-01T09:00:00Z', \
                   '+00:00', '2370', NULL, NULL, NULL, NULL, NULL), \
                 ('email:marie@labarre.earth', 'marie', 'agent', '2364-03-01T09:00:00Z', \
                   '+00:00', NULL, 'claude-test', 'signs as Marie', NULL, 0.8, 'run-1')"
            ),
        ] {
            sql.query().execute(&pool).await.unwrap();
        }
        doltlite_raw::commit_run(&pool, "a store at rung 2")
            .await
            .unwrap();
        let marie_draft = draft::branch_of("marie");
        draft::cut(&pool, &marie_draft).await.unwrap();
        let mut on = draft::OnDraft::enter(&pool, &marie_draft).await.unwrap();
        test_sql!("UPDATE contacts SET note = 'runs the vineyard' WHERE contact_id = 'marie'")
            .query()
            .execute(on.conn())
            .await
            .unwrap();
        on.leave().await.unwrap();
        pool.close().await;

        let store = Store::open(&path).await.unwrap();
        let meta = datalib_store_meta::read(&store.pool)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.schema_version as usize, LADDER.len());
        let vineyard = email("vineyard@labarre.earth");
        let robert = store.contact("robert").await.unwrap().unwrap();
        assert_eq!(robert.handles[0].handle.as_ref(), Some(&vineyard));
        assert_eq!(
            robert.handles[0].stopped_working_by.as_deref(),
            Some("2370")
        );
        let agent_link = test_sql!(
            "SELECT handle || ' ' || reason || ' ' || confidence || ' ' || run \
               FROM handles WHERE agent = 'claude-test'"
        )
        .query_scalar::<String>()
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            agent_link,
            ["email:marie@labarre.earth signs as Marie 0.8 run-1"]
        );

        store
            .link(&vineyard, "marie", WhenHeld::Share, &HandleFrom::default())
            .await
            .unwrap();
        let got = store
            .resolve(std::slice::from_ref(&vineyard))
            .await
            .unwrap();
        let names: Vec<&str> = got[vineyard.as_str()]
            .iter()
            .filter_map(|c| c.name())
            .collect();
        assert_eq!(names, ["Marie Picard", "Robert Picard"]);

        let seen = store
            .draft_view("marie")
            .await
            .unwrap()
            .unwrap()
            .published_commit;
        let saved = store
            .save("marie", &seen, &Default::default())
            .await
            .unwrap();
        assert!(matches!(saved, drafts::Saved::Saved { .. }), "{saved:?}");
        let marie = store.contact("marie").await.unwrap().unwrap();
        assert_eq!(marie.note.as_deref(), Some("runs the vineyard"));
        let held: Vec<_> = marie
            .handles
            .iter()
            .filter_map(|h| h.handle.as_ref())
            .collect();
        assert_eq!(
            held,
            [&vineyard],
            "the shared link survived the save; the agent's address is a suggestion"
        );
        store.close().await;
    }

    /// An address two people share: each has it as their own, so one of
    /// them stopping using it, or being told it is not theirs, leaves the
    /// other's alone; moving it takes it off the others as not theirs; and
    /// merging two who share it keeps one link.
    #[tokio::test]
    async fn a_shared_handle_is_each_ones_own() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let vineyard = email("vineyard@labarre.earth");
        let one = std::slice::from_ref(&vineyard);
        let robert = store.create("Robert Picard", one).await.unwrap();
        let marie = store.create("Marie Picard", &[]).await.unwrap();
        let err = store
            .link(&vineyard, &marie, WhenHeld::Refuse, &HandleFrom::default())
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .ends_with("already belongs to Robert Picard"),
            "{err}"
        );
        store
            .link(&vineyard, &marie, WhenHeld::Share, &HandleFrom::default())
            .await
            .unwrap();
        let holders = |got: HashMap<String, Vec<NormalizedContact>>| -> Vec<String> {
            got.get(vineyard.as_str())
                .map(|cs| cs.iter().map(|c| c.key.clone()).collect())
                .unwrap_or_default()
        };
        assert_eq!(
            holders(store.resolve(one).await.unwrap()),
            vec![marie.clone(), robert.clone()]
        );

        assert!(store
            .set_stopped_working(&vineyard, &robert, Some("2370"))
            .await
            .unwrap());
        let marie_now = store.contact(&marie).await.unwrap().unwrap();
        assert_eq!(marie_now.handles[0].stopped_working_by, None);

        assert!(store.unlink(&vineyard, &marie).await.unwrap());
        assert_eq!(
            holders(store.resolve(one).await.unwrap()),
            vec![robert.clone()]
        );

        let beverly = store.create("Beverly Crusher", &[]).await.unwrap();
        store
            .link(&vineyard, &beverly, WhenHeld::Move, &HandleFrom::default())
            .await
            .unwrap();
        assert_eq!(
            holders(store.resolve(one).await.unwrap()),
            vec![beverly.clone()]
        );
        let down: Vec<String> = store
            .turned_down()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.contact_id)
            .collect();
        assert!(
            down.contains(&robert) && down.contains(&marie),
            "moved off Robert and taken off Marie: {down:?}"
        );

        store
            .link(&vineyard, &marie, WhenHeld::Share, &HandleFrom::default())
            .await
            .unwrap();
        store.merge(&marie, &beverly).await.unwrap();
        assert_eq!(
            holders(store.resolve(one).await.unwrap()),
            vec![beverly.clone()]
        );
        let survivor = store.contact(&beverly).await.unwrap().unwrap();
        assert_eq!(survivor.handles.len(), 1);
        store.close().await;
    }

    #[test]
    fn strum_spellings_round_trip() {
        for k in DecidedHow::VARIANTS {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
            assert_eq!(DecidedHow::parse(k.as_str()), Some(*k));
        }
        for k in WhenHeld::VARIANTS {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
            assert_eq!(WhenHeld::parse(k.as_str()), Some(*k));
        }
    }

    /// The contacts app's own table: every contact by name whatever its
    /// case, each with its links (working first) and its photo, as one
    /// contact reads alone.
    #[tokio::test]
    async fn list_answers_every_contact_as_contact_does() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let riker = email("riker@enterprise.org");
        let tel = Handle::tel("+15550123456").unwrap();
        let will = store
            .create("Will Riker", std::slice::from_ref(&riker))
            .await
            .unwrap();
        store
            .link(&tel, &will, WhenHeld::Refuse, &HandleFrom::default())
            .await
            .unwrap();
        store
            .set_stopped_working(&riker, &will, Some("2369"))
            .await
            .unwrap();
        let data = store.create("data", &[]).await.unwrap();
        let data_photo = store
            .set_photo(&data, "image/png", b"\x89PNG\r\n\x1a\n")
            .await
            .unwrap();

        let listed = store.list().await.unwrap();
        let names: Vec<&str> = listed.iter().filter_map(|c| c.name()).collect();
        assert_eq!(names, ["data", "Will Riker"]);
        assert_eq!(listed[0].photo_url, Some(data_photo));
        let handles: Vec<Option<&str>> = listed[1]
            .handles
            .iter()
            .map(|h| h.handle.as_ref().map(Handle::as_str))
            .collect();
        assert_eq!(
            handles,
            [Some(tel.as_str()), Some(riker.as_str())],
            "working first"
        );
        for c in &listed {
            assert_eq!(Some(c), store.contact(&c.key).await.unwrap().as_ref());
        }
        store.close().await;
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
            .create("Will Riker", std::slice::from_ref(&riker))
            .await
            .unwrap();
        store
            .link(&tel, &id, WhenHeld::Refuse, &HandleFrom::default())
            .await
            .unwrap();
        store
            .link(&tel, &id, WhenHeld::Refuse, &HandleFrom::default())
            .await
            .unwrap();

        let got = store.resolve(&[riker.clone(), tel.clone()]).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[riker.as_str()][0].name(), Some("Will Riker"));
        assert_eq!(got[tel.as_str()][0].key, id);
        assert_eq!(got[tel.as_str()][0].source_id, SOURCE_ID);

        let other = store.create("Thomas Riker", &[]).await.unwrap();
        let err = store
            .link(&tel, &other, WhenHeld::Refuse, &HandleFrom::default())
            .await
            .unwrap_err();
        assert!(
            err.to_string().ends_with("already belongs to Will Riker"),
            "{err}"
        );
        let err = store
            .create("Twin", std::slice::from_ref(&riker))
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
            .set_stopped_working(&tel, &id, Some("2019-06"))
            .await
            .unwrap());
        let c = store.contact(&id).await.unwrap().unwrap();
        assert_eq!(
            c.handles[0].handle.as_ref(),
            Some(&riker),
            "working handles first"
        );
        assert_eq!(c.handles[1].stopped_working_by.as_deref(), Some("2019-06"));

        assert!(store.unlink(&tel, &id).await.unwrap());
        assert!(!store.unlink(&tel, &id).await.unwrap());
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
        let id = store.create("Will Riker", &[]).await.unwrap();
        assert_eq!(store.contact(&id).await.unwrap().unwrap().photo_url, None);
        assert!(store.photo(&id).await.unwrap().is_none());
        let png = b"\x89PNG\r\n\x1a\n not really".to_vec();
        let first = store.set_photo(&id, "image/png", &png).await.unwrap();
        assert!(
            first.starts_with(&format!("/applet/datalib_contacts/photo/{id}?v=")),
            "{first}"
        );
        assert_eq!(
            store.photo(&id).await.unwrap(),
            Some(("image/png".to_string(), png.clone()))
        );
        let c = store.contact(&id).await.unwrap().unwrap();
        assert_eq!(c.photo_url.as_deref(), Some(first.as_str()));
        let riker = email("riker@enterprise.org");
        store
            .link(&riker, &id, WhenHeld::Refuse, &HandleFrom::default())
            .await
            .unwrap();
        let got = store.resolve(std::slice::from_ref(&riker)).await.unwrap();
        assert_eq!(
            got[riker.as_str()][0].photo_url,
            c.photo_url,
            "resolve answers it too"
        );

        let replaced = store
            .set_photo(&id, "image/jpeg", b"\xff\xd8 replaced")
            .await
            .unwrap();
        assert_eq!(store.photo(&id).await.unwrap().unwrap().0, "image/jpeg");
        assert_ne!(
            replaced, first,
            "a new photo is a new URL, or a page keeps drawing the old one"
        );
        assert_eq!(
            store.contact(&id).await.unwrap().unwrap().photo_url,
            Some(replaced)
        );
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
        let log: Vec<String> = test_sql!("SELECT message FROM dolt_log() LIMIT 4")
            .query_scalar()
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

    /// Two contacts made for one person by mistake become one: the
    /// survivor takes the other's links, fields, photo, note and
    /// memberships, and the other's id still answers as the survivor.
    #[tokio::test]
    async fn a_merge_moves_everything_onto_the_survivor_and_keeps_the_old_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let work = email("riker@enterprise.org");
        let home = email("will@risa.test");
        let keep = store
            .create("Will Riker", std::slice::from_ref(&work))
            .await
            .unwrap();
        let dup = store
            .create("Will Riker", std::slice::from_ref(&home))
            .await
            .unwrap();
        let older = store.create("W. Riker", &[]).await.unwrap();
        store.merge(&older, &dup).await.unwrap();
        let dup_photo = store
            .set_photo(&dup, "image/png", b"\x89PNG\r\n\x1a\n")
            .await
            .unwrap();
        test_sql!("UPDATE contacts SET note = 'trombone' WHERE contact_id = ?")
            .query()
            .bind(&dup)
            .execute(&store.pool)
            .await
            .unwrap();

        store.merge(&dup, &keep).await.unwrap();

        let got = store.resolve(&[work.clone(), home.clone()]).await.unwrap();
        assert_eq!(got[home.as_str()][0].key, keep, "the link moved");
        let survivor = store.contact(&keep).await.unwrap().unwrap();
        assert_eq!(survivor.handles.len(), 2);
        assert_eq!(survivor.note.as_deref(), Some("trombone"));
        assert_eq!(
            survivor.photo_url,
            Some(dup_photo.replace(&dup, &keep)),
            "the photo moved, set when it was"
        );
        for old in [&dup, &older] {
            assert_eq!(
                store.contact(old).await.unwrap().unwrap().key,
                keep,
                "an old id answers as the survivor"
            );
        }
        let listed: Vec<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.key)
            .collect();
        assert_eq!(listed, std::slice::from_ref(&keep));
        let chain: Vec<String> =
            test_sql!("SELECT merged_into FROM contacts WHERE merged_into IS NOT NULL")
                .query_scalar()
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            chain,
            [keep.clone(), keep.clone()],
            "never more than one step"
        );

        for (from, into, says) in [
            (&keep, &keep, "into itself"),
            (&dup, &keep, "already merged"),
        ] {
            let err = store.merge(from, into).await.unwrap_err().to_string();
            assert!(err.contains(says), "{err}");
        }
        store.close().await;
    }

    /// Deleting a contact takes everything of it, and of the contacts
    /// merged into it, and leaves its handles nobody's.
    #[tokio::test]
    async fn a_deleted_contact_leaves_its_handles_unlinked() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let tel = Handle::tel("+15550123456").unwrap();
        let keep = store.create("Deanna Troi", &[]).await.unwrap();
        let dup = store
            .create("Deanna Troi", std::slice::from_ref(&tel))
            .await
            .unwrap();
        let older = store.create("D. Troi", &[]).await.unwrap();
        store.merge(&older, &dup).await.unwrap();
        store
            .set_photo(&dup, "image/png", b"\x89PNG\r\n\x1a\n")
            .await
            .unwrap();
        store.draft(&dup).await.unwrap();

        store.delete(&dup).await.unwrap();

        assert!(store
            .resolve(std::slice::from_ref(&tel))
            .await
            .unwrap()
            .is_empty());
        assert!(store.contact(&dup).await.unwrap().is_none());
        assert!(store.contact(&older).await.unwrap().is_none());
        assert!(store.photo(&dup).await.unwrap().is_none());
        assert!(
            store.draft_view(&dup).await.unwrap().is_none(),
            "its draft went too"
        );
        let listed: Vec<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.key)
            .collect();
        assert_eq!(listed, [keep]);
        let err = store.delete(&dup).await.unwrap_err().to_string();
        assert!(err.contains("no contact"), "{err}");
        store
            .link(&tel, &listed[0], WhenHeld::Refuse, &HandleFrom::default())
            .await
            .unwrap();
        store.close().await;
    }

    #[test]
    fn a_merged_note_keeps_both_once() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(merged_note(s("a"), s("b")), s("a\n\nb"));
        assert_eq!(merged_note(s("a"), s(" a ")), s("a"));
        assert_eq!(merged_note(None, s("b")), s("b"));
        assert_eq!(merged_note(s(" "), None), None);
    }

    /// A reader sees every edit as soon as it returns: each write is
    /// sealed onto `main`, not left on the writer's branch.
    #[tokio::test]
    async fn every_edit_reaches_main() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        let store = Store::open(&path).await.unwrap();
        store
            .create("Deanna Troi", &[email("troi@enterprise.org")])
            .await
            .unwrap();
        let reader = doltlite_raw::open_reader(&path, None)
            .await
            .unwrap()
            .unwrap();
        let n: i64 = test_sql!("SELECT count(*) FROM handles")
            .query_scalar()
            .fetch_one(reader.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);
        reader.close().await;
        store.close().await;
    }
}
