//! The render wave for the `calendar` source: its planner and the
//! [`RenderProcessor`] it plans.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_calendar::ingest;
use datalib_etl_calendar_config::CalendarRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};

pub fn plan_render(
    ctx: PlanContext,
    config: CalendarRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(vec![Box::new(CalendarRender {
        id: format!("calendar/{}/render", ctx.name),
        raw_path: config.common.raw_path().to_path_buf(),
        name: ctx.name,
    })])
}

pub struct CalendarRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for CalendarRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse, render};

        let db_path = ingest::db_path_for(&self.raw_path);
        let parsed = parse::parse(&db_path, ctx.raw_range())
            .with_context(|| format!("calendar parse {}", db_path.display()))?;
        let Some(parsed) = parsed else {
            return Ok("no raw store yet".into());
        };
        let mut on_doc = |md| ctx.emit_doc(md);
        let buckets = render::render_all(
            &parsed,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.raw_range(),
            &mut on_doc,
        )
        .context("calendar render_all")?;
        for bucket in &buckets {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = parsed.head.as_deref() {
            ctx.consumed(head);
        }
        Ok("rendered".into())
    }
}
