//! Program-A `DataProcessor`s for the `pdf` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_files::fingerprint_cache::{self, FingerprintCache};
use datalib_etl_pdf_config::PdfConfig;

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

pub fn plan_ingest(ctx: PlanContext, config: PdfConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    config.validate()?;
    let name = ctx.name;
    let root = config
        .fswalk
        .as_ref()
        .ok_or_else(|| anyhow!("pdf source {name} missing `fswalk.path`"))?
        .path();
    Ok(vec![Box::new(PdfIngest {
        id: format!("pdf/{name}/download"),
        raw_path: config.common.raw_path().to_path_buf(),
        root,
        ignore: config.ignore,
        max_bytes: config.max_bytes,
    })])
}

struct PdfIngest {
    id: String,
    raw_path: PathBuf,
    root: PathBuf,
    ignore: Vec<String>,
    max_bytes: Option<u64>,
}

#[async_trait]
impl DataProcessor for PdfIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        ctx.run_store(db.pool().clone(), None, |_| async {
            let s = ingest::fetch(ingest::FetchOptions {
                db,
                source_id: ctx.name.to_string(),
                root: self.root.clone(),
                ignore: self.ignore.clone(),
                cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
                max_bytes: self.max_bytes,
                now: ctx.now.to_string(),
                progress: ctx.progress.clone(),
            })
            .await?;
            Ok(format!(
                "pdfs={} docs={} hashed={} reused={} needs_ocr={} too_large={} docs_removed={} errors={}",
                s.pdfs_seen,
                s.documents,
                s.hashed,
                s.reused,
                s.needs_ocr,
                s.too_large,
                s.documents_removed,
                s.errors,
            ))
        })
        .await
    }
}
