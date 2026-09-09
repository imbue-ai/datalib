//! vCard → grid_rows + rendered markdown.

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
// recipes belong next to the schema). Re-export so
// `crate::render::{contact_uuid, addressbook_uuid}` callers — here
// and in `render.rs` — keep resolving.
pub use datalib_etl_contacts::download::schema_raw::{addressbook_uuid, contact_uuid};
