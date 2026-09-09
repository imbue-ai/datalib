//! Signal render stage.

pub mod normalize;
pub mod parse;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident: the directory is the pipeline STAGE (mirroring
// `download/`), and the file is the rendering step within it, beside
// `parse.rs`. Renaming it would break the symmetry in all twelve
// providers. Allowed here rather than repo-wide so an unintentional
// inception elsewhere still fails the build.
#[allow(clippy::module_inception)]
pub mod render;

// The UUIDv5 identity recipes live in `download::schema_raw` (identity
// recipes belong next to the schema). Re-export so existing
// `crate::render::signal_*` callers keep resolving.
pub use datalib_etl_signal::download::schema_raw::{
    signal_chat_uuid, signal_markdown_uuid, signal_message_uuid, signal_recipient_uuid,
    SIGNAL_UUID_NS,
};

pub use datalib_etl::periodize::Period;
pub use parse::{parse, parse_raw_dir, ParsedSignal};
pub use render::{render_all, render_params, RenderSummary, RENDER_VERSION};
