//! JMAP translate: read raw doltlite db → render one markdown document
//! per JMAP Thread, plus a `grid_rows.json` sidecar, plus the thread's
//! attachment blobs materialized at `<thread>/blobs/<safe_filename>`.

pub mod parse;
pub mod render;

/// The render version stamped into each doc's sidecar — now owned by the
/// renderer (which drives chat-common). Re-exported for callers that
/// referenced `render_and_index_md::RENDER_VERSION`.
pub use render::RENDER_VERSION;
