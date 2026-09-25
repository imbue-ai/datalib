//! The render wave for the slack source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};
use datalib_etl_slack_config::SlackRenderConfig;
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: SlackRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        SlackRender,
    ))
}

struct SlackRender;

#[async_trait]
impl SourceRender for SlackRender {
    const PROVIDER: &'static str = "slack";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        use datalib_etl_slack::ingest::schema_raw::split_thread_key as slack_thread_key_parts;
        let parsed = parse(raw_path, ctx.name, ctx.raw_range())
            .with_context(|| format!("slack parse {}", raw_path.display()))?;
        // Users are read whole every run; messages only for the changed
        // threads on a narrowed run.
        let scope = match parsed.scan.render {
            None => ReadScope::Whole(vec!["users", "messages"]),
            Some(_) => ReadScope::Whole(vec!["users"]),
        };
        ctx.report_unparsed(&scope, &parsed.unparsed, Some(self.render_version()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let summary = render_all(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("slack render_all")?;
        // A thread this run looked at that has no message left builds no
        // chat, so chat-common never sees it: declared with nothing, its
        // documents go. The rendered ones follow and replace that.
        for key in parsed.scan.render.iter().flatten() {
            let Some((team, channel, ts)) = slack_thread_key_parts(key) else {
                continue;
            };
            ctx.declare_bucket(
                &crate::render::ids::thread(ctx.name, team, channel, ts).uuid,
                &[],
            )?;
        }
        ctx.finish(&summary.buckets, parsed.scan.new_head.as_deref())?;
        Ok("rendered".into())
    }
}
