//! ChatGPT Render: raw API capture → parsed rows → markdown +
//! grid_rows into the source's render store.

/// Every entity id this provider mints. See `docs/dev/entity_ids.md`.
pub mod ids;
pub mod parse;
// `render/render.rs` inside `render/` is the layout the providers'
// render crates share: the directory is the render stage, and the file
// is the rendering within it, beside `parse.rs`. Clippy's
// `module_inception` is allowed here rather than repo-wide, so a module
// named after its parent by mistake elsewhere still fails the build.
#[allow(clippy::module_inception)]
pub mod render;
pub mod sentinels;
