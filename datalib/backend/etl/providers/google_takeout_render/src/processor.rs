//! The render wave for the google_takeout source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_google_takeout_config::GoogleTakeoutRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: GoogleTakeoutRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        GoogleTakeoutRender,
    ))
}

struct GoogleTakeoutRender;

#[async_trait]
impl SourceRender for GoogleTakeoutRender {
    const PROVIDER: &'static str = "google_takeout";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        // Only the chat-shaped feeds (Google Chat / Google Voice) render; the
        // other feeds stay queryable in the raw store.
        let mut on_doc = |md| ctx.emit_doc(md);
        let outcome = crate::render::render(
            raw_path,
            ctx.root,
            ctx.name,
            ctx.progress,
            &mut on_doc,
            ctx.raw_range(),
        )?;
        ctx.finish(&outcome.buckets, outcome.new_head.as_deref())?;
        Ok("rendered".into())
    }
}
