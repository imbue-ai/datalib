//! Program-A `DataProcessor` for the `beeper` source: present when `texts`
//! is configured. The render processor lives in `datalib_etl_beeper_render`.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::run_problems;
use datalib_etl_beeper_config::{BeeperConfig, BeeperSync};

use crate::ingest::{self, RawDb};

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

pub fn plan_ingest(ctx: PlanContext, config: BeeperConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    Ok(config
        .texts
        .map(|sync| -> Box<dyn DataProcessor> {
            Box::new(BeeperIngest {
                id: format!("beeper/{}/download", ctx.name),
                raw_path: config.common.raw_path().to_path_buf(),
                sync,
            })
        })
        .into_iter()
        .collect())
}

/// Owns its raw doltlite store end to end: open, register the interrupt
/// hook, mirror, commit + close via `run_store`.
struct BeeperIngest {
    id: String,
    raw_path: PathBuf,
    sync: BeeperSync,
}

#[async_trait]
impl DataProcessor for BeeperIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let beeper_dir = self
            .sync
            .path()
            .unwrap_or_else(ingest::default_beeper_data_dir);
        // Before the store: a store that exists reads as published to the
        // render and the migrate pass.
        let index_db = beeper_dir.join("index.db");
        if !index_db.is_file() {
            anyhow::bail!(
                "no index.db at {}: is Beeper Texts installed, and has it run once? \
                 `texts.path` names another data directory",
                index_db.display()
            );
        }
        let db = RawDb::open(&datalib_etl::doltlite_raw::db_path_for(&self.raw_path)).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        let db = &db;
        ctx.run_store(pool.clone(), Some(cas_pool), |_| async move {
            run_problems::collecting(&pool, &ctx.control.stop, |found| async move {
                let summary =
                    ingest::fetch(&beeper_dir, db, self.sync.media, ctx.progress, &found).await?;
                Ok(summary.summary())
            })
            .await
        })
        .await
    }
}
