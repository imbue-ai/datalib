//! The `DataProcessor` for the `gpx` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_gpx_config::GpxConfig;

use crate::ingest;

pub fn plan_ingest(ctx: PlanContext, config: GpxConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    config.validate()?;
    let name = ctx.name;
    let root = config
        .fswalk
        .as_ref()
        .ok_or_else(|| anyhow!("gpx source {name} missing `fswalk.path`"))?
        .path();
    Ok(vec![Box::new(GpxIngest {
        id: format!("gpx/{name}/download"),
        raw_path: config.common.raw_path().to_path_buf(),
        root,
        ignore: config.ignore,
    })])
}

struct GpxIngest {
    id: String,
    raw_path: PathBuf,
    root: PathBuf,
    ignore: Vec<String>,
}

#[async_trait]
impl DataProcessor for GpxIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = ingest::fetch(ingest::FetchOptions {
            db,
            root: self.root.clone(),
            ignore: self.ignore.clone(),
            cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
            progress: ctx.progress.clone(),
        })
        .await?;
        // How well the store holds each file is otherwise invisible; the
        // three counts say at a glance whether every file comes back.
        let summary = format!(
            "files={} unchanged={} read={} renamed={} removed={} exact={} equivalent={} \
             lossy={} points_added={} points_removed={} errors={}",
            s.files,
            s.unchanged,
            s.read,
            s.renamed,
            s.removed,
            s.exact,
            s.equivalent,
            s.lossy,
            s.points_added,
            s.points_removed,
            s.errors,
        );
        session.finish(ctx, summary).await
    }
}
