//! Program-A `DataProcessor`s for the perseus source (download + render).

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_perseus_config::PerseusConfig;

use crate::download;

// Perseus is genuinely file-tree-backed — it reads TEI `.xml` directly,
// with no doltlite store. The ingest tree *is* the TEI tree: `github`
// fetches into it, and render reads it (or a tree staged by hand, named
// on the render step's `common.input_path`).

/// Download wave: present iff `github` — fetch the TEI files.
pub fn plan_download(
    ctx: PlanContext,
    config: PerseusConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let input_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.github {
        if !sync.alignment_pairs.is_empty() {
            anyhow::bail!(
                "perseus `github.alignment_pairs` is a render knob — put \
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
