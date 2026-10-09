//! Raw-store schema for the CardDAV (contacts) provider.

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::{self as dr, Migration, WirePayload, WirePayloadRow};
use datalib_etl_macros::{RawTable, WirePayloadRow};
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;
use uuid::Uuid;

use super::api::{split_vcards, vcard_categories, vcard_is_group, vcard_members};

pub const DATA_TABLES: &[&str] = &["accounts", "addressbooks", "contacts"];

// accounts

/// `accounts` — one row per configured CardDAV server.
pub const ACCOUNTS_DDL: &str = "CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    server_url TEXT NULL,
    principal_href TEXT NULL,
    addressbook_home_set TEXT NULL
)";

#[derive(Debug, Clone, Default)]
pub struct AccountRow {
    pub id: String,
    pub server_url: Option<String>,
    pub principal_href: Option<String>,
    pub addressbook_home_set: Option<String>,
}

impl BulkUpsertable for AccountRow {
    const TABLE: &'static str = "accounts";
    const TYPED_COLUMNS: &'static [&'static str] =
        &["server_url", "principal_href", "addressbook_home_set"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id)
            .bind(self.server_url.as_deref())
            .bind(self.principal_href.as_deref())
            .bind(self.addressbook_home_set.as_deref())
    }
}

// addressbooks

/// `addressbooks` — one row per CardDAV addressbook collection
/// discovered under an account's home-set.
///
/// PK choice: `"<account_id>!<href>"`. The CardDAV href (e.g.
/// `/dav/addressbooks/user/default/`) is stable per server and known
/// before the first detail fetch.
pub const ADDRESSBOOKS_DDL: &str = "CREATE TABLE IF NOT EXISTS addressbooks (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    href TEXT NOT NULL,
    display_name TEXT NULL,
    description TEXT NULL,
    ctag TEXT NULL,
    sync_token TEXT NULL
)";

pub const ADDRESSBOOKS_BY_ACCOUNT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS addressbooks_by_account ON addressbooks(account_id)";

#[derive(Debug, Clone, Default)]
pub struct AddressbookRow {
    pub id: String,
    pub account_id: String,
    pub href: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub ctag: Option<String>,
}

impl BulkUpsertable for AddressbookRow {
    const TABLE: &'static str = "addressbooks";
    // `sync_token` is the listing's (`RawDb::set_sync_token`), so the
    // upsert leaves it alone.
    const TYPED_COLUMNS: &'static [&'static str] =
        &["account_id", "href", "display_name", "description", "ctag"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id)
            .bind(&self.account_id)
            .bind(&self.href)
            .bind(self.display_name.as_deref())
            .bind(self.description.as_deref())
            .bind(self.ctag.as_deref())
    }
}

/// PK recipe: `"{account_id}!{href}"`.
pub fn addressbook_pk(account_id: &str, href: &str) -> String {
    format!("{account_id}!{href}")
}

// contacts

/// `contacts` — one row per vCard, keyed `"<addressbook_id>#<UID>"`
/// (RFC 6350 mandates a non-empty `UID:`). A card from a server that
/// has none is held in `dav_resources` with a warning and no row here;
/// a `.vcf` file's card gets a synthesized one.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "contacts")]
pub struct ContactRow {
    pub id_and_payload: WirePayload,
    pub addressbook_id: String,
    pub uid: String,
    pub href: String,
    pub etag: Option<String>,
    pub display_name: Option<String>,
    pub revision: Option<String>,
}

impl ContactRow {
    pub fn new(
        addressbook_id: String,
        uid: String,
        href: String,
        etag: Option<String>,
        display_name: Option<String>,
        revision: Option<String>,
        vcard_text: &str,
    ) -> Self {
        let envelope = serde_json::json!({ "vcard": vcard_text });
        Self {
            id_and_payload: WirePayload {
                id: contact_pk(&addressbook_id, &uid),
                payload: envelope.to_string(),
            },
            addressbook_id,
            uid,
            href,
            etag,
            display_name,
            revision,
        }
    }
}

