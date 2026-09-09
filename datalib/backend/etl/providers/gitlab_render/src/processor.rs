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
        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, ctx.name);
        let cursor = datalib_etl::render_cursor::read_for_params(
            &cursor_path,
            &datalib_etl::render_cursor::no_params(),
        )
        .with_context(|| format!("read gitlab render cursor {}", cursor_path.display()))?;
        let parsed = parse_api_dir(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("gitlab parse {}", self.raw_path.display()))?;

        // Named, not swept: this render is narrowed by the diff, so what it
        // emits is only what changed. Handing that to `retain_documents`
        // would delete every MR that merely held still.
        let mut dropped = 0usize;
        for bucket in &parsed.vanished_buckets {
            let Some((proj, iid)) = bucket.rsplit_once('!') else {
                continue;
            };
            let Ok(iid) = iid.parse::<u32>() else {
                continue;
            };
            dropped += ctx.remove_conversation(&crate::render::parse::gitlab_mr_uuid(proj, iid))?;
        }

        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_gitlab(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("render_gitlab")?;
        Ok(format!(
            "rendered={} skipped={} dropped={}",
            s.rendered, parsed.docs_skipped, dropped
        ))
    }
}
