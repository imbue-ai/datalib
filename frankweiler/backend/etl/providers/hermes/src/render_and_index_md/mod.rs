//! Parse Hermes/OpenClaw exports and render them to Markdown + `grid_rows`.
//!
//! `parse` turns the export directory into structured [`parse::HermesSession`]s;
//! `render` normalizes those into the shared [`frankweiler_etl_chat_common`]
//! chat model and delegates all Markdown / grid-row / sidecar plumbing to it.

pub mod parse;
pub mod render;
