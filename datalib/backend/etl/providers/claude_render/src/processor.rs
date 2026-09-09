//! The render wave for the claude source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_claude_config::ClaudeRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: ClaudeRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(ClaudeRender {
        id: format!("claude/{name}/render"),
        raw_path,
        name,
        max_project_doc_bytes: config.max_project_doc_bytes,
    })])
}

struct ClaudeRender {
    id: String,
    raw_path: PathBuf,
    name: String,
    /// See [`ClaudeRenderConfig::max_project_doc_bytes`].
    max_project_doc_bytes: Option<usize>,
}

#[async_trait]
impl RenderProcessor for ClaudeRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, &self.name);
        let cursor = datalib_etl::render_cursor::read_for_params(
            &cursor_path,
            &datalib_etl::render_cursor::no_params(),
        )
        .with_context(|| format!("read claude render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("claude parse {}", self.raw_path.display()))?;
        // Conversations and projects claude.ai no longer has. Their pages go
        // before we render, so a run interrupted afterwards has already
        // dropped them rather than leaving a document whose source is gone.
        // A bucket id could have been either kind and the store no longer
        // says which, so both derivations are offered; the one that names
        // nothing removes nothing.
        let mut dropped = 0usize;
        for bucket in &parsed.vanished_buckets {
            dropped += ctx.remove_conversation(&crate::render::ids::conversation(bucket).uuid)?;
            dropped += ctx.remove_conversation(&crate::render::ids::project(bucket).uuid)?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(
            &parsed,
            ctx.root,
            &self.name,
            crate::render::render::RenderOptions {
                max_project_doc_bytes: self.max_project_doc_bytes,
            },
            ctx.progress,
            &mut on_doc,
        )
        .context("claude render_all")?;
        Ok(if dropped == 0 {
            "rendered".into()
        } else {
            format!("rendered, {dropped} document(s) gone upstream")
        })
    }
}
