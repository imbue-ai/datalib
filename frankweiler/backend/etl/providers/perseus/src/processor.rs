//! Program-A `DataProcessor`s for the perseus source (extract + translate).
//!
//! Perseus is **file-tree-backed**: extract writes raw TEI XML to disk (no
//! doltlite store), so its extract processor opens no pool, registers no
//! `Checkpoint`, and issues no commit. Translate parses the on-disk tree and
//! (optionally) runs sentence alignment before rendering.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx, SourcePlan};
use frankweiler_etl_perseus_config::PerseusConfig;

use crate::extract;

pub fn plan(ctx: PlanContext, config: PerseusConfig) -> Result<SourcePlan> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
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
    let mut plan = SourcePlan::new();
    plan.translate.push(Box::new(PerseusRender {
        id: format!("perseus/{name}/translate"),
        raw_path: raw_path.clone(),
        name: name.clone(),
        pairs,
    }));
    // Managed (has a `sync:` block) → fetch the TEI files; otherwise the source
    // is translate-only over files already on disk.
    if let Some(sync) = config.sync {
        plan.extract.push(Box::new(PerseusExtract {
            id: format!("perseus/{name}/extract"),
            raw_path,
            files: sync.files,
        }));
    }
    Ok(plan)
}

struct PerseusExtract {
    id: String,
    raw_path: PathBuf,
    files: Vec<String>,
}

#[async_trait]
impl DataProcessor for PerseusExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // File-tree-backed: no pool, no checkpoint, no commit.
        let s = extract::fetch(extract::FetchOptions {
            out_dir: self.raw_path.clone(),
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
    raw_path: PathBuf,
    name: String,
    pairs: Vec<(String, String)>,
}

#[async_trait]
impl DataProcessor for PerseusRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render_and_index_md::{align, parse, render};
        let parsed = parse::parse(&self.raw_path)
            .with_context(|| format!("perseus parse {}", self.raw_path.display()))?;
        // Within-section sentence alignment is opt-in and dominates runtime; it
        // is async (model load + hf-hub fetch). We're driven by `futures`'
        // executor (the translate phase), which enters no tokio context, so we
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
