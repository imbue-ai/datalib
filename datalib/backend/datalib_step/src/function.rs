//! The functions this binary performs, as one enum.
//!
//! A step's function is the second half of its id and the directory it
//! writes under its group's, so the spelling here is a storage contract:
//! `ingest` is what `<group>/ingest` is named after. A step's `command`
//! names it (`datalib-step run <function> …`); the runner never interprets
//! it, and this binary is the one thing that does.

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
    Ingest,
    RenderMarkdown,
    UnifiedGridAndSearchIndex,
    Embed,
    EmbeddingMap,
    RenderTimeSeries,
    TimeSeriesIndex,
    RenderOccurrences,
}

impl Function {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a function this binary does not perform.
    pub fn parse(s: &str) -> Option<Function> {
        s.parse().ok()
    }

    // What `datalib-dag --reset` can empty; `reset.rs` refuses the rest
    // through this, and `datalib-step help` tells a function's page by it.
    pub fn resets(self) -> bool {
        matches!(
            self,
            Function::Ingest | Function::RenderMarkdown | Function::EmbeddingMap
        )
    }

    pub fn describe(self) -> &'static str {
        match self {
            Function::Ingest => {
                "Bring a source's data in, from its service or from files on disk, into its \
                 raw store, <group>/ingest/. Its params say where from: \
                 `datalib-step help <type>`. One per source; inputs = []."
            }
            Function::RenderMarkdown => {
                "Turn a source's raw store into markdown documents and the rows the grid \
                 shows, under <group>/render_markdown/. One per source; \
                 inputs = [\"<group>/ingest\"]."
            }
            Function::UnifiedGridAndSearchIndex => {
                "Stack every source's rendered rows into the one SQL index the grid and \
                 search read, unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite, and put \
                 each searched source's documents in the search index's keyword (BM25) \
                 tables beside them. Goes under the `unified_grid_and_search_index` group; \
                 inputs = [\"*/render_markdown\"]. `params.not_searched` lists the sources \
                 kept out of search."
            }
            Function::Embed => {
                "Cut a source's documents into chunks and embed the ones the search index \
                 has no vector for, for search by meaning. The slow one: turn it off for a source with \
                 `datalib-dag turn-off <config> <group>/embed`, and keyword search still \
                 works. One per source searched by meaning; \
                 inputs = [\"unified_grid_and_search_index/unified_grid_and_search_index\"]. Param \
                 `embed_on_modal_from_n_chunks`: a run that owes at least that many \
                 chunks embeds them on the person's own Modal account; absent, \
                 nothing leaves the machine."
            }
            Function::EmbeddingMap => {
                "Lay the search index's embeddings out on a plane, for the map card. Goes \
                 under the `unified_grid_and_search_index` group; inputs = [\"*/embed\"], \
                 so it runs after every embed."
            }
            Function::RenderTimeSeries => {
                "Turn a raw store into its time series, under \
                 <group>/render_time_series/, beside render_markdown, for a source whose \
                 data is also numbers over time (garmin, yolink, airvisual). \
                 inputs = [\"<group>/ingest\"]."
            }
            Function::TimeSeriesIndex => {
                "Index every source's time series, without copying the points. Goes \
                 under the `unified_time_series_index` group; inputs = \
                 [\"*/render_time_series\"]."
            }
            Function::RenderOccurrences => {
                "List every occurrence of a calendar's events in a window of days around \
                 now, one row each with Google's instance ids, moved and cancelled dates \
                 applied, in <group>/render_occurrences/occurrences.doltlite_db. For a \
                 `calendar` source; params `days_back` (30) and `days_ahead` (400); \
                 inputs = [\"<group>/ingest\"]."
            }
        }
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
        assert_eq!(
            Function::RenderTimeSeries.as_str(),
            datalib_etl::layout::RENDER_TIME_SERIES_DIR
        );
        assert_eq!(
            Function::RenderOccurrences.as_str(),
            datalib_etl::layout::RENDER_OCCURRENCES_DIR
        );
        assert_eq!(
            Function::UnifiedGridAndSearchIndex.as_str(),
            datalib_core::layout::INDEX_STEP_DIR
        );
        assert_eq!(
            Function::TimeSeriesIndex.as_str(),
            datalib_core::layout::TIME_SERIES_INDEX_DIR
        );
        assert_eq!(
            Function::EmbeddingMap.as_str(),
            datalib_unified_grid_and_search_index::embedding_map::DIR
        );
    }
}
