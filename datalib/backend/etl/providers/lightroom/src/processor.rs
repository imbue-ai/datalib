//! Program-A `DataProcessor` for the `lightroom` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_lightroom_config::LightroomConfig;

use crate::ingest::{self, MirrorOptions};

pub fn mirror_options(config: &LightroomConfig) -> Result<MirrorOptions> {
    let source_path = config
        .catalog
        .as_ref()
        .ok_or_else(|| anyhow!("lightroom: missing `catalog.path` (the .lrcat to mirror)"))?
        .path();
    Ok(MirrorOptions {
        source_path,
        snapshot: config.snapshot,
        include_tables: config.include_tables.clone(),
        exclude_tables: config.exclude_tables.clone(),
        exclude_columns: config.effective_excluded_columns(),
        stable_key_columns: config.stable_key_columns.clone(),
        primary_keys: config.primary_keys.clone(),
        gc: config.gc,
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
        options: mirror_options(&config)?,
    })])
}

/// The mirror processor. Owns its doltlite store end to end (open,
/// register the interrupt hook, mirror, commit + close via
/// `session.finish`).
struct LightroomIngest {
    id: String,
    raw_path: PathBuf,
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
