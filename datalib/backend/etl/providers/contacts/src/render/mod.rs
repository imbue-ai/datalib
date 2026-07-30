//! vCard → grid_rows + rendered markdown.
//!
//! Render reads `.vcf` files from a directory tree of the shape
//!   `<input_path>/<addressbook_dir>/<some_name>.vcf`
//! and emits one rendered markdown file + one grid row per contact.
//! The directory name (the addressbook label) becomes the `channel`
//! column on each row so the UI can group all contacts in one
//! addressbook together.
//!
//! This path also works for the test pipeline: a config that omits
//! the `sync:` block is render-only (same shape as
//! `SourceConfig::ClaudeExport`), so a checked-in fixture full of
//! vCards renders without any CardDAV server in the loop.
//!
//! The UUID derivation is upstream-stable: `contact_uuid(account,
//! addressbook, uid)` derives the same id whether the vCard came
//! over CardDAV or off disk.

pub mod parse;
pub mod render;

// The UUIDv5 identity recipes live in `download::schema_raw` (identity
// recipes belong next to the schema). Re-export so
// `crate::render::{contact_uuid, addressbook_uuid}` callers — here
// and in `render.rs` — keep resolving.
pub use super::download::schema_raw::{addressbook_uuid, contact_uuid};
