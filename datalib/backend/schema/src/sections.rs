// What each piece of a rendered document says, in order, kept apart from
// how it is drawn, so the viewer, an export and the search index can each
// draw it their own way (docs/dev/plans/documents_to_blocks.md
// §"Store the body, format on read"). An item's piece keeps every fact its
// box shows, and is drawn from them when it is read; `drawings` keeps only
// the layout between items. Held by the render store alone; the grid index
// never copies it.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One piece of a document, carried by the document.
#[derive(
    Debug, Clone, Default, PartialEq, Serialize, Deserialize, PortableTable, sqlx::FromRow,
)]
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
    /// nothing around it. Otherwise `text` is an item's body, and the
    /// rest of its piece is drawn from the columns below.
    #[col(sql = "INTEGER")]
    pub framed: bool,
    /// How an item's piece is drawn around its body ([`ItemForm`]); null
    /// for a piece kept whole.
    #[col(sql = "TEXT")]
    pub form: Option<String>,
    /// Who wrote the item, as its header shows them; null for nobody.
    #[col(sql = "TEXT")]
    pub author: Option<String>,
    /// Who wrote it, as a handle (`Handle::as_str`), where the source
    /// names one.
    #[col(sql = "TEXT")]
    pub author_handle: Option<String>,
    /// When, as its header shows it: an RFC 3339 instant, `Z` where the
    /// source named no clock, or a day.
    #[col(sql = "TEXT")]
    pub at: Option<String>,
    /// Words the source says about the item beside its author (a review's
    /// `APPROVED`): a JSON list of strings; null for none.
    #[col(sql = "TEXT")]
    pub tags: Option<String>,
    /// The item's own page at its source, its header's `↗`.
    #[col(sql = "TEXT")]
    pub link: Option<String>,
    /// The reactions on the item, in the order they are drawn: a JSON list
    /// of [`Reaction`]. Null for none.
    #[col(sql = "TEXT")]
    pub reactions: Option<String>,
    /// How many replies the thread this item opens has upstream; null
    /// where the source does not say.
    #[col(sql = "INTEGER")]
    pub replies: Option<i64>,
    /// How many of those replies the mirror holds.
    #[col(sql = "INTEGER")]
    pub replies_held: Option<i64>,
    /// A chat message that continues a run of its author's messages, so
    /// it is drawn under the header of the first of them.
    #[col(sql = "INTEGER")]
    pub continues: bool,
    /// Who the item was addressed to where it says so itself, as an
    /// email's To and Cc do: a JSON list of [`Addressee`]. Null for none.
    #[col(sql = "TEXT")]
    pub addressed: Option<String>,
    /// Where the item is filed upstream, as it names them: an email's
    /// labels and mailboxes. A JSON list of strings; null for none.
    #[col(sql = "TEXT")]
    pub labels: Option<String>,
    /// The class of the box the piece is drawn in, where it is in one:
    /// a `<div>` naming the item, around what the piece says
    /// (`Section::boxed`). Null for a piece in no box.
    #[col(sql = "TEXT")]
    pub class: Option<String>,
}

/// How an item's piece is drawn around its body.
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
pub enum ItemForm {
    /// A header naming who and when, then who it was addressed to, its
    /// labels, its body, its reactions and its replies.
    Message,
    /// A message in a chat: as a message, but its reactions are one pill
    /// per emoji, and one that continues its author's run has no header.
    Chat,
    /// One small line: when, and the body, which says what happened (a
    /// member joined, a chat was renamed).
    Event,
}

impl ItemForm {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// One reaction on an item, as its item draws it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    /// The reaction's own uuid: its grid row, and its anchor in the item.
    pub uuid: String,
    pub emoji: String,
    /// Who reacted, as the source shows them.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// How many reacted, where the source counts them and names nobody;
    /// one where `name` is who did.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub count: u32,
}

fn one() -> u32 {
    1
}

fn is_one(count: &u32) -> bool {
    *count == 1
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

/// `reactions` as stored: `None` for none.
pub fn reactions_json(reactions: &[Reaction]) -> Option<String> {
    (!reactions.is_empty())
        .then(|| serde_json::to_string(reactions).expect("a reaction always serializes"))
}

/// `None` for a column this build cannot read.
pub fn parse_reactions(json: &str) -> Option<Vec<Reaction>> {
    serde_json::from_str(json).ok()
}

/// `None` for a column this build cannot read. `tags` is read the same way.
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
    fn strum_and_serde_spell_a_form_alike() {
        for form in ItemForm::VARIANTS {
            let json = serde_json::to_string(form).unwrap();
            assert_eq!(json, format!("\"{}\"", form.as_str()));
            assert_eq!(ItemForm::parse(form.as_str()), Some(*form));
        }
        assert_eq!(ItemForm::parse("page"), None);
    }

    #[test]
    fn reactions_round_trip_and_none_is_null() {
        let r = vec![Reaction {
            uuid: "r1".into(),
            emoji: "🖖".into(),
            name: "Spock".into(),
            handle: None,
            count: 1,
        }];
        let json = reactions_json(&r).unwrap();
        assert_eq!(json, r#"[{"uuid":"r1","emoji":"🖖","name":"Spock"}]"#);
        assert_eq!(parse_reactions(&json), Some(r.clone()));
        let crowd = vec![Reaction {
            name: "3 people".into(),
            count: 3,
            ..r[0].clone()
        }];
        let json = reactions_json(&crowd).unwrap();
        assert!(json.ends_with(r#""count":3}]"#), "{json}");
        assert_eq!(parse_reactions(&json), Some(crowd));
        assert_eq!(reactions_json(&[]), None);
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
