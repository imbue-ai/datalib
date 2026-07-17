//! Program-A `DataProcessor`s for the perseus source (download + render).
//!
//! Perseus is **file-tree-backed**: download writes raw TEI XML to disk (no
//! doltlite store), so its download processor opens no pool, registers no
//! `Checkpoint`, and issues no commit. Render parses the on-disk tree and
//! (optionally) runs sentence alignment before rendering.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_perseus_config::PerseusConfig;

use crate::download;

// Perseus is genuinely file-tree-backed — it reads TEI `.xml` directly,
// with no doltlite store, so `raw_path` (our store dir) has no meaning
// here. Both waves key off the input path (the TEI tree): download fetches
// into it, render reads from it. For a managed source `input_path:` is
// unset and `input_or_raw_path()` falls back to `<data_root>/raw/perseus`;
// for a render-only source it's the pre-staged tree named by `input_path:`.

/// Download wave: present iff `sync:` — fetch the TEI files; otherwise
/// the source is render-only over files already on disk.
pub fn plan_download(
    ctx: PlanContext,
    config: PerseusConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let input_path = config.common.input_or_raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(PerseusDownload {
            id: format!("perseus/{name}/download"),
            input_path,
            files: sync.files,
        }));
    }
    Ok(procs)
}

/// Render wave. NOTE: reads `sync.alignment_pairs` — a render-relevant
/// knob that historically lives in the `sync:` block; it moves out in
/// the config-format split.
pub fn plan_render(ctx: PlanContext, config: PerseusConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let input_path = config.common.input_or_raw_path().to_path_buf();
    let pairs: Vec<(String, String)> = config
        .sync
        .as_ref()
        .map(|s| {
            s.alignment_pairs
                .iter()
                .map(|[a, b]| (a.clone(), b.clone()))
                .collect()
        })
        .unwrap_or_default();
    Ok(vec![Box::new(PerseusRender {
        id: format!("perseus/{name}/render"),
        input_path,
        name,
        pairs,
    })])
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

struct PerseusRender {
    id: String,
    input_path: PathBuf,
    name: String,
    pairs: Vec<(String, String)>,
}

#[async_trait]
impl DataProcessor for PerseusRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::{align, parse, render};
        let parsed = parse::parse(&self.input_path)
            .with_context(|| format!("perseus parse {}", self.input_path.display()))?;
        // Within-section sentence alignment is opt-in and dominates runtime; it
        // is async (model load + hf-hub fetch). We're driven by `futures`'
        // executor (the render phase), which enters no tokio context, so we
        // drive the async aligner with tokio's `block_on` here — the same shape
        // the old synchronous renderer used.
        let alignments = tokio::runtime::Handle::current()
            .block_on(align::align_all(&parsed, &self.pairs))
            .context("perseus align_all")?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render::render_all(
            &parsed,
            &alignments,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("perseus render_all")?;
        Ok("rendered".into())
    }
}
