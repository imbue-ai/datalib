//! The render wave for the yolink source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use datalib_etl_yolink_config::YolinkRenderConfig;
use std::path::PathBuf;

/// Render wave: always present. Renders whatever is in the raw store
/// into the single timeseries page (see [`crate::render`]); the page's
/// own HEAD-vs-cursor check decides whether there is work to do, so
/// planning it unconditionally costs one `dolt_log()` query on a
/// no-op run.
pub fn plan_render(
    ctx: PlanContext,
    config: YolinkRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(YolinkRender {
        id: format!("yolink/{name}/render"),
        raw_path,
        name,
    })])
}

struct YolinkRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for YolinkRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::parse::{parse, Parsed};
        use crate::render::render::{cursor_params, render_all};

        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, &self.name);
        let cursor = datalib_etl::render_cursor::read_for_params(&cursor_path, &cursor_params())
            .with_context(|| format!("read yolink render cursor {}", cursor_path.display()))?;

        match parse(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("yolink parse {}", self.raw_path.display()))?
        {
            // The whole store is one document, so an unchanged HEAD means
            // an unchanged page — nothing was appended, nothing to draw.
            Parsed::UpToDate { head } => {
                tracing::info!(
                    event = "yolink_render_skipped",
                    source = %self.name,
                    head = %head,
                    "raw store HEAD unchanged since last render",
                );
                Ok(format!("up to date at {head}"))
            }
            Parsed::Fresh(parsed) => {
                let mut on_doc = |md| ctx.emit_doc(md);
                let s = render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
                    .context("yolink render_all")?;
                Ok(format!(
                    "devices={} series={} points={} plots={}",
                    s.devices, s.series, s.points, s.plots,
                ))
            }
        }
    }
}
