//! The `qmd_index` function: the qmd search index over every
//! `render_markdown` tree, written to `unified_index/qmd_index`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::events::{Emitter, OutputClaim};

/// The one tree this step writes, as the applet that reads it resolves
/// it from the data root.
pub fn out_rel() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_INDEX_DIR,
        datalib_core::layout::QMD_DIR
    )
}

pub async fn run(
    data_root: &Path,
    models_dir: Option<PathBuf>,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("qmd index");
    let mut opts = datalib_qmd_indexer::IndexOptions::new(data_root);
    if let Some(d) = models_dir {
        opts.models_dir = d;
    }
    // run_index shells out to qmd; blocking work.
    let outcome = tokio::task::spawn_blocking(move || datalib_qmd_indexer::run_index(&opts))
        .await
        .context("qmd task panicked")??;
    tracing::info!(index = %outcome.index_path.display(), "qmd: done");
    // The index rebuilds from the render_markdown trees, so cache-aware
    // backups (`restic --exclude-caches` etc.) may skip it. Tag the
    // whole `unified_index/` tree for the same reason the grid step
    // does — one tag covers both indexes however they are ordered.
    datalib_core::layout::mark_derived_cache(&datalib_core::layout::unified_index_dir(data_root));

    // qmd's sqlite gets touched on every pass, so any version we could
    // derive would move even when nothing was indexed. Report nothing:
    // the step is a leaf (nothing consumes unified_index/qmd_index
    // downstream), so the runner's fallback hash is never read by anyone
    // and the imprecision costs nothing.
    Ok(vec![])
}
