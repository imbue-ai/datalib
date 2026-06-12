//! Anthropic Translate: raw API capture → parsed rows → markdown +
//! grid_rows sidecars. Stages 3-4 fill in render + sidecar emit;
//! `parse` is in place.

pub mod blob_reader;
pub mod grid_rows;
pub mod parse;
pub mod render;
