//! What changed on disk, for a source that reads local files: walk a tree,
//! hash only what the host cache cannot vouch for, and keep each feed's
//! resume cursor. `README.md` beside this crate has the rules.

pub mod export_files;
pub mod file_checkpoint;
pub mod fingerprint_cache;
pub mod fsscan;
pub mod fswalk;
