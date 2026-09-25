//! The render wave for the airvisual source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_airvisual_config::AirvisualRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use datalib_etl_timeseries_render::page::skip_if_current;
use std::path::Path;

/// Always planned: the driver's reverse lookup says whether the page's
/// tables moved, so a no-op run costs one `dolt_log()` query.
pub fn plan_render(
    ctx: PlanContext,
    config: AirvisualRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        AirvisualRender,
    ))
}

struct AirvisualRender;

#[async_trait]
impl SourceRender for AirvisualRender {
    const PROVIDER: &'static str = "airvisual";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::parse::{inputs, parse};
        use crate::render::render::{document_uuid, render_all};

        let page = document_uuid(ctx.name);
        if let Some(done) = skip_if_current(ctx, Self::PROVIDER, &page) {
            return Ok(done);
        }
        let parsed = parse(raw_path, ctx.raw_range())
            .with_context(|| format!("airvisual parse {}", raw_path.display()))?;
        ctx.declare_bucket(&page, &inputs())?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_all(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("airvisual render_all")?;
        if let Some(head) = parsed.head.as_deref() {
            ctx.consumed(head);
        }
        Ok(format!(
            "devices={} series={} points={} plots={}",
            s.devices, s.series, s.points, s.plots,
        ))
    }
}
