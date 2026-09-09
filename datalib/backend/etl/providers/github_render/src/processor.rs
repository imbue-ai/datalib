//! The render wave for the github source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_github_config::GithubRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: GithubRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(GithubRender {
        id: format!("github/{name}/render"),
        raw_path,
    })])
}

struct GithubRender {
    id: String,
    raw_path: PathBuf,
}

#[async_trait]
impl RenderProcessor for GithubRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::grid_rows::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse_api_dir, render_github};
        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, ctx.name);
        let cursor = datalib_etl::render_cursor::read_for_params(
            &cursor_path,
            &datalib_etl::render_cursor::no_params(),
        )
        .with_context(|| format!("read github render cursor {}", cursor_path.display()))?;
        let parsed = parse_api_dir(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("github parse {}", self.raw_path.display()))?;

        // Deletions are named, not swept. This renderer is narrowed by the
        // diff above, so the documents it emitted are only the ones that
        // *changed* — handing that set to `retain_documents` would delete
        // every PR that merely held still. That swap is the load-bearing
        // half of putting a renderer on a cursor, and getting it wrong
        // empties the source on the first quiet run.
        let mut dropped = 0usize;
        for bucket in &parsed.vanished_buckets {
            let Some((repo, num)) = bucket.rsplit_once('#') else {
                continue;
            };
            let Ok(num) = num.parse::<u32>() else {
                continue;
            };
            dropped += ctx.remove_conversation(&crate::render::parse::github_pr_uuid(repo, num))?;
        }

        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render_github(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("render_github")?;
        Ok(format!(
            "rendered={} skipped={} dropped={}",
            s.rendered, parsed.docs_skipped, dropped
        ))
    }
}
