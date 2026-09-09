//! The render wave for the perseus source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_perseus_config::PerseusRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderPass, RenderProcessor};
use std::path::PathBuf;

pub fn plan_render(
    ctx: PlanContext,
    config: PerseusRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let input_path = config.common.input_or_raw_path().to_path_buf();
    let pairs: Vec<(String, String)> = config
        .alignment_pairs
        .iter()
        .map(|[a, b]| (a.clone(), b.clone()))
        .collect();
    Ok(vec![Box::new(PerseusRender {
        id: format!("perseus/{name}/render"),
        input_path,
        name,
        pairs,
    })])
}

struct PerseusRender {
    id: String,
    input_path: PathBuf,
    name: String,
    pairs: Vec<(String, String)>,
}

#[async_trait]
impl RenderProcessor for PerseusRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
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
        // This renderer walks the whole raw store every run, so the set it
        // considered is the complete one: anything else the render store
        // holds is a document whose source is gone. The driver sweeps.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut on_doc = |md| ctx.emit_doc(md);
        render::render_all(
            &parsed,
            &alignments,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut seen,
        )
        .context("perseus render_all")?;
        // `render_all` has no early return: reaching here means it walked.
        ctx.retain_documents(RenderPass::Walked, &seen);
        Ok("rendered".into())
    }
}
