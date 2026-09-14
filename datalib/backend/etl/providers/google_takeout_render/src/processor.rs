//! The render wave for the google_takeout source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_google_takeout_config::GoogleTakeoutRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: GoogleTakeoutRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(GoogleTakeoutRender {
        id: format!("google_takeout/{name}/render"),
        raw_path,
        name,
    })])
}

struct GoogleTakeoutRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for GoogleTakeoutRender {
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
        // Only the chat-shaped feeds (Google Chat / Google Voice) render; the
        // other feeds stay queryable in the raw store.
        let mut on_doc = |md| ctx.emit_doc(md);
        let outcome = crate::render::render(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            &mut on_doc,
            ctx.raw_range(),
        )?;
        for bucket in &outcome.buckets {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = outcome.new_head.as_deref() {
            ctx.consumed(head);
        }
        Ok("rendered".into())
    }
}
