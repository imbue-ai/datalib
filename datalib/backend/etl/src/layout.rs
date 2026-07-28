//! Per-stanza on-disk layout. Every artifact a config stanza (data source)
//! produces is grouped under `<data_root>/<stanza>/`, so a source's whole
//! footprint is one self-contained subtree:
//!
//! ```text
//! <data_root>/<stanza>/raw/…           (download — see source_common)
//! <data_root>/<stanza>/rendered_md/…   (render — markdown + sidecars)
//! ```
//!
//! Cross-stanza aggregates (`backend_index/db.doltlite_db`, `qmd/`) live
//! under `<data_root>/system/`, not under any one stanza.
//!
//! Path components below `rendered_md/` are UUID/canonical-id derived, never
//! slug/title derived, so an upstream rename re-renders in place instead of
//! orphaning the old file at a now-stale path.

use std::path::{Path, PathBuf};

/// Directory holding everything one stanza produces: `<data_root>/<stanza>`.
pub fn stanza_dir(data_root: &Path, stanza: &str) -> PathBuf {
    data_root.join(stanza)
}

/// Root of a stanza's rendered-markdown tree:
/// `<data_root>/<stanza>/rendered_md`. The single place the `rendered_md`
/// literal lives; every renderer builds its content hierarchy under this.
pub fn rendered_md_root(data_root: &Path, stanza: &str) -> PathBuf {
    stanza_dir(data_root, stanza).join("rendered_md")
}
