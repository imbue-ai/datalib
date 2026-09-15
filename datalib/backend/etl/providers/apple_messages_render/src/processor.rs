//! The render wave for the apple_messages source: its planner and the
//! [`RenderProcessor`] it plans.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::periodize::Period;
use datalib_etl::processor::PlanContext;
use datalib_etl_apple_messages_config::AppleMessagesRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};

pub fn plan_render(
    ctx: PlanContext,
    config: AppleMessagesRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(vec![Box::new(AppleMessagesRender {
        id: format!("apple_messages/{}/render", ctx.name),
        raw_path: config.common.raw_path().to_path_buf(),
        name: ctx.name,
    })])
}

struct AppleMessagesRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for AppleMessagesRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        let period = Period::from_config(None).context("default apple_messages period")?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let outcome = crate::render::render(
            &self.raw_path,
            ctx.root,
            &self.name,
            period,
            ctx.progress,
            ctx.raw_range(),
            &mut on_doc,
        )
        .context("apple_messages render")?;
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
