//! The render wave for the pdf source: its planner and the
//! [`RenderProcessor`] it plans.

use crate::render;
use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_pdf_config::PdfRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

pub fn plan_render(
    ctx: PlanContext,
    config: PdfRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(ctx, config.common.raw_path(), PdfRender))
}

struct PdfRender;

#[async_trait]
impl SourceRender for PdfRender {
    const PROVIDER: &'static str = "pdf";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let out_dir = datalib_etl::layout::render_markdown_root(ctx.root, ctx.name);
        // Load first, render second: the document sink borrows `ctx`
        // and is not `Send`, so it must not be alive across an await.
        // `None`, not an empty corpus: a store we could not read must stop
        // the pass rather than look like a corpus that lost every document.
        let Some(render::Loaded {
            targets,
            scan_meta_id,
            scan,
        }) = render::load(raw_path, ctx.raw_range())
            .await
            .context("pdf load render targets")?
        else {
            return Ok("skipped=store-unreadable".to_string());
        };

        let (to_render, skipped) = match &scan.render {
            None => (targets, 0usize),
            Some(changed) => {
                let before = targets.len();
                let kept: Vec<_> = targets
                    .into_iter()
                    .filter(|t| changed.contains(&t.blake3))
                    .collect();
                let skipped = before.saturating_sub(kept.len());
                (kept, skipped)
            }
        };
        tracing::info!(
            source = ctx.name,
            scan_elapsed_ms = scan.elapsed.map(|d| d.as_millis() as u64),
            convert = to_render.len(),
            skipped,
            cold_start = scan.render.is_none(),
            "[render] pdf dolt_diff scan"
        );

        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render::render_targets(&to_render, &out_dir, ctx.name, ctx.progress, &mut on_doc)
            .context("pdf render")?;
        for (doc_uuid, error) in &s.failures {
            ctx.report_document_failed(doc_uuid, error, Some(self.render_version()))?;
        }

        // Every document this run looked at is declared with nothing —
        // one the corpus no longer reaches, because its last file was
        // deleted, builds no page and its old one goes — and then the
        // converted ones with what they read. One whose conversion failed
        // is left out of both: its page is stale rather than gone, and a
        // declared bucket keeps only what the run emitted.
        let looked_at: Option<std::collections::HashSet<String>> =
            scan.render.as_ref().map(|set| {
                set.iter()
                    .filter(|b| !s.failed_blake3s.contains(*b))
                    .cloned()
                    .collect()
            });
        let converted: Vec<_> = to_render
            .iter()
            .filter(|t| !s.failed_blake3s.contains(&t.blake3))
            .cloned()
            .collect();
        for bucket in render::buckets_of(looked_at.as_ref(), &converted, scan_meta_id.as_deref()) {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }

        if let Some(head) = scan.new_head.as_deref() {
            ctx.consumed(head);
        }
        Ok(format!(
            "converted={} skipped={} failed={}",
            s.converted, skipped, s.failed
        ))
    }
}
