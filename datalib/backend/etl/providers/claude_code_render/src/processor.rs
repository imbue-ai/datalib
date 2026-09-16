//! The render wave for the claude_code source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_claude_code_config::ClaudeCodeRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

pub fn plan_render(
    ctx: PlanContext,
    config: ClaudeCodeRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(ClaudeCodeRender {
        id: format!("claude_code/{name}/render"),
        raw_path,
        name,
        max_tool_result_bytes: config.max_tool_result_bytes,
    })])
}

struct ClaudeCodeRender {
    id: String,
    raw_path: PathBuf,
    name: String,
    max_tool_result_bytes: usize,
}

#[async_trait]
impl RenderProcessor for ClaudeCodeRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params_with(serde_json::json!({
            "max_tool_result_bytes": self.max_tool_result_bytes,
        }))
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        let mut on_doc = |md| ctx.emit_doc(md);
        let outcome = crate::render::render(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            &mut on_doc,
            ctx.raw_range(),
            self.max_tool_result_bytes,
        )
        .context("claude_code render")?;

        for bucket in &outcome.buckets {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = outcome.new_head.as_deref() {
            ctx.consumed(head);
        }
        Ok(format!(
            "rendered={} skipped={}",
            outcome.rendered, outcome.skipped
        ))
    }
}
