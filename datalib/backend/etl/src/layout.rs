//! Per-group on-disk layout. Every artifact a group (data source)
//! produces is under `<data_root>/<group>/`, one directory per step
//! function, so a source's whole footprint is one self-contained subtree.

use std::path::{Path, PathBuf};

/// The ingest step's function, and so the directory its raw store lives
/// in: `<data_root>/<group>/ingest`.
pub const INGEST_DIR: &str = "ingest";
/// The markdown render step's function, and so the directory it writes:
/// `<data_root>/<group>/render_markdown`.
pub const RENDER_MARKDOWN_DIR: &str = "render_markdown";

pub fn stanza_dir(data_root: &Path, stanza: &str) -> PathBuf {
    data_root.join(stanza)
}

pub fn ingest_root(data_root: &Path, stanza: &str) -> PathBuf {
    stanza_dir(data_root, stanza).join(INGEST_DIR)
}

pub fn render_markdown_root(data_root: &Path, stanza: &str) -> PathBuf {
    stanza_dir(data_root, stanza).join(RENDER_MARKDOWN_DIR)
}
