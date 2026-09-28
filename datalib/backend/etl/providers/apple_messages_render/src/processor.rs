//! The render wave for the apple_messages source: its planner and the
//! [`RenderProcessor`] it plans.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::periodize::Period;
use datalib_etl::processor::PlanContext;
use datalib_etl_apple_messages_config::AppleMessagesRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};

pub fn plan_render(
    ctx: PlanContext,
    config: AppleMessagesRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        AppleMessagesRender,
    ))
}

struct AppleMessagesRender;

#[async_trait]
impl SourceRender for AppleMessagesRender {
    const PROVIDER: &'static str = "apple_messages";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let period = Period::from_config(None).context("default apple_messages period")?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let outcome = crate::render::render(
            raw_path,
            ctx.root,
            ctx.name,
            period,
            ctx.progress,
            ctx.raw_range(),
            &mut on_doc,
        )
        .context("apple_messages render")?;
        ctx.finish(&outcome.buckets, outcome.new_head.as_deref())?;
        Ok(format!(
            "rendered={} skipped={}",
            outcome.rendered, outcome.skipped
        ))
    }
}
