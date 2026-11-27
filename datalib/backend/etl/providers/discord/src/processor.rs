//! The `discord` source's download processor.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_discord_config::{DiscordConfig, DiscordSync};
use datalib_etl_web::http::LatchkeySettings;

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

/// One download processor when `api` is set, none otherwise.
pub fn plan_ingest(ctx: PlanContext, config: DiscordConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.api {
        procs.push(Box::new(DiscordIngest {
            id: format!("discord/{}/download", ctx.name),
            raw_path,
            sync,
            blob_size_limit_bytes: config.common.blob_size_limit_bytes,
            latchkey: config.latchkey_settings,
        }));
    }
    Ok(procs)
}

struct DiscordIngest {
    id: String,
    raw_path: PathBuf,
    sync: DiscordSync,
    blob_size_limit_bytes: Option<u64>,
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for DiscordIngest {
    fn id(&self) -> &str {
        &self.id
    }

    /// Seals as pages land.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let now = datalib_time::parse_strict(ctx.now)?;
        let db = ingest::RawDb::open(&ingest::db_path_for(&self.raw_path)).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        ctx.run_store(pool, Some(cas_pool), |sealer| async {
            let s = ingest::fetch(ingest::FetchOptions {
                latchkey: self.latchkey.clone(),
                sync: self.sync.clone(),
                blob_size_limit_bytes: self.blob_size_limit_bytes,
                progress: ctx.progress.clone(),
                control: ctx.control.clone(),
                sealer: Some(sealer),
                ..ingest::FetchOptions::new(db, now)
            })
            .await?;
            Ok(s.line())
        })
        .await
    }
}