/// PK recipe: `"{addressbook_id}#{uid}"`.
pub fn contact_pk(addressbook_id: &str, uid: &str) -> String {
    format!("{addressbook_id}#{uid}")
}

impl ContactRow {
    /// The vCard text the row holds: one card, or for a row a `.vcf`
    /// file made, possibly several.
    pub fn vcard(&self) -> String {
        serde_json::from_str::<serde_json::Value>(&self.id_and_payload.payload)
            .ok()
            .and_then(|v| v.get("vcard")?.as_str().map(str::to_string))
            .unwrap_or_default()
    }
}

// contact_group_members

/// `contact_group_members` — one row per member a group card names.
/// Derived from the group's `contacts` row on every write of it, so a
/// member added or dropped is a row added or dropped in
/// `dolt_diff_contact_group_members` rather than an edit inside a vCard.
/// Synthesized `id` PK (`"{group_id}#{member}"`), like email's join
/// tables.
#[derive(Debug, Clone, PartialEq, Eq, RawTable)]
#[raw_table(
    table = "contact_group_members",
    index = "contact_group_members_by_group:group_id",
    index = "contact_group_members_by_member:member_id"
)]
pub struct GroupMemberRow {
    pub id: String,
    /// The `contacts` row that holds the group card.
    pub group_id: String,
    pub addressbook_id: String,
    /// The member as the card writes it: `urn:uuid:<UID>` from Apple and
    /// Fastmail, sometimes a `mailto:` in vCard 4.
    pub member: String,
    /// The `contacts` row a member naming a UID refers to, whether or not
    /// that row is here. `None` for a member named any other way.
    pub member_id: Option<String>,
}

impl GroupMemberRow {
    /// Every membership the group cards in `row` name.
    pub fn for_contact(row: &ContactRow) -> Vec<Self> {
        Self::from_vcard(&row.id_and_payload.id, &row.addressbook_id, &row.vcard())
    }

    pub fn from_vcard(group_id: &str, addressbook_id: &str, vcard: &str) -> Vec<Self> {
        let mut blocks = split_vcards(vcard);
        if blocks.is_empty() {
            blocks.push(vcard.to_string());
        }
        let mut out: Vec<Self> = blocks
            .iter()
            .filter(|b| vcard_is_group(b))
            .flat_map(|b| vcard_members(b))
            .map(|member| Self {
                id: format!("{group_id}#{member}"),
                group_id: group_id.to_string(),
                addressbook_id: addressbook_id.to_string(),
                member_id: member_uid(&member).map(|uid| contact_pk(addressbook_id, uid)),
                member,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }
}

// contact_categories

/// `contact_categories` — one row per name in a card's `CATEGORIES`, the
/// way Google's export files a contact under its labels (`myContacts`,
/// `starred`, and the ones a person made). Stored as written; derived
/// from the card on every write of it, like [`GroupMemberRow`].
#[derive(Debug, Clone, PartialEq, Eq, RawTable)]
#[raw_table(
    table = "contact_categories",
    index = "contact_categories_by_contact:contact_id",
    index = "contact_categories_by_category:category"
)]
pub struct ContactCategoryRow {
    pub id: String,
    pub contact_id: String,
    pub addressbook_id: String,
    pub category: String,
}

impl ContactCategoryRow {
    pub fn for_contact(row: &ContactRow) -> Vec<Self> {
        Self::from_vcard(&row.id_and_payload.id, &row.addressbook_id, &row.vcard())
    }

