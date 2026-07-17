//! The `qmd_index` step type: the qmd search index over every
//! rendered_md tree, writing `system/qmd`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::events::{Emitter, OutputClaim};

pub const OUT_REL: &str = "system/qmd";

pub async fn run(
    data_root: &Path,
    models_dir: Option<PathBuf>,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("qmd index");
    let mut opts = frankweiler_qmd_indexer::IndexOptions::new(data_root);
    if let Some(d) = models_dir {
        opts.models_dir = d;
    }
    // run_index shells out to qmd; blocking work.
    let outcome = tokio::task::spawn_blocking(move || frankweiler_qmd_indexer::run_index(&opts))
        .await
        .context("qmd task panicked")??;
    tracing::info!(index = %outcome.index_path.display(), "qmd: done");

    // qmd's sqlite gets touched on every pass, so a content hash would
    // always read "changed"; and qmd has no incremental-change signal
    // we can cheaply expose. Claim nothing beyond having run — the
    // step is a leaf (nothing consumes system/qmd downstream), so the
    // imprecision costs nothing today.
    Ok(vec![OutputClaim {
        path: OUT_REL.to_string(),
        changed: None,
        version: None,
    }])
}
