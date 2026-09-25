//! The render wave for the `garmin` source.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_garmin_config::GarminRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};

/// Always planned: the driver's reverse lookup says whether the page's
/// tables moved, so a no-op run costs one `dolt_log()` query.
pub fn plan_render(
    ctx: PlanContext,
    config: GarminRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        GarminRender,
    ))
}

struct GarminRender;

#[async_trait]
impl SourceRender for GarminRender {
    const PROVIDER: &'static str = "garmin";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::parse::{inputs, parse};
        use crate::render::render::{document_uuid, render_all};

        let range = ctx.raw_range();
        let page = document_uuid(ctx.name);
        if let (Some(pin), false) = (range.pin, range.is_stale(&page)) {
            tracing::info!(
                event = "garmin_render_skipped",
                source = %ctx.name,
                head = %pin,
                "nothing the page reads changed since the last render",
            );
            ctx.consumed(pin);
            return Ok(format!("up to date at {pin}"));
        }
        let parsed = parse(raw_path, range)
            .with_context(|| format!("garmin parse {}", raw_path.display()))?;
        ctx.declare_bucket(&page, &inputs())?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_all(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("garmin render_all")?;
        if let Some(head) = parsed.head.as_deref() {
            ctx.consumed(head);
        }
        Ok(format!(
            "weigh_ins={} devices={} plots={}",
            s.weigh_ins, s.devices, s.plots
        ))
    }
}
