//! The render wave for the chatgpt source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_chatgpt_config::ChatgptRenderConfig;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: ChatgptRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        ChatgptRender,
    ))
}

struct ChatgptRender;

#[async_trait]
impl SourceRender for ChatgptRender {
    const PROVIDER: &'static str = "chatgpt";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let parsed = parse(raw_path, ctx.name, ctx.raw_range())
            .with_context(|| format!("chatgpt parse {}", raw_path.display()))?;
        ctx.report_unparsed(
            &ReadScope::Whole(vec!["conversations"]),
            &parsed.unparsed,
            Some(self.render_version()),
        )?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let buckets = render_all(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("chatgpt render_all")?;
        // A conversation this run looked at that builds no page is
        // declared with nothing, so its page goes; the rendered ones
        // follow and replace that.
        for conv_id in parsed.scan.render.iter().flatten() {
            ctx.declare_bucket(
                &crate::render::ids::conversation(ctx.name, conv_id).uuid,
                &[],
            )?;
        }
        ctx.declare_empty(parsed.scan.gone.iter().map(String::as_str))?;
        ctx.finish(&buckets, parsed.scan.new_head.as_deref())?;
        Ok("rendered".into())
    }
}
