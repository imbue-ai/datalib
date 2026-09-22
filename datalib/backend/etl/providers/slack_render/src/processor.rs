//! The render wave for the slack source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{ReadScope, RenderCtx, RenderProcessor};
use datalib_etl_slack_config::SlackRenderConfig;
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: SlackRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(SlackRender {
        id: format!("slack/{name}/render"),
        raw_path,
        name,
    })])
}

struct SlackRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for SlackRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        use datalib_etl_slack::ingest::schema_raw::split_thread_key as slack_thread_key_parts;
        let parsed = parse(&self.raw_path, ctx.raw_range())
            .with_context(|| format!("slack parse {}", self.raw_path.display()))?;
        // Users are read whole every run; messages only for the changed
        // threads on a narrowed run.
        let scope = match parsed.scan.render {
            None => ReadScope::Whole(vec!["users", "messages"]),
            Some(_) => ReadScope::Whole(vec!["users"]),
        };
        ctx.report_unparsed(&scope, &parsed.unparsed, self.render_version())?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let summary = render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
            .context("slack render_all")?;
        // A thread this run looked at that has no message left builds no
        // chat, so chat-common never sees it: declared with nothing, its
        // documents go. The rendered ones follow and replace that.
        for key in parsed.scan.render.iter().flatten() {
            let Some((team, channel, ts)) = slack_thread_key_parts(key) else {
                continue;
            };
            ctx.declare_bucket(
                &crate::render::ids::thread(&self.name, team, channel, ts).uuid,
                &[],
            )?;
        }
        for bucket in &summary.buckets {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = parsed.scan.new_head.as_deref() {
            ctx.consumed(head);
        }
        Ok("rendered".into())
    }
}
