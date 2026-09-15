//! The render wave for the airvisual source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_airvisual_config::AirvisualRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

/// Always planned: the driver's reverse lookup says whether the page's
/// tables moved, so a no-op run costs one `dolt_log()` query.
pub fn plan_render(
    ctx: PlanContext,
    config: AirvisualRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(AirvisualRender {
        id: format!("airvisual/{name}/render"),
        raw_path,
        name,
    })])
}

struct AirvisualRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for AirvisualRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::parse::{inputs, parse};
        use crate::render::render::{document_uuid, render_all};

        let range = ctx.raw_range();
        let page = document_uuid(&self.name);
        if let (Some(pin), false) = (range.pin, range.is_stale(&page)) {
            tracing::info!(
                event = "airvisual_render_skipped",
                source = %self.name,
                head = %pin,
                "nothing the page reads changed since the last render",
            );
            ctx.consumed(pin);
            return Ok(format!("up to date at {pin}"));
        }
        let parsed = parse(&self.raw_path, range)
            .with_context(|| format!("airvisual parse {}", self.raw_path.display()))?;
        ctx.declare_bucket(&page, &inputs())?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
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
