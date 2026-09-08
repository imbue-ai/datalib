//! Program-A `DataProcessor`s for the `pdf` source.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_pdf_config::{PdfConfig, PdfRenderConfig};

use crate::{download, render};

pub fn plan_download(ctx: PlanContext, config: PdfConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    config.validate()?;
    let name = ctx.name;
    Ok(vec![Box::new(PdfDownload {
        id: format!("pdf/{name}/download"),
        raw_path: config.common.raw_path().to_path_buf(),
        root: config.common.input_or_raw_path().to_path_buf(),
        ignore: config.ignore,
        max_bytes: config.max_bytes,
    })])
}

pub fn plan_render(
    ctx: PlanContext,
    config: PdfRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    Ok(vec![Box::new(PdfRender {
        id: format!("pdf/{name}/render"),
        raw_path: config.common.raw_path().to_path_buf(),
    })])
}

struct PdfDownload {
    id: String,
    raw_path: PathBuf,
    root: PathBuf,
    ignore: Vec<String>,
    max_bytes: Option<u64>,
}

#[async_trait]
impl DataProcessor for PdfDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
            db,
            source_name: ctx.name.to_string(),
            root: self.root.clone(),
            ignore: self.ignore.clone(),
            cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
            max_bytes: self.max_bytes,
            force_rehash: ctx.control.reset_and_redownload,
            now: ctx.now.to_string(),
            progress: ctx.progress.clone(),
        })
        .await?;
        let summary = format!(
            "pdfs={} docs={} hashed={} reused={} needs_ocr={} too_large={} errors={}",
            s.pdfs_seen, s.documents, s.hashed, s.reused, s.needs_ocr, s.too_large, s.errors,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct PdfRender {
    id: String,
    raw_path: PathBuf,
}

#[async_trait]
impl DataProcessor for PdfRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let out_dir = datalib_etl::layout::rendered_md_root(ctx.root, ctx.name);
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

        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, ctx.name);
        let cursor = datalib_etl::render_cursor::read_for_params(
            &cursor_path,
            &datalib_etl::render_cursor::no_params(),
        )
        .with_context(|| format!("read pdf render cursor {}", cursor_path.display()))?;
        let scan = render::scan_changed(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .await
        .context("pdf dolt_diff scan")?;

        // `load_targets` returns the whole corpus, so it doubles as the
        // membership test the deletion needs: a bucket the diff named that
        // no target carries is a document the corpus no longer reaches —
        // its last file was deleted. That is a cleaner question than "is
        // there still a row", because a `pdf_documents` row outlives the
        // paths pointing at it.
        let mut dropped = 0usize;
        let (to_render, skipped) = match &scan.changed {
            None => (targets, 0usize),
            Some(changed) => {
                let present: std::collections::HashSet<&str> =
                    targets.iter().map(|t| t.blake3.as_str()).collect();
                for gone in changed.iter().filter(|b| !present.contains(b.as_str())) {
                    dropped +=
                        ctx.remove_conversation(&crate::render::grid_rows::document_uuid(gone))?;
                }
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
            cold_start = scan.changed.is_none(),
            "[render] pdf dolt_diff scan"
        );

        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render::render_targets(
            &to_render,
            &out_dir,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("pdf render")?;

        // After the conversions landed, so an interrupted run re-scans the
        // same range rather than believing it consumed it.
        if let Some(head) = scan.new_head.as_deref() {
            datalib_etl::render_cursor::write(
                &cursor_path,
                head,
                scan.elapsed,
                &datalib_etl::render_cursor::no_params(),
            )
            .with_context(|| format!("write pdf render cursor {}", cursor_path.display()))?;
        }
        Ok(format!(
            "converted={} unchanged={} skipped={} dropped={} failed={}",
            s.converted, s.skipped_unchanged, skipped, dropped, s.failed
        ))
    }
}
