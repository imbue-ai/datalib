//! The render wave for the perseus source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_perseus_config::PerseusRenderConfig;
use datalib_etl_render::inputs::Input;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

pub fn plan_render(
    ctx: PlanContext,
    config: PerseusRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let pairs: Vec<(String, String)> = config
        .alignment_pairs
        .iter()
        .map(|[a, b]| (a.clone(), b.clone()))
        .collect();
    // Perseus renders straight from its input tree: that is the path
    // the processor carries.
    Ok(plan_source_render(
        ctx,
        config.common.input_or_raw_path(),
        PerseusRender { pairs },
    ))
}

struct PerseusRender {
    pairs: Vec<(String, String)>,
}

#[async_trait]
impl SourceRender for PerseusRender {
    const PROVIDER: &'static str = "perseus";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    async fn run(&self, input_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{align, parse, render};
        let parsed = parse::parse(input_path)
            .with_context(|| format!("perseus parse {}", input_path.display()))?;
        // Within-section sentence alignment is opt-in and dominates runtime; it
        // is async (model load + hf-hub fetch). We're driven by `futures`'
        // executor (the render phase), which enters no tokio context, so we
        // drive the async aligner with tokio's `block_on` here — the same shape
        // the old synchronous renderer used.
        let alignments = tokio::runtime::Handle::current()
            .block_on(align::align_all(&parsed, &self.pairs))
            .context("perseus align_all")?;
        let inputs: Vec<Input> = parsed
            .files
            .iter()
            .map(|f| Input::new("file", f.clone()))
            .collect();
        ctx.declare_bucket(&render::bucket_key(ctx.name), &inputs)?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render::render_all(
            &parsed,
            &alignments,
            ctx.root,
            ctx.name,
            ctx.progress,
            &mut on_doc,
        )
        .context("perseus render_all")?;
        Ok("rendered".into())
    }
}
