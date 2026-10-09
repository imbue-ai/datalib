//! Program-A `DataProcessor` for the `apple_photos` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl::run_problems;
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
        snapshot: config.snapshot,
        include_tables: config.include_tables.clone(),
        exclude_tables: config.effective_excluded_tables(),
        exclude_columns: config.effective_excluded_columns(),
        stable_key_columns: config.stable_key_columns.clone(),
        primary_keys: config.primary_keys.clone(),
        gc: config.gc,
        ..MirrorOptions::new(photos_sqlite_path(&library))
    })
}

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let pool = ingest::mirror::open_mirror(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    pool.close().await;
    Ok(())
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
/// `run_store`).
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
        let pool = &pool;
        ctx.run_store(pool.clone(), None, |_| async move {
            run_problems::collecting(pool, &ctx.control.stop, |found| async move {
                let stats =
                    ingest::mirror::run_or_report(pool, &self.options, ctx.progress, &found)
                        .await?;
                Ok(stats.map_or_else(|| "nothing mirrored".to_string(), |s| s.summary()))
            })
            .await
        })
        .await
    }
}
