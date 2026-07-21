//! Render stage: read the event-store JSONL written by
//! [`crate::download`] and emit one markdown + one `.grid_rows.json`
//! sidecar per Notion page.

pub mod grid_rows;
pub mod parse;
pub mod render;

pub use parse::{parse_api_dir, ParsedNotionOfficial};
