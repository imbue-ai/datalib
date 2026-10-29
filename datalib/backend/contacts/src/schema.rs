// The contacts app's tables, one row struct each: the struct is the
// table's schema, and its DDL, column list and upsert are derived from
// it (`etl/macros/README.md` § "PortableTable").

use datalib_contact_schema::ContactKind;
use datalib_etl_macros::PortableTable;

use crate::{DecidedHow, FieldKind};

pub mod contacts {
    use super::*;

    #[derive(Debug, Clone, PortableTable)]
    #[portable_table(table = "contacts", primary_key = "contact_id")]
    pub struct ContactRow {
        #[col(sql = "TEXT")]
        pub contact_id: String,
        #[col(sql = "TEXT", enum)]
        pub kind: ContactKind,
        #[col(sql = "TEXT")]
        pub name: String,
        #[col(sql = "TEXT")]
        pub note: Option<String>,
        #[col(sql = "TEXT")]
        pub merged_into: Option<String>,
        /// The agent that made the contact, for one a person did not.
        #[col(sql = "TEXT")]
        pub agent: Option<String>,
        #[col(sql = "TEXT")]
        pub created_at_utc: String,
        #[col(sql = "TEXT")]
        pub updated_at_utc: String,
        #[col(sql = "TEXT")]
        pub tz_offset: String,
    }
}

pub mod handles {
    use super::*;

    /// A link: a handle belongs to one contact for all time, and an
    /// address two people share belongs to a group contact. A link an
    /// agent made names the agent and its reason, and waits for a person
    /// until `reviewed_at_utc` says one confirmed it.
    #[derive(Debug, Clone, PortableTable)]
    #[portable_table(
        table = "handles",
        primary_key = "handle",
        index = "handles_by_contact:contact_id"
    )]
    pub struct HandleRow {
        #[col(sql = "TEXT")]
        pub handle: String,
        #[col(sql = "TEXT")]
        pub contact_id: String,
        #[col(sql = "TEXT", enum)]
        pub linked_how: DecidedHow,
        #[col(sql = "TEXT")]
        pub linked_at_utc: String,
        #[col(sql = "TEXT")]
        pub tz_offset: String,
        #[col(sql = "TEXT")]
        pub stopped_working_by: Option<String>,
        #[col(sql = "TEXT")]
        pub agent: Option<String>,
        #[col(sql = "TEXT")]
        pub reason: Option<String>,
        #[col(sql = "TEXT")]
        pub reviewed_at_utc: Option<String>,
        /// How sure the agent was, 0 to 1, and which of its runs said so.
        #[col(sql = "REAL")]
        pub confidence: Option<f64>,
        #[col(sql = "TEXT")]
        pub run: Option<String>,
    }
}

pub mod not_linked {
    use super::*;

    /// A person said `handle` is not `contact_id`: they unlinked or
    /// rejected an agent's link. An agent may not propose it again, and
    /// the row outlives the contact, so it names the contact as it was
    /// called and keeps what the agent said. `contact_rejected` marks the
    /// handles of a contact an agent made that the person rejected whole:
    /// together they are a grouping no agent may propose again.
    #[derive(Debug, Clone, PortableTable)]
    #[portable_table(table = "not_linked", primary_key = "handle, contact_id")]
    pub struct NotLinkedRow {
        #[col(sql = "TEXT")]
        pub handle: String,
        #[col(sql = "TEXT")]
        pub contact_id: String,
        #[col(sql = "TEXT")]
        pub contact_name: String,
        #[col(sql = "TEXT")]
        pub agent: Option<String>,
        #[col(sql = "TEXT")]
        pub reason: Option<String>,
        #[col(sql = "TEXT")]
        pub decided_at_utc: String,
        #[col(sql = "TEXT")]
        pub tz_offset: String,
        #[col(sql = "INTEGER")]
        pub contact_rejected: bool,
    }
}

pub mod members {
    use super::*;

    #[derive(Debug, Clone, PortableTable)]
    #[portable_table(table = "members", primary_key = "group_id, member_id")]
    pub struct MemberRow {
        #[col(sql = "TEXT")]
        pub group_id: String,
        #[col(sql = "TEXT")]
        pub member_id: String,
        #[col(sql = "TEXT")]
        pub added_at_utc: String,
        #[col(sql = "TEXT")]
        pub tz_offset: String,
    }
}

