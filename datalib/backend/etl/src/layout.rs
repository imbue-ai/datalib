//! Per-stanza on-disk layout. Every artifact a config stanza (data source)
//! produces is grouped under `<data_root>/<stanza>/`, so a source's whole
//! footprint is one self-contained subtree:

use std::path::{Path, PathBuf};

pub fn stanza_dir(data_root: &Path, stanza: &str) -> PathBuf {
    data_root.join(stanza)
}

pub fn rendered_md_root(data_root: &Path, stanza: &str) -> PathBuf {
    stanza_dir(data_root, stanza).join("rendered_md")
}
