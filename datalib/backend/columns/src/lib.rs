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
    /// A [`Quantity`]: one figure, drawn by its unit (`count` as grouped
    /// digits, `seconds` as "25 min"), a short note in its place when
    /// there is no figure to give, and the reasoning on hover.
    Quantity,
    /// An [`Identity`]: something resolved to a label and an icon token
    /// by whoever serves the row, shown as icon + label with the id on
    /// hover.
    Identity,
    /// A [`Status`]: a glyph for the word, when it got there (relative,
    /// like a `Timestamp`), the reason on hover, and a bar while it
    /// moves. Sorts on when.
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
    /// How the producer's search bar filters on this column, where it
    /// can: a cell's value becomes a term the viewer writes into it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<ColumnSearch>,
    /// On an [`Identity`] column: the row field holding [`Chip`]s the
    /// cell draws after the label, as bare counts. A double-click on
    /// them reaches the viewer as a double-click on that field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badges: Option<String>,
}

/// The key a term on this column starts with (`author:`), and the row
/// field whose value the term names: the uuid behind a name, the id
/// behind a label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSearch {
    pub key: String,
    pub field: String,
}

/// What a paged grid reads of its rows beyond their columns: the field
/// that names a row, the document a selected row opens, and what free
/// text in its search bar matches.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RowsSpec {
    pub row_key: &'static str,
    pub document: DocumentLink,
    pub free_text: FreeTextMatch,
}

/// A key the search bar takes, and what its values are, so the bar can
/// offer them as a person types.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchKeySpec {
    pub key: &'static str,
    /// Older spellings it still reads.
    pub aliases: &'static [&'static str],
    pub values: KeyValues,
}

/// A value the search bar offers for a key, with how many rows have it
/// where that was counted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValueSuggestion {
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeyValues {
    /// Whatever is typed.
    Text,
    /// One of these words and nothing else.
    Words { words: Vec<&'static str> },
    /// A configured source's id, drawn as its group chip.
    Source,
    /// Any configured group's id, drawn as its group chip.
    Group,
    /// A step's id, `<group>/<function>`, drawn as its step chip.
    Step,
    /// A person: a handle, drawn as the person's chip, or text that
    /// matches part of a handle or a name.
    Person,
    /// A date or a moment, `before:` and `after:`.
    Stamp,
}

/// The document a row opens: the first of `fields` the row has a value
/// in, at the section `anchor` names.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DocumentLink {
    pub fields: &'static [&'static str],
    pub anchor: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreeTextMatch {
    /// qmd ranks the rows: they come in its order, best first.
    Qmd,
    /// A substring of some of the columns; the rows keep their order.
    Like,
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
            search: None,
            badges: None,
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
    pub fn badges(mut self, field: &str) -> Self {
        self.badges = Some(field.into());
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
    /// The entity this names, as the URI a chip link would carry
    /// (`datalib:group/slack`), when the viewer should draw it as a chip
    /// it can resolve, open and copy (docs/dev/chips.md). Beside
    /// `id` rather than in it: other code keys on the bare id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
}

/// Something datalib itself names that a chip can resolve: a group of
/// the config, or one of its steps. `ui/src/cards/chipLinks.js` reads
/// and writes the same URIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entity<'a> {
    /// A group id: the directory under the data root.
    Group(&'a str),
    /// A step id, `<group>/<function>`.
    Step(&'a str),
}

impl<'a> Entity<'a> {
    pub fn uri(self) -> String {
        match self {
            Entity::Group(id) => format!("datalib:group/{id}"),
            Entity::Step(id) => format!("datalib:step/{id}"),
        }
    }

    /// `None` for a URI that names neither, or names one with an empty id.
    pub fn parse(uri: &'a str) -> Option<Self> {
        let rest = uri.strip_prefix("datalib:")?;
        let (kind, id) = rest.split_once('/')?;
        if id.is_empty() {
            return None;
        }
        match kind {
            "group" => Some(Entity::Group(id)),
            "step" => Some(Entity::Step(id)),
            _ => None,
        }
    }
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
    /// How far back the plot reaches, in seconds. The producer's to
    /// say, since it knows how far back it kept: bytes on disk over
    /// minutes and items over days sit side by side in one table.
    pub window_secs: u64,
    /// The breakdown behind the number, for its hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One figure with the reasoning behind it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Quantity {
    /// `None` when there is no figure to give; then `note` says why in a
    /// word or two, or the cell is blank.
    pub value: Option<i64>,
    /// `count` or `seconds`; the viewer formats by it.
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// How the figure was reached, for its hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
    Metric,
    Warning,
    Error,
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
    /// The enabled button's hover: what pressing it does, in a sentence.
    /// `label` alone when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The disabled button's hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// Drawn as something to think twice about.
    #[serde(default)]
    pub danger: bool,
    /// Drawn as an on/off switch in this position rather than a button;
    /// pressing it asks for the other one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
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

    /// The URIs a chip link carries for a group and a step; the TS
    /// mirror in `chipLinks.js` is tested over the same cases.
    #[test]
    fn an_entity_round_trips_through_its_uri() {
        for e in [Entity::Group("slack"), Entity::Step("slack/ingest")] {
            let uri = e.uri();
            assert_eq!(Entity::parse(&uri), Some(e));
        }
        assert_eq!(Entity::Group("slack").uri(), "datalib:group/slack");
        assert_eq!(
            Entity::Step("slack/ingest").uri(),
            "datalib:step/slack/ingest"
        );
        assert_eq!(Entity::parse("datalib:group/"), None);
        assert_eq!(Entity::parse("datalib:handle/tel/+15550123456"), None);
        assert_eq!(Entity::parse("mailto:riker@enterprise.org"), None);
    }
}
