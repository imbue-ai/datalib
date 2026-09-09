//! JMAP render: read raw doltlite db → render one markdown document
//! per JMAP Thread, plus its rows in the render store, plus the thread's
//! attachment blobs materialized at `<thread>/blobs/<safe_filename>`.

pub mod parse;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident: the directory is the pipeline STAGE (mirroring
// `download/`), and the file is the rendering step within it, beside
// `parse.rs`. Renaming it would break the symmetry in all twelve
// providers. Allowed here rather than repo-wide so an unintentional
// inception elsewhere still fails the build.
#[allow(clippy::module_inception)]
pub mod render;

/// The render version stamped onto each doc's `markdowns` row — now owned by the
/// renderer (which drives chat-common). Re-exported for callers that
/// referenced `render::RENDER_VERSION`.
pub use render::RENDER_VERSION;
