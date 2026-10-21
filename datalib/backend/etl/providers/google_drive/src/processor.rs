//! The ingest wave for the `google_drive` source.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_google_drive_config::{GoogleDriveApi, GoogleDriveConfig};
use datalib_etl_web::http::LatchkeySettings;

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

pub fn plan_ingest(
    ctx: PlanContext,
    config: GoogleDriveConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(api) = config.api {
        procs.push(Box::new(GoogleDriveIngest {
            id: format!("google_drive/{name}/download"),
            raw_path,
            latchkey: config.latchkey_settings,
            api,
        }));
    }
    Ok(procs)
}

struct GoogleDriveIngest {
    id: String,
    raw_path: PathBuf,
    latchkey: LatchkeySettings,
    api: GoogleDriveApi,
}

#[async_trait]
impl DataProcessor for GoogleDriveIngest {
    fn id(&self) -> &str {
        &self.id
    }

    /// Every write is a whole capture in one transaction and nothing is
    /// ever deleted, so between checkpoints the store is the previous
    /// one plus the captures this run finished.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let db = ingest::RawDb::open(&ingest::db_path_for(&self.raw_path)).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        ctx.run_store(pool, Some(cas_pool), |sealer| async {
            let s = ingest::fetch(ingest::FetchOptions {
                db,
                latchkey: self.latchkey.clone(),
                api: self.api.clone(),
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
