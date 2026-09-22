//! The render wave for the gitlab source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_gitlab_config::GitlabRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: GitlabRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(GitlabRender {
        id: format!("gitlab/{name}/render"),
        raw_path,
    })])
}

struct GitlabRender {
    id: String,
    raw_path: PathBuf,
}

#[async_trait]
impl RenderProcessor for GitlabRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::grid_rows::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse_api_dir, render_gitlab};
        let parsed = parse_api_dir(&self.raw_path, ctx.name, ctx.raw_range())
            .with_context(|| format!("gitlab parse {}", self.raw_path.display()))?;
        // Every MR this run renders is declared first with nothing, so
        // one whose row is gone loses its document; the render below
        // re-declares the ones it produced.
        for key in parsed.render.iter().flatten() {
            ctx.declare_bucket(key, &[])?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_gitlab(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("render_gitlab")?;
        for bucket in &s.buckets {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = parsed.head.as_deref() {
            ctx.consumed(head);
        }
        Ok(format!("rendered={}", s.rendered))
    }
}
