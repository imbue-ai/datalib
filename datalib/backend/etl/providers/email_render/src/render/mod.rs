//! JMAP render: read raw doltlite db → render one markdown document
//! per JMAP Thread, plus its rows in the render store. Its attachments
//! are linked inside the stored `.eml`s, never copied beside the page.

pub mod ids;
pub mod mentions;
mod padding;
pub mod parse;
// `render/render.rs` inside `render/` is the layout every provider's
// render crate shares, not an accident: the directory is the render
// stage, and the file is the rendering step within it, beside
// `parse.rs`. Allowed here rather than repo-wide so a module named like
// its parent anywhere else still fails clippy's `module_inception`.
#[allow(clippy::module_inception)]
pub mod render;

/// The render version stamped onto each doc's `markdowns` row, defined by the
/// renderer (which drives chat-common) and re-exported here as
/// `render::RENDER_VERSION`.
pub use render::RENDER_VERSION;
