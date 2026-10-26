//! The ingest wave for the `zoom` source.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_zoom_config::{ZoomApi, ZoomConfig};

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

pub fn plan_ingest(ctx: PlanContext, config: ZoomConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(api) = config.api {
        procs.push(Box::new(ZoomIngest {
            id: format!("zoom/{name}/download"),
            raw_path,
            latchkey: config.latchkey_settings,
            blob_size_limit_bytes: config.common.blob_size_limit_bytes,
            api,
        }));
    }
    Ok(procs)
}

struct ZoomIngest {
    id: String,
    raw_path: PathBuf,
    latchkey: LatchkeySettings,
    blob_size_limit_bytes: Option<u64>,
    api: ZoomApi,
}

#[async_trait]
impl DataProcessor for ZoomIngest {
    fn id(&self) -> &str {
        &self.id
    }

    /// Every write is a window of listing entries with its coverage, or
    /// a flush of whole records, each in one transaction, and nothing is
    /// deleted: between checkpoints the store is the previous one plus
    /// what this run finished.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let today = datalib_time::parse_strict(ctx.now)
            .with_context(|| format!("zoom: run stamp {:?}", ctx.now))?
            .inner()
            .naive_utc()
            .date();
        let db = ingest::RawDb::open(&ingest::db_path_for(&self.raw_path)).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        ctx.run_store(pool, Some(cas_pool), |sealer| async {
            let s = ingest::fetch(ingest::FetchOptions {
                db,
                latchkey: self.latchkey.clone(),
                api: self.api.clone(),
                blob_size_limit_bytes: self.blob_size_limit_bytes,
                today,
                progress: ctx.progress.clone(),
                control: ctx.control.clone(),
                sealer: Some(sealer),
            })
            .await?;
            Ok(s.line())
        })
        .await
    }
}
