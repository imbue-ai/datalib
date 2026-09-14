//! The render wave for the slack source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
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

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let parsed = parse(&self.raw_path, ctx.raw_cursor)
            .with_context(|| format!("slack parse {}", self.raw_path.display()))?;
        // Threads no message belongs to any more — a deleted thread, or
        // one whose every message was deleted. The bucket key is already
        // the uuid render keys the thread's documents by.
        let mut dropped = 0usize;
        for thread_uuid in &parsed.vanished_buckets {
            dropped += ctx.remove_conversation(thread_uuid)?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        let summary = render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
            .context("slack render_all")?;
        // A thread the diff named that has no message left builds no
        // chat, so chat-common never sees it; the named set goes first.
        for thread_uuid in parsed.scan.changed_threads.iter().flatten() {
            ctx.declare_bucket(thread_uuid, &[])?;
        }
        for bucket in &summary.buckets {
            ctx.declare_bucket(bucket, &[])?;
        }
        if let Some(head) = parsed.scan.new_head.as_deref() {
            ctx.consumed(head);
        }
        Ok(if dropped == 0 {
            "rendered".into()
        } else {
            format!("rendered, {dropped} document(s) gone upstream")
        })
    }
}
