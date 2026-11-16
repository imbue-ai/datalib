// What each piece of a rendered document says, in order, kept apart from
// how it is drawn, so the viewer, an export and the search index can each
// draw it their own way (docs/dev/plans/messages_not_documents.md
// §"Store the body, format on read"). Held by the render store alone; the
// grid index never copies it.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One piece of a document, carried by the document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "sections", primary_key = "markdown_uuid, seq")]
pub struct SectionRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    /// Its place in the document, from 0.
    #[col(sql = "INTEGER")]
    pub seq: i64,
    /// The item it is, its `data-section-uuid`; null for a piece that is
    /// no item's.
    #[col(sql = "VARCHAR(96)")]
    pub uuid: Option<String>,
    #[col(sql = "TEXT")]
    pub text: String,
    /// `text` is the piece whole, as drawn, by a renderer that has not
    /// split an item's body off its header yet: what reads it draws
    /// nothing around it. Otherwise `text` is an item's body, and its
    /// header is drawn from its row and `addressed`.
    #[col(sql = "INTEGER")]
    pub framed: bool,
    /// Who the item was addressed to where it says so itself, as an
    /// email's To and Cc do: a JSON list of [`Addressee`]. Null for none.
    #[col(sql = "TEXT")]
    pub addressed: Option<String>,
    /// Where the item is filed upstream, as it names them: an email's
    /// labels and mailboxes. A JSON list of strings; null for none.
    #[col(sql = "TEXT")]
    pub labels: Option<String>,
}

/// One person an item was addressed to, as the item names them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Addressee {
    pub role: AddressRole,
    pub name: String,
    /// The handle the item names them by (`email:…`, `tel:…`), where it
    /// names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AddressRole {
    To,
    Cc,
    Bcc,
}

impl AddressRole {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// `addressed` as stored: `None` for nobody.
pub fn addressed_json(to: &[Addressee]) -> Option<String> {
    (!to.is_empty()).then(|| serde_json::to_string(to).expect("an addressee always serializes"))
}

/// `labels` as stored: `None` for none.
pub fn labels_json(labels: &[String]) -> Option<String> {
    (!labels.is_empty()).then(|| serde_json::to_string(labels).expect("strings serialize"))
}

/// `None` for a column this build cannot read.
pub fn parse_labels(json: &str) -> Option<Vec<String>> {
    serde_json::from_str(json).ok()
}

/// `None` for a column this build cannot read, which the caller treats as
/// nobody named.
pub fn parse_addressed(json: &str) -> Option<Vec<Addressee>> {
    serde_json::from_str(json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    #[test]
    fn strum_and_serde_spell_a_role_alike() {
        for role in AddressRole::VARIANTS {
            let json = serde_json::to_string(role).unwrap();
            assert_eq!(json, format!("\"{}\"", role.as_str()));
            assert_eq!(AddressRole::parse(role.as_str()), Some(*role));
        }
        assert_eq!(AddressRole::parse("reply_to"), None);
    }

    #[test]
    fn addressees_round_trip_and_nobody_is_null() {
        let to = vec![
            Addressee {
                role: AddressRole::Cc,
                name: "Deanna Troi".into(),
                handle: None,
            },
            Addressee {
                role: AddressRole::To,
                name: "Data".into(),
                handle: Some("email:data@enterprise.starfleet".into()),
            },
        ];
        let json = addressed_json(&to).unwrap();
        assert_eq!(
            json,
            r#"[{"role":"cc","name":"Deanna Troi"},{"role":"to","name":"Data","handle":"email:data@enterprise.starfleet"}]"#
        );
        assert_eq!(parse_addressed(&json), Some(to));
        assert_eq!(addressed_json(&[]), None);
        let labels = vec!["Patrol routes".to_string()];
        assert_eq!(parse_labels(&labels_json(&labels).unwrap()), Some(labels));
        assert_eq!(labels_json(&[]), None);
    }
}
