//! The ingest wave for the airvisual source: its planner and the
//! `DataProcessor` it plans.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_airvisual_config::{AirvisualConfig, AirvisualDevice};

use crate::ingest;

pub fn plan_ingest(
    ctx: PlanContext,
    config: AirvisualConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let Some(export) = config.export else {
        anyhow::bail!("airvisual source {name} names no `export` (the Pros' data folders)");
    };
    Ok(vec![Box::new(AirvisualIngest {
        id: format!("airvisual/{name}/download"),
        raw_path,
        devices: export.devices,
    })])
}

struct AirvisualIngest {
    id: String,
    raw_path: PathBuf,
    devices: Vec<AirvisualDevice>,
}

#[async_trait]
impl DataProcessor for AirvisualIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = ingest::fetch(ingest::FetchOptions {
            db,
            devices: self.devices.clone(),
            cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "devices={} files={} files_skipped={} lines={} samples={} sentinels={} clock_unset={} bad_lines={} errors={}",
            s.devices, s.files, s.files_skipped, s.lines, s.samples, s.sentinels, s.clock_unset, s.bad_lines, s.errors,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
