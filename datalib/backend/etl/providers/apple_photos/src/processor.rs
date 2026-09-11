//! Program-A `DataProcessor` for the `apple_photos` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_apple_photos_config::{photos_sqlite_path, ApplePhotosConfig};

use crate::ingest::{self, MirrorOptions};

pub fn mirror_options(config: &ApplePhotosConfig) -> Result<MirrorOptions> {
    let library = config
        .library
        .as_ref()
        .ok_or_else(|| {
            anyhow!("apple_photos: missing `library.path` (the .photoslibrary to mirror)")
        })?
        .path();
    Ok(MirrorOptions {
        source_path: photos_sqlite_path(&library),
        snapshot: config.snapshot,
        include_tables: config.include_tables.clone(),
        exclude_tables: config.effective_excluded_tables(),
        exclude_columns: config.effective_excluded_columns(),
        stable_key_columns: config.stable_key_columns.clone(),
        primary_keys: config.primary_keys.clone(),
        gc: config.gc,
        sidecar_tables: Vec::new(),
    })
}

pub fn plan_ingest(
    ctx: PlanContext,
    config: ApplePhotosConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    Ok(vec![Box::new(ApplePhotosIngest {
        id: format!("apple_photos/{name}/download"),
        raw_path: config.common.raw_path().to_path_buf(),
        options: mirror_options(&config)?,
    })])
}

/// The mirror processor. Owns its doltlite store end to end (open,
/// register the interrupt hook, mirror, commit + close via
/// `session.finish`).
struct ApplePhotosIngest {
    id: String,
    raw_path: PathBuf,
    options: MirrorOptions,
}

#[async_trait]
impl DataProcessor for ApplePhotosIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let pool = ingest::mirror::open_mirror(&entity_db).await?;
        let session = ctx.open_store(pool.clone(), entity_db).await;
        let stats = ingest::fetch(ingest::FetchOptions {
            mirror_path: self.raw_path.clone(),
            pool: Some(pool),
            options: self.options.clone(),
            progress: ctx.progress.clone(),
        })
        .await?;
        Ok(session.finish(ctx, stats.summary()).await)
    }
}
