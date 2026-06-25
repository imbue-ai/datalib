//! Per-stanza on-disk layout. Every artifact a config stanza (data source)
//! produces is grouped under `<data_root>/<stanza>/`, so a source's whole
//! footprint is one self-contained subtree:
//!
//! ```text
//! <data_root>/<stanza>/raw/…           (extract — see source_common)
//! <data_root>/<stanza>/rendered_md/…   (translate — markdown + sidecars)
//! ```
//!
//! Cross-stanza aggregates (`backend_index.doltlite_db`, `qmd/`) live at the
//! top of `<data_root>`, not under any one stanza.
//!
//! Path components below `rendered_md/` are UUID/canonical-id derived, never
//! slug/title derived, so an upstream rename re-renders in place instead of
//! orphaning the old file at a now-stale path.

use std::path::{Path, PathBuf};

// Re-exported so the orchestrator (`frankweiler-sync`, which deliberately does
// not depend on `frankweiler_core`) can mark the derived index dirs as cache
// through the one crate it does depend on.
pub use frankweiler_core::layout::{backend_index_dir, mark_derived_cache, qmd_dir};

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
