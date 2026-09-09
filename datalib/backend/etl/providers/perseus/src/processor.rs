//! Program-A `DataProcessor`s for the perseus source (download + render).

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_perseus_config::PerseusConfig;

use crate::download;

// Perseus is genuinely file-tree-backed — it reads TEI `.xml` directly,
// with no doltlite store, so `raw_path` (our store dir) has no meaning
// here. Both waves key off the input path (the TEI tree): download fetches
// into it, render reads from it. For a managed source `input_path:` is
// unset and `input_or_raw_path()` falls back to `<data_root>/raw/perseus`;
// without a `sync:` block it is the pre-staged tree named by `input_path:`.

/// Download wave: present iff `sync:` — fetch the TEI files; otherwise
/// nothing is fetched and render reads the files already on disk.
pub fn plan_download(
    ctx: PlanContext,
    config: PerseusConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let input_path = config.common.input_or_raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        if !sync.alignment_pairs.is_empty() {
            anyhow::bail!(
                "perseus `sync.alignment_pairs` is a render knob — put \
                 `alignment_pairs` in the render step's params instead"
            );
        }
        procs.push(Box::new(PerseusDownload {
            id: format!("perseus/{name}/download"),
            input_path,
            files: sync.files,
        }));
    }
    Ok(procs)
}

struct PerseusDownload {
    id: String,
    input_path: PathBuf,
    files: Vec<String>,
}

#[async_trait]
impl DataProcessor for PerseusDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // File-tree-backed: no pool, no checkpoint, no commit.
        let s = download::fetch(download::FetchOptions {
            out_dir: self.input_path.clone(),
            files: self.files.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        Ok(format!(
            "fetched={} skipped={} bytes={} requests={}",
            s.fetched, s.skipped, s.bytes, s.requests,
        ))
    }
}
