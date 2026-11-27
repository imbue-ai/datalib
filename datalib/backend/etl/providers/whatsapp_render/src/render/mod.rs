//! WhatsApp render stage.

pub mod ids;
pub mod parse;
// `render/render.rs` inside `render/` is the layout several render crates
// share, not an accident: the directory is the render stage, and the
// file is the rendering step within it, beside `parse.rs`. Allowed
// here rather than repo-wide so an unintentional inception elsewhere
// still fails the build.
#[allow(clippy::module_inception)]
pub mod render;

pub use parse::parse;
pub use render::{render_all, RENDER_VERSION};