    pub fn from_vcard(contact_id: &str, addressbook_id: &str, vcard: &str) -> Vec<Self> {
        let mut out: Vec<Self> = vcard_categories(vcard)
            .into_iter()
            .map(|category| Self {
                id: format!("{contact_id}#{category}"),
                contact_id: contact_id.to_string(),
                addressbook_id: addressbook_id.to_string(),
                category,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }
}

/// The UID a group member names: `urn:uuid:<UID>`, or a bare UID. `None`
/// for any other URI (`mailto:`), which names no card.
pub fn member_uid(member: &str) -> Option<&str> {
    let bare = member
        .get(..9)
        .filter(|p| p.eq_ignore_ascii_case("urn:uuid:"))
        .map_or(member, |_| &member[9..]);
    (!bare.is_empty() && !bare.contains(':')).then_some(bare)
}

/// The raw store's migration ladder (etl/README.md §"The migration
/// ladder"). Every table but `contacts` is derived from the cards, so a
/// rung that adds one fills it from the cards already stored, and a rung
/// that drops one loses nothing.
pub const LADDER: &[Migration] = &[
    Migration {
        version: 1,
        name: "contact_group_members from the group cards",
        apply: |conn| {
            Box::pin(async move {
                create(conn, GroupMemberRow::all_ddl()).await?;
                for (id, book, vcard) in stored_cards(conn).await? {
                    for m in GroupMemberRow::from_vcard(&id, &book, &vcard) {
                        sqlx::query(
                            "INSERT INTO contact_group_members \
                             (id, group_id, addressbook_id, member, member_id) \
                             VALUES (?, ?, ?, ?, ?)",
                        )
                        .bind(&m.id)
                        .bind(&m.group_id)
                        .bind(&m.addressbook_id)
                        .bind(&m.member)
                        .bind(&m.member_id)
                        .execute(&mut *conn)
                        .await?;
                    }
                }
                Ok(())
            })
        },
    },
    Migration {
        version: 2,
        name: "contact_categories from the cards",
        apply: |conn| {
            Box::pin(async move {
                create(conn, ContactCategoryRow::all_ddl()).await?;
                for (id, book, vcard) in stored_cards(conn).await? {
                    for c in ContactCategoryRow::from_vcard(&id, &book, &vcard) {
                        sqlx::query(
                            "INSERT INTO contact_categories \
                             (id, contact_id, addressbook_id, category) VALUES (?, ?, ?, ?)",
                        )
                        .bind(&c.id)
                        .bind(&c.contact_id)
                        .bind(&c.addressbook_id)
                        .bind(&c.category)
                        .execute(&mut *conn)
                        .await?;
                    }
                }
                Ok(())
            })
        },
    },
    Migration {
        version: 3,
        name: "drop contact_photos; a photo stays in its vCard",
        apply: |conn| {
            Box::pin(async move {
                sqlx::query("DROP TABLE IF EXISTS contact_photos")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        },
    },
    Migration {
        version: 4,
        name: "dav_resources lists what each address book holds",
        apply: |conn| {
            Box::pin(datalib_etl_web::dav::state::adopt(
                conn,
                "contacts",
                "addressbook_id",
            ))
        },
    },
];

/// A rung creates its table rather than leaving it to the DDL, so the
/// open does not see a new table and clear the cursors: an address book's
/// sync-token would still say "caught up", and nothing would ever fill it.
async fn create(conn: &mut sqlx::SqliteConnection, ddl: Vec<String>) -> anyhow::Result<()> {
    for stmt in ddl {
        // Audited: the derive's own DDL; nothing from upstream.
        sqlx::query(sqlx::AssertSqlSafe(stmt))
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Every stored card: its row id, address book and vCard text.
async fn stored_cards(
    conn: &mut sqlx::SqliteConnection,
) -> anyhow::Result<Vec<(String, String, String)>> {
    Ok(
        sqlx::query_as("SELECT id, addressbook_id, json_extract(payload, '$.vcard') FROM contacts")
            .fetch_all(&mut *conn)
            .await?,
    )
}

/// Frozen UUIDv5 namespace for synthesized contacts identity. Changing
/// these bytes re-keys every UID-less contact we have ever ingested, so
/// the sequence is effectively immutable.
const CONTACTS_UUID_NS: Uuid = Uuid::from_bytes([
    0x2d, 0x9b, 0x4e, 0x7a, 0x1c, 0x44, 0x5f, 0x6d, 0x8b, 0x5a, 0x7c, 0x2b, 0x1d, 0x3e, 0x4f, 0x5a,
]);

/// Surrogate `uid` for a vCard that carries no RFC 6350 `UID:` — most
/// notably Google's vCard export, which omits it entirely. Derived from
/// the contact's first + last name so the *same person* collapses onto
/// one PK across re-exports; it's the closest thing to object permanence
/// the data allows when there's no stable server id.
pub fn synthesized_name_uid(given: &str, family: &str) -> String {
    synthesized_name_uid_nth(given, family, 1)
}

/// [`synthesized_name_uid`] for the `nth` card of one file that carries
/// the same name, so two people called the same are two rows. The first
/// keeps the plain id.
pub fn synthesized_name_uid_nth(given: &str, family: &str, nth: usize) -> String {
    let mut recipe = format!(
        "contact:name:{}:{}",
        given.trim().to_lowercase(),
        family.trim().to_lowercase(),
    );
    if nth > 1 {
        recipe.push_str(&format!(":{nth}"));
    }
    Uuid::new_v5(&CONTACTS_UUID_NS, recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

pub const CONTACTS_BY_ADDRESSBOOK_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS contacts_by_addressbook ON contacts(addressbook_id)";

pub const CONTACTS_BY_HREF_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS contacts_by_href ON contacts(addressbook_id, href)";

// Composer

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        ACCOUNTS_DDL.to_string(),
        ADDRESSBOOKS_DDL.to_string(),
        ADDRESSBOOKS_BY_ACCOUNT_INDEX_DDL.to_string(),
        ContactRow::ddl(),
        CONTACTS_BY_ADDRESSBOOK_INDEX_DDL.to_string(),
        CONTACTS_BY_HREF_INDEX_DDL.to_string(),
        // Resume cursor for the local-`.vcf` path: skip re-ingesting a
        // file whose `(size, mtime)` hasn't moved since last run. The
        // CardDAV server path uses etags/sync-tokens instead and never
        // touches this table. See [`vcf_dir`].
        datalib_etl_files::file_checkpoint::INGESTED_FILES_DDL.to_string(),
    ];
    out.extend(GroupMemberRow::all_ddl());
    out.extend(ContactCategoryRow::all_ddl());
    out.extend(datalib_etl_web::dav::state::ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A group card names its members; a person card names none; a
    /// `mailto:` member is kept but names no row.
    #[test]
    fn a_group_card_yields_one_row_per_member() {
        let group = "BEGIN:VCARD\nX-ADDRESSBOOKSERVER-KIND:GROUP\nUID:bridge\n\
                     X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:tng-picard\n\
                     X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:tng-riker\n\
                     MEMBER:mailto:q@continuum.test\nEND:VCARD\n";
        let rows = GroupMemberRow::from_vcard("ab#bridge", "ab", group);
        let got: Vec<(&str, Option<&str>)> = rows
            .iter()
            .map(|r| (r.member.as_str(), r.member_id.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("mailto:q@continuum.test", None),
                ("urn:uuid:tng-picard", Some("ab#tng-picard")),
                ("urn:uuid:tng-riker", Some("ab#tng-riker")),
            ]
        );
        assert!(rows.iter().all(|r| r.group_id == "ab#bridge"));
        let person = "BEGIN:VCARD\nUID:tng-picard\nFN:Picard\nEND:VCARD\n";
        assert!(GroupMemberRow::from_vcard("ab#tng-picard", "ab", person).is_empty());
    }

    #[test]
    fn synthesized_name_uid_is_stable_and_normalized() {
        let a = synthesized_name_uid("Ada", "Lovelace");
        // Same person, re-exported with different case/whitespace, keeps
        // one identity — no churn across exports.
        assert_eq!(a, synthesized_name_uid("  ada ", "LOVELACE"));
        // Distinct names get distinct ids.
        assert_ne!(a, synthesized_name_uid("Alan", "Turing"));
        // Stable, hyphenated UUID string.
        assert_eq!(a.len(), 36);
    }

    #[test]
    fn a_second_card_of_the_same_name_gets_its_own_id() {
        assert_eq!(
            synthesized_name_uid("John", "Smith"),
            synthesized_name_uid_nth("John", "Smith", 1),
        );
        assert_ne!(
            synthesized_name_uid("John", "Smith"),
            synthesized_name_uid_nth("John", "Smith", 2),
        );
    }
}
