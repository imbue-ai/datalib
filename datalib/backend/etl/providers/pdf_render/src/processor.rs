//! The render wave for the pdf source: its planner and the
//! [`RenderProcessor`] it plans.

use crate::render;
use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_pdf_config::PdfRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use std::path::PathBuf;

pub fn plan_render(
    ctx: PlanContext,
    config: PdfRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    Ok(vec![Box::new(PdfRender {
        id: format!("pdf/{name}/render"),
        raw_path: config.common.raw_path().to_path_buf(),
    })])
}

struct PdfRender {
    id: String,
    raw_path: PathBuf,
}

#[async_trait]
impl RenderProcessor for PdfRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        // The fingerprint skip is the driver's: every document is emitted
        // and the store writes only the ones that changed.
        let no_priors: std::collections::HashMap<String, String> = Default::default();
        let out_dir = datalib_etl::layout::render_markdown_root(ctx.root, ctx.name);
        // Load first, render second: the document sink borrows `ctx`
        // and is not `Send`, so it must not be alive across an await.
        // `None`, not an empty corpus: this list is the membership test the
        // deletion below uses, so a store we could not read must stop the
        // pass rather than look like a corpus that lost every document.
        let Some(targets) = render::load_targets(&self.raw_path)
            .await
            .context("pdf load render targets")?
        else {
            return Ok("skipped=store-unreadable".to_string());
        };

        let scan = render::scan_changed(&self.raw_path, ctx.raw_cursor)
            .await
            .context("pdf dolt_diff scan")?;

        // `load_targets` returns the whole corpus, so it doubles as the
        // membership test the deletion needs: a bucket the diff named that
        // no target carries is a document the corpus no longer reaches —
        // its last file was deleted. That is a cleaner question than "is
        // there still a row", because a `pdf_documents` row outlives the
        // paths pointing at it.
        let mut dropped = 0usize;
        if let Some(changed) = &scan.changed {
            let present: std::collections::HashSet<&str> =
                targets.iter().map(|t| t.blake3.as_str()).collect();
            for gone in changed.iter().filter(|b| !present.contains(b.as_str())) {
                dropped +=
                    ctx.remove_conversation(&crate::render::grid_rows::document_uuid(gone))?;
            }
        }
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
        let s = render::render_targets(
            &to_render,
            &out_dir,
            ctx.name,
            ctx.progress,
            &no_priors,
            &mut on_doc,
        )
        .context("pdf render")?;

        if let Some(head) = scan.new_head.as_deref() {
            ctx.consumed(head);
        }
        Ok(format!(
            "converted={} unchanged={} skipped={} dropped={} failed={}",
            s.converted, s.skipped_unchanged, skipped, dropped, s.failed
        ))
    }
}
