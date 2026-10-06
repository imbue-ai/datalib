//! The render wave for the yolink source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use datalib_etl_timeseries_render::page::skip_if_current;
use datalib_etl_yolink_config::YolinkRenderConfig;
use std::path::Path;

/// Always planned: the driver's reverse lookup says whether the page's
/// tables moved, so a no-op run costs one `dolt_log()` query.
pub fn plan_render(
    ctx: PlanContext,
    config: YolinkRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        YolinkRender,
    ))
}

struct YolinkRender;

#[async_trait]
impl SourceRender for YolinkRender {
    const PROVIDER: &'static str = "yolink";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    /// A history window that would not fetch is its device's.
    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        use datalib_etl_yolink::ingest::schema_raw::{device_of_window_id, YOLINK_WINDOWS_TABLE};
        if table != YOLINK_WINDOWS_TABLE {
            return None;
        }
        let device = device_of_window_id(id)?;
        Some(crate::render::render::device_uuid(source_id, device))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::parse::{inputs, parse};
        use crate::render::render::{document_uuid, render_all};

        let page = document_uuid(ctx.name);
        if let Some(done) = skip_if_current(ctx, Self::PROVIDER, &page) {
            return Ok(done);
        }
        let parsed = parse(raw_path, ctx.raw_range())
            .with_context(|| format!("yolink parse {}", raw_path.display()))?;
        ctx.declare_bucket(&page, &inputs())?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_all(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("yolink render_all")?;
        if let Some(head) = parsed.head.as_deref() {
            ctx.consumed(head);
        }
        Ok(format!(
            "devices={} series={} points={} plots={}",
            s.devices, s.series, s.points, s.plots,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_yolink::ingest::schema_raw::window_id_recipe;

    /// A failed window's `problems` row reaches the device it belongs to.
    #[test]
    fn a_window_is_its_devices_row() {
        let item = |table, id| YolinkRender.item_of_entity("src", table, id);
        let window = window_id_recipe("cargo-bay-2", 1, 2);
        assert_eq!(
            item("yolink_windows", &window),
            Some(crate::render::render::device_uuid("src", "cargo-bay-2"))
        );
        assert_eq!(item("yolink_readings", &window), None);
        assert_eq!(item("yolink_windows", "no-separator"), None);
    }
}
