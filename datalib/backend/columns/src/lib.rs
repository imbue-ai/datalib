//! The column-type vocabulary. A table's producer — `GET /api/manage/rows`,
//! the `unified_index` applet's search — declares a [`ColumnSpec`] per
//! column, and the one typed viewer in the UI draws each cell by its
//! type rather than by comparing field names. The value shapes below
//! are what a cell of each type holds on the wire. Add a member when a
//! second surface needs it, not in anticipation.
//!
//! Mirrored by hand as string unions in `datalib/ui/src/api.ts`; change
//! both halves together.

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

pub mod source_catalog;

/// What a cell holds, and so how it is drawn.
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
pub enum ColumnType {
    /// A string, shown as is. The default.
    Text,
    /// An integer count, shown with grouped digits.
    Count,
    /// A float, shown to a few decimals, right-aligned.
    Number,
    /// A byte count, shown as a human size with the exact figure on hover.
    Bytes,
    /// An ISO-8601 stamp, shown relative ("7 days ago") with the exact
    /// stamp on hover; sorts on the instant. For a stamp about *now* —
    /// when something last ran.
    Timestamp,
    /// An ISO-8601 stamp, shown as the date and time it names; sorts on
    /// the instant. For a stamp that is the record's — when a message
    /// was sent.
    Datetime,
    /// A [`Timeseries`]: its latest value over a sparkline of recent
    /// samples, calibrated across the column.
    Timeseries,
    /// An [`Identity`]: something resolved to a label and an icon token
    /// by whoever serves the row, shown as icon + label with the id on
    /// hover.
    Identity,
    /// A [`Status`]: a glyph for the word, the reason on hover, and a
    /// bar while it moves.
    Status,
    /// A row of [`Chip`]s.
    Chips,
    /// A row of [`Action`]s, drawn as buttons; the viewer maps a known
    /// id to code it already holds and draws nothing for one it doesn't.
    Actions,
    /// A `markdowns.uuid`, shown as its title; opens the document on click.
    MarkdownUuid,
}

impl ColumnType {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// One column, as the producer declares it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSpec {
    /// The key on each row.
    pub field: String,
    pub header: String,
    pub r#type: ColumnType,
    /// What the column means, for its header's hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Shown until someone hides it.
    #[serde(default = "yes")]
    pub default_visible: bool,
    /// The viewer may offer an in-place edit of this cell; the card
    /// decides what an edit does.
    #[serde(default)]
    pub editable: bool,
}

fn yes() -> bool {
    true
}

impl ColumnSpec {
    pub fn new(field: &str, header: &str, r#type: ColumnType) -> Self {
        ColumnSpec {
            field: field.into(),
            header: header.into(),
            r#type,
            description: None,
            default_visible: true,
            editable: false,
        }
    }
    pub fn describe(mut self, description: &str) -> Self {
        self.description = Some(description.into());
        self
    }
    pub fn hidden(mut self) -> Self {
        self.default_visible = false;
        self
    }
    pub fn editable(mut self) -> Self {
        self.editable = true;
        self
    }
}

/// Something resolved before it was sent: the id the producer joins on,
/// the label a person reads, and an icon *token* the viewer maps to an
/// asset (`"slack"`, `"step:ingest"`). The producer is the only party
/// that can resolve it; the viewer owns how it looks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// What the icon stands for, for its hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// ISO-8601 with its offset.
    pub at: String,
    pub value: i64,
}

/// A value with the recent measurements behind it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Timeseries {
    /// `None` is "nothing measured yet" — the absence of a plot, not a
    /// flat line at zero.
    pub value: Option<i64>,
    /// What the value counts; the viewer formats `bytes` as a size.
    pub unit: String,
    /// Oldest first. Compacted: a step function, not an even grid.
    pub samples: Vec<Sample>,
    /// The breakdown behind the number, for its hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One segment of a status bar: a part of the whole and the status it
/// is in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub id: String,
    pub key: String,
    pub label: String,
}

/// One row's status, reduced to a vocabulary a Status column can draw.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// The status word's key — `running`, `never_run` — which picks the
    /// glyph. A key the viewer has not met is drawn as its label.
    pub key: String,
    pub label: String,
    /// When this status was reached, if it is the kind that is reached.
    pub at: Option<String>,
    /// When the thing last succeeded, whatever it has done since. Equal
    /// to `at` for a row whose latest outcome was a success; older for
    /// one that has failed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<String>,
    /// Why it is that word — the failure, what it is waiting on.
    pub detail: Option<String>,
    /// How far along, in `[0, 1]`, when the thing said how much is ahead
    /// of it. Drawn only while `key` is `running`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f64>,
    /// For a status that aggregates several things in flight: one
    /// segment each, drawn as a bar instead of the glyph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segments: Option<Vec<Segment>>,
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
pub enum ChipKind {
    Info,
    Idle,
    Metric,
    Warning,
    Error,
    /// Checked and found clean: a green zero, as opposed to `Idle`'s
    /// nothing-to-do grey-green.
    Ok,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chip {
    pub kind: ChipKind,
    pub text: String,
    pub title: String,
}

/// A button on a row. Data decides whether it appears and what it says;
/// the viewer's code decides what it does — deliberately no URL here,
/// since a URL arriving as data is a capability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    /// The disabled button's hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// Drawn as something to think twice about.
    #[serde(default)]
    pub danger: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strum and serde are independent derives producing independent
    /// strings, so their agreement is a real check.
    #[test]
    fn strum_and_serde_spell_the_types_the_same() {
        for t in ColumnType::VARIANTS {
            let json = serde_json::to_string(t).unwrap();
            assert_eq!(json, format!("\"{}\"", t.as_str()));
            assert_eq!(ColumnType::parse(t.as_str()), Some(*t));
        }
        for k in ChipKind::VARIANTS {
            let json = serde_json::to_string(k).unwrap();
            let s: &'static str = (*k).into();
            assert_eq!(json, format!("\"{s}\""));
        }
        assert_eq!(ColumnType::parse("hologram"), None);
    }

    #[test]
    fn a_spec_serializes_its_type_under_the_plain_key() {
        let spec = ColumnSpec::new("bytes", "On disk", ColumnType::Timeseries).hidden();
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["type"], "timeseries");
        assert_eq!(json["default_visible"], false);
        assert_eq!(json["editable"], false);
        assert!(json.get("description").is_none());
    }
}
