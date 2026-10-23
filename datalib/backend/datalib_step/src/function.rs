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
    Ingest,
    RenderMarkdown,
    GridIndex,
    QmdAggregator,
    KeywordIndex,
    Embed,
    EmbeddingMap,
    RenderTimeSeries,
    TimeSeriesIndex,
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
            Function::GridIndex => {
                "Stack every source's rendered rows into the one SQL index the grid and \
                 search read, unified_document_index/grid_index/db.doltlite_db. Goes \
                 under the `unified_document_index` group; its inputs name each \
                 source's `<group>/render_markdown`."
            }
            Function::KeywordIndex => {
                "Bring a source's collection of the qmd search index up to date for \
                 keyword (BM25) search. One per searched source; \
                 inputs = [\"<group>/render_markdown\"]."
            }
            Function::Embed => {
                "Embed what a source's qmd collection lacks, for search by meaning. The \
                 slow one: turn it off for a source with `datalib-dag turn-off <config> \
                 <group>/embed`, and keyword search still works. One per searched \
                 source; inputs = [\"<group>/keyword_index\"]."
            }
            Function::QmdAggregator => {
                "Keep the qmd search index to the sources it reads, and report on the \
                 whole. Goes under the `unified_document_index` group; its inputs name \
                 each searched source's `<group>/keyword_index` and `<group>/embed`. \
                 Without it, free-text search is off."
            }
            Function::EmbeddingMap => {
                "Lay the search index's embeddings out on a plane, for the map card. Goes \
                 under the `unified_document_index` group; \
                 inputs = [\"unified_document_index/qmd_aggregator\"]."
            }
            Function::RenderTimeSeries => {
                "Turn a raw store into its time series, under \
                 <group>/render_time_series/, beside render_markdown, for a source whose \
                 data is also numbers over time (garmin, yolink, airvisual). \
                 inputs = [\"<group>/ingest\"]."
            }
            Function::TimeSeriesIndex => {
                "Index every source's time series, without copying the points. Goes \
                 under the `unified_time_series_index` group; its inputs name each \
                 source's `<group>/render_time_series`."
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
        assert_eq!(Function::GridIndex.as_str(), datalib_core::layout::GRID_DIR);
        assert_eq!(
            Function::TimeSeriesIndex.as_str(),
            datalib_core::layout::TIME_SERIES_INDEX_DIR
        );
        assert_eq!(
            Function::QmdAggregator.as_str(),
            datalib_core::layout::QMD_DIR
        );
        assert_eq!(
            Function::EmbeddingMap.as_str(),
            datalib_unified_document_index::embedding_map::DIR
        );
    }
}
