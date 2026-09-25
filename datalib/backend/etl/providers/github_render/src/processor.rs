//! The render wave for the github source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_github_config::GithubRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: GithubRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        GithubRender,
    ))
}

struct GithubRender;

#[async_trait]
impl SourceRender for GithubRender {
    const PROVIDER: &'static str = "github";

    fn render_version(&self) -> u32 {
        crate::render::grid_rows::RENDER_VERSION
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse_api_dir, render_github};
        let parsed = parse_api_dir(raw_path, ctx.name, ctx.raw_range())
            .with_context(|| format!("github parse {}", raw_path.display()))?;
        // Every PR this run renders is declared first with nothing, so
        // one whose row is gone loses its document; the render below
        // re-declares the ones it produced.
        for key in parsed.render.iter().flatten() {
            ctx.declare_bucket(key, &[])?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_github(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("render_github")?;
        ctx.finish(&s.buckets, parsed.head.as_deref())?;
        Ok(format!("rendered={}", s.rendered))
    }
}
