//! The download `DataProcessor`s for the `markdown` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_files::fingerprint_cache::{self, FingerprintCache};
use datalib_etl_markdown_config::MarkdownConfig;

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

pub fn plan_ingest(
    ctx: PlanContext,
    config: MarkdownConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    config.validate()?;
    let name = ctx.name;
    let root = config
        .fswalk
        .as_ref()
        .ok_or_else(|| anyhow!("markdown source {name} missing `fswalk.path`"))?
        .path();
    Ok(vec![Box::new(MarkdownIngest {
        id: format!("markdown/{name}/download"),
        raw_path: config.common.raw_path().to_path_buf(),
        root,
        ignore: config.scan_ignores(),
        max_bytes: config.max_bytes,
    })])
}

struct MarkdownIngest {
    id: String,
    raw_path: PathBuf,
    root: PathBuf,
    ignore: Vec<String>,
    max_bytes: Option<u64>,
}

#[async_trait]
impl DataProcessor for MarkdownIngest {
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
                max_bytes: self.max_bytes,
                cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
                now: ctx.now.to_string(),
                progress: ctx.progress.clone(),
            })
            .await?;
            Ok(format!(
                "files={} added={} changed={} unchanged={} removed={} too_large={} errors={}",
                s.files_seen, s.added, s.changed, s.unchanged, s.removed, s.too_large, s.errors,
            ))
        })
        .await
    }
}
