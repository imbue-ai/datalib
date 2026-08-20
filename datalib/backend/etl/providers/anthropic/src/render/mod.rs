//! Anthropic Render: raw API capture → parsed rows → markdown +
//! grid_rows sidecars. Stages 3-4 fill in render + sidecar emit;
//! `parse` is in place.

pub mod parse;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident: the directory is the pipeline STAGE (mirroring
// `download/`), and the file is the rendering step within it, beside
// `parse.rs`. Renaming it would break the symmetry in all twelve
// providers. Allowed here rather than repo-wide so an unintentional
// inception elsewhere still fails the build.
#[allow(clippy::module_inception)]
pub mod render;
