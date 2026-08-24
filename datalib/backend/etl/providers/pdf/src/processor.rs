//! Program-A `DataProcessor`s for the `pdf` source.
//!
//! Both waves are always present: download scans and identifies,
//! render converts whatever the store says is convertible. The source
//! owns its raw store end to end (open, register the interrupt hook,
//! write, commit) via the standard `RawStoreSession`; the orchestrator
//! only drives `run`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

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

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let out_dir = datalib_etl::layout::rendered_md_root(ctx.root, ctx.name);
        // Load first, render second: the document sink borrows `ctx`
        // and is not `Send`, so it must not be alive across an await.
        let targets = render::load_targets(&self.raw_path)
            .await
            .context("pdf load render targets")?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let s = render::render_targets(
            &targets,
            &out_dir,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("pdf render")?;
        Ok(format!(
            "converted={} unchanged={} failed={}",
            s.converted, s.skipped_unchanged, s.failed
        ))
    }
}
