//! Render stage: read the event-store JSONL written by
//! [`crate::download`] and emit one markdown document per MR plus a
//! co-located `.grid_rows.json` sidecar.

pub mod grid_rows;
pub mod parse;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident: the directory is the pipeline STAGE (mirroring
// `download/`), and the file is the rendering step within it, beside
// `parse.rs`. Renaming it would break the symmetry in all twelve
// providers. Allowed here rather than repo-wide so an unintentional
// inception elsewhere still fails the build.
#[allow(clippy::module_inception)]
pub mod render;

pub use parse::{parse_api_dir, MergeRequestRow, NoteRow, ParsedGitlabApi};
pub use render::{render_gitlab, RenderSummary};
