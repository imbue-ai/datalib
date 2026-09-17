// How a `grid_rows` row differs between the two commits a diff group
// compares. NULL on every real source's rows: a value here is what
// says a row came from a diff tree at all.

use serde::{Deserialize, Serialize};

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
pub enum DiffStatus {
    /// Present at `to`, absent at `from`.
    Added,
    /// Present at `from`, absent at `to`; the row is the `from` side's.
    Removed,
    /// Present on both sides with at least one cell different —
    /// `diff_changed_columns` names which.
    Modified,
    /// Present on both sides, identical. Kept so a changed document
    /// reads in full; the grid's "changed only" view filters it.
    Unchanged,
}

impl DiffStatus {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// The separator between column names in `grid_rows.diff_changed_columns`.
pub const CHANGED_COLUMNS_SEPARATOR: char = '|';

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    #[test]
    fn strum_and_serde_agree() {
        for &s in DiffStatus::VARIANTS {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(DiffStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(DiffStatus::parse("edited"), None);
    }
}
