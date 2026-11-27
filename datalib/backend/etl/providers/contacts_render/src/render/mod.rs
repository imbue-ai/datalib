//! A stored card, vCard or Google's, → grid_rows + rendered markdown.

pub mod google;
pub mod parse;
// `render/render.rs` inside `render/` is the layout every provider's
// render crate shares, not an accident: the directory is the render
// stage, and the file is the rendering step within it, beside
// `parse.rs`. Allowed here rather than repo-wide so a module named like
// its parent anywhere else still fails clippy's `module_inception`.
pub mod ids;
#[allow(clippy::module_inception)]
pub mod render;
