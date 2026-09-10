//! The functions this binary performs, as one enum.
//!
//! A step's function is the second half of its id and the directory it
//! writes under its group's, so the spelling here is a storage contract:
//! `ingest` is what `<group>/ingest` is named after. The runner hands the
//! function over in `DATALIB_DAG_FUNCTION` and never interprets it; this
//! binary is the one thing that does.

use strum::VariantArray;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
    strum::Display,
)]
#[strum(serialize_all = "snake_case")]
pub enum Function {
    /// Bring a source's data in — from a live origin or from files on
    /// disk — into its raw store.
    Ingest,
    /// Turn a raw store into markdown plus `grid_rows`.
    RenderMarkdown,
    /// Stack every source's render store into the unified grid table.
    GridIndex,
    /// Build the qmd search index over every rendered tree.
    QmdIndex,
}

impl Function {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a function this binary does not perform.
    pub fn parse(s: &str) -> Option<Function> {
        s.parse().ok()
    }

    pub fn known_list() -> String {
        Function::VARIANTS
            .iter()
            .map(|f| format!("`{}`", f.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_function_round_trips() {
        for &f in Function::VARIANTS {
            assert_eq!(Function::parse(f.as_str()), Some(f));
        }
        assert_eq!(Function::parse("raw"), None);
    }

    /// The function names double as directory names that other crates
    /// spell on their own: the render side's layout helpers and the
    /// index's layout constants. They cannot drift from these.
    #[test]
    fn function_names_are_the_tree_names_other_crates_use() {
        assert_eq!(Function::Ingest.as_str(), datalib_etl::layout::INGEST_DIR);
        assert_eq!(
            Function::RenderMarkdown.as_str(),
            datalib_etl::layout::RENDER_MARKDOWN_DIR
        );
        assert_eq!(Function::GridIndex.as_str(), datalib_core::layout::GRID_DIR);
        assert_eq!(Function::QmdIndex.as_str(), datalib_core::layout::QMD_DIR);
    }
}
