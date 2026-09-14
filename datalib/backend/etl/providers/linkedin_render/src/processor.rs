//! The render wave for the linkedin source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_linkedin_config::LinkedinRenderConfig;
use datalib_etl_render::inputs::{Buckets, Input, RawRange};
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
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
    /// The rows the account label came from, declared by every document.
    pub account_inputs: &'a [Input],
    /// The raw store as this run reads it — the driver's cursor, pin and
    /// stale set.
    pub range: RawRange<'a>,
}

/// What one feed's render pass did: every bucket it looked at, named
/// first with nothing and then, for the rendered ones, with what they
/// read; and the commit it read.
#[derive(Debug, Default)]
pub struct FeedOutcome {
    pub buckets: Buckets,
    pub new_head: Option<String>,
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

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        let mut on_doc = |md| ctx.emit_doc(md);
        let account = crate::account::load_account(&self.raw_path, ctx.raw_range())
            .context("linkedin account")?;
        let source = Source {
            raw_dir: &self.raw_path,
            out_dir: ctx.root,
            name: &self.name,
            account: account.label.as_deref(),
            account_inputs: &account.inputs,
            range: ctx.raw_range(),
        };

        // Every message-shaped feed (DMs + AI-coach transcripts) renders.
        let messages =
            crate::render::render(&source, ctx.progress, &mut on_doc).context("linkedin render")?;
        // Connections render as first-class contacts via the shared contact
        // renderer (sibling of the chat path above).
        let connections =
            crate::connections::render_connections(&source, ctx.progress, &mut on_doc)
                .context("linkedin connections render")?;
        // Your own posts (Shares) and the comments you left, grouped one
        // chat-style thread per post, with linkouts back to linkedin.com.
        let posts = crate::posts::render_posts(&source, ctx.progress, &mut on_doc)
            .context("linkedin posts render")?;

        // Each feed names the buckets it looked at, with nothing, and then
        // the ones it rendered, with what they read — in that order, so a
        // bucket that no feed rendered ends declared with nothing and its
        // documents go. The three feeds' keys never collide.
        for bucket in messages
            .buckets
            .iter()
            .chain(&connections.buckets)
            .chain(&posts.buckets)
        {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = [&messages, &connections, &posts]
            .iter()
            .find_map(|o| o.new_head.as_deref())
        {
            ctx.consumed(head);
        }
        Ok("rendered".into())
    }
}
