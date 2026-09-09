//! The render half of the ETL framework: the per-source render store,
//! the unified-index load, and the run context a render processor is
//! handed.
//!
//! It sits above [`datalib_etl`] rather than inside it because this is
//! the only side that knows `datalib_schema` — the `grid_rows` /
//! `edges` / `markdowns` tables the UI reads. A downloader links
//! `datalib_etl` and stops there, so moving a `grid_rows` column no
//! longer rebuilds the code that fetches from Slack.

pub mod grid_index;
pub mod indexed_markdown;
pub mod processor;
pub mod section;
