//! The render wave for the google_takeout source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_google_takeout_config::GoogleTakeoutRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::collections::HashMap;
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

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        // Only the chat-shaped feeds (Google Chat / Google Voice) render; the
        // other feeds stay queryable in the raw store.
        let prior: &HashMap<String, String> = ctx.prior_fingerprints;
        // This renderer walks the whole raw store every run, so the set it
        // considered is the complete one: anything else the render store
        // holds is a document whose source is gone. The driver sweeps.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut on_doc = |md| ctx.emit_doc(md);
        let pass = crate::render::render(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            prior,
            &mut on_doc,
            &mut seen,
        )?;
        ctx.retain_documents(pass, &seen);
        Ok("rendered".into())
    }
}
