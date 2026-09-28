//! The render wave for the claude_code source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_claude_code_config::ClaudeCodeRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

pub fn plan_render(
    ctx: PlanContext,
    config: ClaudeCodeRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        ClaudeCodeRender {
            max_tool_result_bytes: config.max_tool_result_bytes,
        },
    ))
}

struct ClaudeCodeRender {
    max_tool_result_bytes: usize,
}

#[async_trait]
impl SourceRender for ClaudeCodeRender {
    const PROVIDER: &'static str = "claude_code";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params_with(serde_json::json!({
            "max_tool_result_bytes": self.max_tool_result_bytes,
        }))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let mut on_doc = |md| ctx.emit_doc(md);
        let outcome = crate::render::render(
            raw_path,
            ctx.root,
            ctx.name,
            ctx.progress,
            &mut on_doc,
            ctx.raw_range(),
            self.max_tool_result_bytes,
        )
        .context("claude_code render")?;

        ctx.finish(&outcome.buckets, outcome.new_head.as_deref())?;
        Ok(format!(
            "rendered={} skipped={}",
            outcome.rendered, outcome.skipped
        ))
    }
}
