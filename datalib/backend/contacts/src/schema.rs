// The contacts app's tables, one row struct each: the struct is the
// table's schema, and its DDL, column list and upsert are derived from
// it (`etl/macros/README.md` § "PortableTable").

use datalib_contact_schema::ContactKind;
use datalib_etl_macros::PortableTable;

use crate::{FieldKind, LinkedHow};

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
    /// address two people share belongs to a group contact.
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
        pub linked_how: LinkedHow,
        #[col(sql = "TEXT")]
        pub linked_at_utc: String,
        #[col(sql = "TEXT")]
        pub tz_offset: String,
        #[col(sql = "TEXT")]
        pub stopped_working_by: Option<String>,
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

/// Every table, then every index: what the store is opened with.
pub const DDL: &[&str] = &[
    contacts::DDL[0].1,
    handles::DDL[0].1,
    members::DDL[0].1,
    photos::DDL[0].1,
    fields::DDL[0].1,
    handles::INDEXES[0].1,
    fields::INDEXES[0].1,
];
