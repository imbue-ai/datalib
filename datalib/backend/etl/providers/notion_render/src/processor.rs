//! The render wave for the notion source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_notion_config::NotionRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: NotionRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(NotionRender {
        id: format!("notion/{name}/render"),
        raw_path,
    })])
}

struct NotionRender {
    id: String,
    raw_path: PathBuf,
}

#[async_trait]
impl RenderProcessor for NotionRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse_api_dir, render::render_notion};
        let parsed = parse_api_dir(&self.raw_path, ctx.raw_range())
            .with_context(|| format!("notion parse {}", self.raw_path.display()))?;
        // Every bucket this run renders — page or thread — is declared
        // first with nothing, so one whose rows are gone loses its
        // documents; the render below re-declares the ones it produced.
        for key in parsed.render.iter().flatten() {
            ctx.declare_bucket(key, &[])?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_notion(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("render_notion")?;
        for bucket in &s.buckets {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = parsed.head.as_deref() {
            ctx.consumed(head);
        }
        Ok(format!("rendered {} document(s)", s.rendered))
    }
}
