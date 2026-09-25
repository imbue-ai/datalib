//! The render wave for the signal source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::periodize::Period;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use datalib_etl_signal_config::SignalRenderConfig;
use std::path::Path;

pub fn plan_render(
    ctx: PlanContext,
    config: SignalRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let period = Period::from_config(config.period.as_deref()).context("signal period")?;
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        SignalRender { period },
    ))
}

/// Signal's render processor — reads the raw store (driven by the render
/// cursor's commit) and emits one rendered markdown per period-bucket through
/// the fused-Load callback.
struct SignalRender {
    period: Period,
}

#[async_trait]
impl SourceRender for SignalRender {
    const PROVIDER: &'static str = "signal";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    // `period` decides how messages bucket into documents, so a change
    // re-renders every document.
    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params_with(crate::render::render_params(
            self.period,
        ))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse, render_all};

        let parsed = parse(raw_path, self.period, ctx.name, ctx.raw_range())
            .with_context(|| format!("signal parse {}", raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let summary = render_all(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("signal render_all")?;
        // A chat this run looked at that came back with no items builds
        // no chat at all, so chat-common never sees it: declared with
        // nothing, its documents go. The rendered ones follow and
        // replace that.
        for chat_id in parsed.scan.render.iter().flatten() {
            ctx.declare_bucket(&crate::render::ids::chat(ctx.name, chat_id).uuid, &[])?;
        }
        ctx.declare_empty(parsed.scan.gone.iter().map(String::as_str))?;
        ctx.finish(&summary.buckets, parsed.scan.new_head.as_deref())?;
        Ok("rendered".into())
    }
}
