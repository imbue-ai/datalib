//! The render wave for the airvisual source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_airvisual_config::AirvisualRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

/// Render wave: always present. Renders whatever is in the raw store
/// into the single timeseries page (see [`crate::render`]); the page's
/// own HEAD-vs-cursor check decides whether there is work to do, so
/// planning it unconditionally costs one `dolt_log()` query on a
/// no-op run.
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
        use crate::render::parse::{parse, Parsed};
        use crate::render::render::render_all;

        match parse(&self.raw_path, ctx.raw_cursor)
            .with_context(|| format!("airvisual parse {}", self.raw_path.display()))?
        {
            // The whole store is one document, so an unchanged HEAD means
            // an unchanged page — nothing was appended, nothing to draw.
            Parsed::UpToDate { head } => {
                tracing::info!(
                    event = "airvisual_render_skipped",
                    source = %self.name,
                    head = %head,
                    "raw store HEAD unchanged since last render",
                );
                ctx.consumed(&head);
                Ok(format!("up to date at {head}"))
            }
            Parsed::Fresh(parsed) => {
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
    }
}
