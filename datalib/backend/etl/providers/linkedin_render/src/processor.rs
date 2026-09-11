//! The render wave for the linkedin source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_linkedin_config::LinkedinRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderPass, RenderProcessor};
use std::path::{Path, PathBuf};

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: LinkedinRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(LinkedinRender {
        id: format!("linkedin/{name}/render"),
        raw_path,
        name,
    })])
}

/// What every feed's render needs to know about the source it is
/// rendering: where the raw store and the output tree are, what the
/// source is called, and whose export it is.
pub struct Source<'a> {
    pub raw_dir: &'a Path,
    pub out_dir: &'a Path,
    pub name: &'a str,
    pub account: Option<&'a str>,
}

/// LinkedIn's render processor — renders the three feeds (messages,
/// connections, posts) and emits each rendered markdown through the
/// fused-Load callback.
struct LinkedinRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for LinkedinRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        // This renderer walks the whole raw store every run, so the set it
        // considered is the complete one: anything else the render store
        // holds is a document whose source is gone. The driver sweeps.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut on_doc = |md| ctx.emit_doc(md);
        let account = crate::account::load_account(&self.raw_path).context("linkedin account")?;
        let source = Source {
            raw_dir: &self.raw_path,
            out_dir: ctx.root,
            name: &self.name,
            account: account.as_deref(),
        };

        // Every message-shaped feed (DMs + AI-coach transcripts) renders.
        let r_pass = crate::render::render(
            &source,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut seen,
        )
        .context("linkedin render")?;
        // Connections render as first-class contacts via the shared contact
        // renderer (sibling of the chat path above).
        let c_pass = crate::connections::render_connections(
            &source,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut seen,
        )
        .context("linkedin connections render")?;
        // Your own posts (Shares) and the comments you left, grouped one
        // chat-style thread per post, with linkouts back to linkedin.com.
        let p_pass = crate::posts::render_posts(
            &source,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut seen,
        )
        .context("linkedin posts render")?;

        // One sweep over the union of all three feeds: each contributes a
        // slice of this source's documents, and sweeping per feed would
        // have each delete the other two's.
        // The sweep drops anything none of the three named, so it is only
        // safe when all three actually walked. One that bailed contributed
        // no uuids, and sweeping on that deletes what it would have named.
        let pass = if [c_pass, p_pass, r_pass]
            .iter()
            .all(|p| *p == RenderPass::Walked)
        {
            RenderPass::Walked
        } else {
            RenderPass::Skipped
        };
        ctx.retain_documents(pass, &seen);
        Ok("rendered".into())
    }
}
