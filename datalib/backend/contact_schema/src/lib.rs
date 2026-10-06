//! `DatalibContact`: a person as one source describes them — a Slack
//! user, a vCard, a LinkedIn connection, a contact someone made in the
//! contacts app. Many describe the same person; `source_id` tells them
//! apart. This crate is only the shape, so anything can take it: the
//! render crates fill it from raw rows, contact-common renders it, the
//! contacts app answers in it. `docs/dev/plans/contacts.md` has the design.

use datalib_handle::{Handle, HandleKind};
use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatalibContact {
    /// Who describes the person: a source's group id, or the contacts app.
    pub source_id: String,
    /// That source's own id for them: a vCard UID, a Slack user id, a
    /// profile URL, a contact id.
    pub key: String,
    pub kind: ContactKind,
    /// The names the source shows, the one it prefers first.
    pub names: Vec<String>,
    pub handles: Vec<ContactHandle>,
    pub photo: Option<Photo>,
    pub org: Option<String>,
    pub title: Option<String>,
    pub note: Option<String>,
    /// Anything else the source says, in its order: an address, a
    /// birthday, when a connection was made.
    pub details: Vec<Detail>,
    /// The groups the person is filed under.
    pub groups: Vec<String>,
    /// For a group: the names of its members.
    pub members: Vec<String>,
    /// The person's own page at the source, where it has one.
    pub source_url: Option<String>,
    /// Stamps the record itself carries, as the source wrote them.
    pub created_at: Option<String>,
    pub modified_at: Option<String>,
    /// How much of this source the person wrote, where it is a chat.
    pub seen: Option<Seen>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub items: u64,
    /// The newest item's stamp, as the source wrote it.
    pub last_at: Option<String>,
}

impl DatalibContact {
    pub fn new(source_id: impl Into<String>, key: impl Into<String>, kind: ContactKind) -> Self {
        Self {
            source_id: source_id.into(),
            key: key.into(),
            kind,
            names: Vec::new(),
            handles: Vec::new(),
            photo: None,
            org: None,
            title: None,
            note: None,
            details: Vec::new(),
            groups: Vec::new(),
            members: Vec::new(),
            source_url: None,
            created_at: None,
            modified_at: None,
            seen: None,
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.names.first().map(String::as_str)
    }
}

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
pub enum ContactKind {
    Person,
    /// Several people behind one identity: a household's shared
    /// address, a mailing list, a vCard group.
    Group,
}

impl ContactKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// One way to reach the person, as the source wrote it. `handle` is the
/// normalized identifier when there is one; a phone number written
/// without its country code has none, and is kept anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactHandle {
    pub medium: Medium,
    /// The source's word for it: `work`, `cell`, `home`.
    pub label: Option<String>,
    pub value: String,
    pub handle: Option<Handle>,
    /// A partial date (`2019`, `2019-06`, `2019-06-14`) by which it had
    /// stopped working; only the contacts app records this.
    pub stopped_working_by: Option<String>,
}

impl ContactHandle {
    pub fn email(label: Option<String>, value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            medium: Medium::Email,
            handle: Handle::email(&value),
            label,
            value,
            stopped_working_by: None,
        }
    }

    pub fn phone(label: Option<String>, value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            medium: Medium::Phone,
            handle: Handle::tel(&value),
            label,
            value,
            stopped_working_by: None,
        }
    }

    /// A handle with nothing the source wrote beside it: what chat-common
    /// knows of an author.
    pub fn of(handle: Handle) -> Self {
        Self {
            medium: Medium::of_kind(handle.kind()),
            label: None,
            value: handle.value().to_string(),
            handle: Some(handle),
            stopped_working_by: None,
        }
    }
}

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
pub enum Medium {
    Email,
    Phone,
    Other,
}

impl Medium {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn of_kind(kind: HandleKind) -> Self {
        match kind {
            HandleKind::Email => Medium::Email,
            HandleKind::Tel => Medium::Phone,
            HandleKind::Slack => Medium::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detail {
    pub label: String,
    pub value: String,
}

impl Detail {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Photo {
    /// The image itself, from the source's own record.
    Inline {
        content_type: String,
        /// Written to the document's `blobs/`; never stored as a row.
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    /// Only where it can be fetched; nothing fetched it yet.
    Url(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_without_its_country_code_is_kept_without_a_handle() {
        let h = ContactHandle::phone(Some("cell".into()), "(555) 010-1234");
        assert_eq!(h.value, "(555) 010-1234");
        assert_eq!(h.handle, None);
        let h = ContactHandle::phone(None, "+1 555 010 1234");
        assert_eq!(h.handle.unwrap().as_str(), "tel:+15550101234");
    }

    #[test]
    fn strum_and_serde_agree() {
        for k in ContactKind::VARIANTS {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
            assert_eq!(ContactKind::parse(k.as_str()), Some(*k));
        }
        for m in Medium::VARIANTS {
            assert_eq!(serde_json::to_value(m).unwrap(), m.as_str());
        }
    }
}
