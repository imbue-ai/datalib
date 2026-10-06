//! Program-A `DataProcessor` for the `lightroom` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_lightroom_config::LightroomConfig;

use datalib_etl::fingerprint_cache::{self, FingerprintCache};

use crate::ingest::{self, sync, MirrorOptions};

/// The engine's options for this config. `source_path` is the catalog,
/// else the backups folder, which the engine never reads itself: each
/// backup in it is mirrored as its own source.
pub fn mirror_options(config: &LightroomConfig) -> Result<MirrorOptions> {
    let source_path = config
        .catalog
        .as_ref()
        .or(config.backups.as_ref())
        .ok_or_else(|| anyhow!("lightroom: set `catalog.path`, `backups.path`, or both"))?
        .path();
    Ok(MirrorOptions {
        snapshot: config.snapshot,
        include_tables: config.include_tables.clone(),
        exclude_tables: config.exclude_tables.clone(),
        exclude_columns: config.effective_excluded_columns(),
        stable_key_columns: config.stable_key_columns.clone(),
        primary_keys: config.primary_keys.clone(),
        gc: config.gc,
        ..MirrorOptions::new(source_path)
    })
}

pub fn plan_ingest(
    ctx: PlanContext,
    config: LightroomConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    Ok(vec![Box::new(LightroomIngest {
        id: format!("lightroom/{name}/download"),
        raw_path: config.common.raw_path().to_path_buf(),
        catalog: config.catalog.as_ref().map(|p| p.path()),
        backups: config.backups.as_ref().map(|p| p.path()),
        options: mirror_options(&config)?,
    })])
}

/// The mirror processor. Owns its doltlite store end to end (open,
/// register the interrupt hook, mirror, commit + close via
/// `run_store`).
struct LightroomIngest {
    id: String,
    raw_path: PathBuf,
    catalog: Option<PathBuf>,
    backups: Option<PathBuf>,
    options: MirrorOptions,
}

#[async_trait]
impl DataProcessor for LightroomIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let pool = ingest::mirror::open_mirror(&entity_db).await?;
        ctx.run_store(pool.clone(), None, |_| async {
            let cache = FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?;
            let run = sync::run(
                &pool,
                &cache,
                sync::Inputs {
                    backups: self.backups.as_deref(),
                    catalog: self.catalog.as_deref(),
                },
                &self.options,
                ctx.progress,
                &ctx.control.stop,
                ctx.name,
            )
            .await?;
            Ok(run.summary())
        })
        .await
    }
}
