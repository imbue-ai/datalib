//! Translate stage: read the event-store JSONL written by
//! [`crate::extract`] and emit one markdown document per MR plus a
//! co-located `.grid_rows.json` sidecar.

pub mod grid_rows;
pub mod parse;
pub mod render;

pub use parse::{parse_api_dir, MergeRequestRow, NoteRow, ParsedGitlabApi};
pub use render::{render_gitlab, RenderSummary};
