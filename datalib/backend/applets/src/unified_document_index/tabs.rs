//! Which of a free-text search's three answers a request asks for. The
//! search grid shows each in a tab of its own. With none named, a search
//! answers as it always has: identifiers from the search terms file, the
//! rest from qmd's hybrid query.

use serde::{Deserialize, Serialize};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SearchTab {
    /// The ids, people, titles and names a row answers to (the terms
    /// file).
    Fields,
    /// The words of every document, ranked by BM25 (qmd's keyword index,
    /// read directly).
    Words,
    /// Documents about the same thing, whatever their words (qmd's vector
    /// search).
    Meaning,
}

impl SearchTab {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    #[test]
    fn strum_and_serde_spell_each_tab_alike() {
        for tab in SearchTab::VARIANTS {
            let serde = serde_json::to_value(tab).unwrap();
            assert_eq!(serde, serde_json::json!(tab.as_str()));
            assert_eq!(tab.as_str().parse::<SearchTab>().ok(), Some(*tab));
        }
    }
}
