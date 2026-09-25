//! The render wave for the `calendar` source: its planner and the
//! [`RenderProcessor`] it plans.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_calendar::ingest;
use datalib_etl_calendar_config::CalendarRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};

pub fn plan_render(
    ctx: PlanContext,
    config: CalendarRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        CalendarRender,
    ))
}

pub struct CalendarRender;

#[async_trait]
impl SourceRender for CalendarRender {
    const PROVIDER: &'static str = "calendar";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse, render};

        let db_path = ingest::db_path_for(raw_path);
        let parsed = parse::parse(&db_path, ctx.raw_range())
            .with_context(|| format!("calendar parse {}", db_path.display()))?;
        let Some(parsed) = parsed else {
            return Ok("no raw store yet".into());
        };
        let mut on_doc = |md| ctx.emit_doc(md);
        let buckets = render::render_all(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.raw_range(),
            &mut on_doc,
        )
        .context("calendar render_all")?;
        ctx.finish(&buckets, parsed.head.as_deref())?;
        Ok("rendered".into())
    }
}
