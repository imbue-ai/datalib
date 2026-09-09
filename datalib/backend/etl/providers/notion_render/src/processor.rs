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
        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, ctx.name);
        let cursor = datalib_etl::render_cursor::read_for_params(
            &cursor_path,
            &datalib_etl::render_cursor::no_params(),
        )
        .with_context(|| format!("read notion render cursor {}", cursor_path.display()))?;
        let parsed = parse_api_dir(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("notion parse {}", self.raw_path.display()))?;
        // Documents whose source is gone. They go before the render, so a
        // run interrupted afterwards has already dropped them rather than
        // leaving a document pointing at a page Notion no longer has.
        //
        // A page and its threads are separate conversations, so a vanished
        // page names both: its own uuid, and each discussion the store
        // still remembers hanging off it. The discussion pass then catches
        // a thread whose last comment went while its page survived.
        let mut dropped = 0usize;
        for page in &parsed.vanished_pages {
            dropped += ctx.remove_conversation(page)?;
            for disc in discussions_of(&parsed, page) {
                dropped += ctx.remove_conversation(&disc)?;
            }
        }
        for disc in &parsed.vanished_discussions {
            dropped += ctx.remove_conversation(disc)?;
        }
        let mut on_doc = |md| ctx.emit_doc(md);
        render_notion(&parsed, ctx.root, ctx.name, ctx.progress, &mut on_doc)
            .context("render_notion")?;
        Ok(if dropped == 0 {
            "rendered".into()
        } else {
            format!("rendered, {dropped} document(s) gone upstream")
        })
    }
}

/// Discussions the store still remembers hanging off `page_id`.
///
/// A vanished page's comment rows outlive it — nothing prunes them —
/// which is what makes its threads findable at all. Without this, a
/// deleted page's threads would be orphaned documents no later run
/// would ever name.
fn discussions_of(parsed: &crate::render::ParsedNotion, page_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in &parsed.comments {
        if c.get("page_id").and_then(|v| v.as_str()) != Some(page_id) {
            continue;
        }
        if let Some(d) = c.get("discussion_id").and_then(|v| v.as_str()) {
            if !d.is_empty() && !out.iter().any(|x| x == d) {
                out.push(d.to_string());
            }
        }
    }
    out
}