pub mod photos {
    use super::*;

    /// The photo a person put on a contact: one per contact, the bytes as
    /// given. Served by the applet at `/photo/<contact_id>`.
    #[derive(Debug, Clone, PortableTable)]
    #[portable_table(table = "photos", primary_key = "contact_id")]
    pub struct PhotoRow {
        #[col(sql = "TEXT")]
        pub contact_id: String,
        #[col(sql = "TEXT")]
        pub content_type: String,
        #[col(sql = "BLOB")]
        pub bytes: Vec<u8>,
        #[col(sql = "TEXT")]
        pub set_at_utc: String,
        #[col(sql = "TEXT")]
        pub tz_offset: String,
    }
}

pub mod fields {
    use super::*;

    /// One line of what a contact says about a person, as vCard has
    /// them: a number, an address, a title. A field links nothing, even
    /// an email or a number; `handle` is its value as a handle, where it
    /// is one, so the card can say how it stands against the links.
    #[derive(Debug, Clone, PortableTable)]
    #[portable_table(
        table = "fields",
        primary_key = "field_id",
        index = "fields_by_contact:contact_id"
    )]
    pub struct FieldRow {
        #[col(sql = "TEXT")]
        pub field_id: String,
        #[col(sql = "TEXT")]
        pub contact_id: String,
        #[col(sql = "TEXT", enum)]
        pub kind: FieldKind,
        #[col(sql = "TEXT")]
        pub label: Option<String>,
        #[col(sql = "TEXT")]
        pub value: String,
        #[col(sql = "TEXT")]
        pub handle: Option<String>,
        #[col(sql = "INTEGER")]
        pub position: i64,
        /// The source whose record the value was copied from, and that
        /// record's key there; empty for a value the person typed.
        #[col(sql = "TEXT")]
        pub copied_from_source: Option<String>,
        #[col(sql = "TEXT")]
        pub copied_from_key: Option<String>,
    }
}

pub mod me {
    use super::*;

    /// A contact the person said is them: `from:me` reaches every handle
    /// it does. More than one may be, for a duplicate not merged yet.
    #[derive(Debug, Clone, PortableTable)]
    #[portable_table(table = "me", primary_key = "contact_id")]
    pub struct MeRow {
        #[col(sql = "TEXT")]
        pub contact_id: String,
        #[col(sql = "TEXT")]
        pub marked_at_utc: String,
        #[col(sql = "TEXT")]
        pub tz_offset: String,
    }
}

pub mod labels {
    use super::*;

    /// A label on a subject, which is a handle (`email:…`) or a contact
    /// (`contact:<id>`), such as `REAL` (not spam). A row says
    /// the label is in or out; no row says nobody knows. An agent's row
    /// waits for a person (`reviewed_at_utc`); one the person overrode is
    /// theirs (`manual`) and still names the agent, which said the
    /// opposite.
    #[derive(Debug, Clone, PortableTable)]
    #[portable_table(
        table = "labels",
        primary_key = "subject, label",
        index = "labels_by_label:label"
    )]
    pub struct LabelRow {
        #[col(sql = "TEXT")]
        pub subject: String,
        #[col(sql = "TEXT")]
        pub label: String,
        #[col(sql = "INTEGER")]
        pub applies: bool,
        #[col(sql = "TEXT", enum)]
        pub decided_how: DecidedHow,
        #[col(sql = "TEXT")]
        pub agent: Option<String>,
        #[col(sql = "TEXT")]
        pub reason: Option<String>,
        #[col(sql = "TEXT")]
        pub decided_at_utc: String,
        #[col(sql = "TEXT")]
        pub reviewed_at_utc: Option<String>,
        #[col(sql = "TEXT")]
        pub tz_offset: String,
        #[col(sql = "REAL")]
        pub confidence: Option<f64>,
        #[col(sql = "TEXT")]
        pub run: Option<String>,
    }
}

/// Every table, then every index: what the store is opened with.
pub const DDL: &[&str] = &[
    contacts::DDL[0].1,
    handles::DDL[0].1,
    members::DDL[0].1,
    photos::DDL[0].1,
    fields::DDL[0].1,
    me::DDL[0].1,
    not_linked::DDL[0].1,
    labels::DDL[0].1,
    handles::INDEXES[0].1,
    fields::INDEXES[0].1,
    labels::INDEXES[0].1,
];
