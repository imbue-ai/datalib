//! Render stage: read the raw store written by [`datalib_etl_notion::ingest`] and
//! emit one document per Notion page, one per comment thread, and one
//! table of contents.

mod enhanced;
pub mod grid_rows;
pub mod ids;
pub mod parse;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident: the directory is the pipeline STAGE (mirroring
// `download/`), and the file is the rendering step within it, beside
// `parse.rs`. Renaming it would break the symmetry in all twelve
// providers. Allowed here rather than repo-wide so an unintentional
// inception elsewhere still fails the build.
#[allow(clippy::module_inception)]
pub mod render;
pub mod toc;

pub use parse::{parse_api_dir, ParsedNotion};
