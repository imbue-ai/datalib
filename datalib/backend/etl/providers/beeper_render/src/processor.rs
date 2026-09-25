//! The render wave for the beeper source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::periodize::Period;
use datalib_etl::processor::PlanContext;
use datalib_etl_beeper_config::BeeperRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

pub fn plan_render(
    ctx: PlanContext,
    config: BeeperRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let period = Period::from_config(config.period.as_deref()).context("parse beeper period")?;
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        BeeperRender { period },
    ))
}

/// Beeper's render processor — reads the raw store and emits one rendered
/// markdown per `(room, period)` through the fused-Load callback.
struct BeeperRender {
    period: Period,
}

#[async_trait]
impl SourceRender for BeeperRender {
    const PROVIDER: &'static str = "beeper";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let parsed = parse(raw_path, ctx.name, self.period, ctx.raw_range())
            .with_context(|| format!("beeper parse {}", raw_path.display()))?;
        let raw_db_path = datalib_etl::doltlite_raw::db_path_for(raw_path);
        let mut on_doc = |md| ctx.emit_doc(md);
        let summary = render_all(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            &mut on_doc,
            &raw_db_path,
        )
        .context("beeper render_all")?;
        // A room this run looked at that has no event left builds no
        // chat, so chat-common never sees it: declared with nothing, its
        // documents go. The rendered ones follow and replace that.
        for room in parsed.scan.render.iter().flatten() {
            ctx.declare_bucket(&crate::render::ids::room(ctx.name, room).uuid, &[])?;
        }
        ctx.declare_empty(parsed.scan.gone.iter().map(String::as_str))?;
        ctx.finish(&summary.buckets, parsed.scan.new_head.as_deref())?;
        Ok("rendered".into())
    }
}
