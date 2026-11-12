//! The render half of the ETL framework: the per-source render store,
//! the document-index load, and the run context a render processor is
//! handed.
//!
//! It sits above [`datalib_etl`] rather than inside it because this is
//! the only side that knows `datalib_schema` — the `grid_rows` /
//! `edges` / `markdowns` tables the UI reads. A downloader links
//! `datalib_etl` and stops there, so moving a `grid_rows` column no
//! longer rebuilds the code that fetches from Slack.

pub mod diff;
pub mod front_matter;
pub mod grid_index;
pub mod html;
pub mod indexed_markdown;
pub mod inputs;
pub mod message;
#[allow(dead_code)]
mod names;
pub mod not_read;
pub mod pages;
pub mod photo;
pub mod processor;
pub mod search_terms;
pub mod section;
pub mod sources;
pub mod title;

/// Bump when what this crate writes into every source's documents
/// changes — a shared writer (`title`, `message`, `html`,
/// `front_matter`) or what `names` settles — so every source renders
/// again. It rides in every source's render params; a provider's own
/// change bumps its own version.
pub const SHARED_RULES_VERSION: u32 = 1;
