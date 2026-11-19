//! The render wave for the markdown source: its planner and the
//! [`RenderProcessor`] it plans.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_markdown_config::MarkdownRenderConfig;
use datalib_etl_render::inputs::Bucket;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};

use crate::render::{self, note};

pub fn plan_render(
    ctx: PlanContext,
    config: MarkdownRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        MarkdownRender,
    ))
}

struct MarkdownRender;

#[async_trait]
impl SourceRender for MarkdownRender {
    const PROVIDER: &'static str = "markdown";

    fn render_version(&self) -> u32 {
        render::RENDER_VERSION
    }

    /// A problem the download keeps on a file names it by its path, which
    /// is the note's.
    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        (table == "files").then(|| note::note_id(source_id, id).uuid)
    }

    fn upstream_of_entity(&self, table: &str, id: &str) -> Option<(&'static str, String)> {
        (table == "files").then(|| (note::ENTITY_KIND_NOTE, id.to_string()))
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        // `None`, not an empty folder: a store that could not be read must
        // stop the pass rather than look like one that lost every note.
        let Some(loaded) = render::load(raw_path, ctx.raw_range())
            .await
            .context("markdown load notes")?
        else {
            return Ok("skipped=store-unreadable".to_string());
        };
        ctx.declare_empty(loaded.gone.iter().map(String::as_str))?;
        let (scan_id, root) = match &loaded.scan {
            Some((id, root)) => (Some(id.as_str()), Some(root.as_path())),
            None => (None, None),
        };
        ctx.progress.set_length(Some(loaded.notes.len() as u64));
        let mut buckets = Vec::with_capacity(loaded.notes.len());
        for n in &loaded.notes {
            ctx.progress.inc(1);
            ctx.emit_doc(note::render_note(ctx.name, n, root))
                .with_context(|| format!("store the document for {}", n.id))?;
            buckets.push(Bucket {
                key: n.id.clone(),
                inputs: render::inputs_of(n, scan_id),
            });
        }
        let rendered = buckets.len();
        ctx.finish(&buckets, loaded.head.as_deref())?;
        Ok(format!("rendered={rendered} gone={}", loaded.gone.len()))
    }
}
