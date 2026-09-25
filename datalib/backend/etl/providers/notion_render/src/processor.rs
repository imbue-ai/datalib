//! The render wave for the notion source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_notion_config::NotionRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: NotionRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        NotionRender,
    ))
}

struct NotionRender;

#[async_trait]
impl SourceRender for NotionRender {
    const PROVIDER: &'static str = "notion";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse_api_dir, render::render_notion};
        let parsed = parse_api_dir(raw_path, ctx.raw_range())
            .with_context(|| format!("notion parse {}", raw_path.display()))?;
        // Every bucket this run renders — page or thread — is declared
        // first with nothing, so one whose rows are gone loses its
        // documents; the render below re-declares the ones it produced.
        for key in parsed.render.iter().flatten() {
            ctx.declare_bucket(key, &[])?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_notion(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("render_notion")?;
        ctx.finish(&s.buckets, parsed.head.as_deref())?;
        Ok(format!("rendered {} document(s)", s.rendered))
    }
}
