// A fixture tool: its output is a status line on stderr, and it owns no
// MultiProgress. Exempt from the workspace-wide ban in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! The fixture's qmd index, built with the library a sync uses: register
//! every source, keyword-index them, then embed them all in one call, so
//! the model loads once. Run by `build_qmd_index.py`.
//!
//! argv: <root> <node> <qmd package dir> <models dir> <group>...

use std::path::Path;

use anyhow::{bail, Result};
use datalib_qmd_indexer::{Index, Qmd};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [root, node, package, models, groups @ ..] = args.as_slice() else {
        bail!("usage: build_qmd_index <root> <node> <qmd package dir> <models dir> <group>...");
    };
    let index = Index::open(Path::new(root), Qmd::new(node, package))?;
    index.link_models(Path::new(models))?;
    index.register(groups)?;
    let updated = index.keyword_index(groups, &|_| {})?;
    let embedded = index.embed(groups, &|_| {})?;
    eprintln!("[build_qmd_index] {updated}; {embedded}");
    if embedded.errors > 0 {
        bail!("{} chunk(s) failed to embed", embedded.errors);
    }
    Ok(())
}
