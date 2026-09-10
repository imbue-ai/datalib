//! Program-A `DataProcessor`s for the beeper source. Beeper contributes an
//! **download** processor ([`BeeperIngest`] — reads Beeper Texts' on-disk
//! SQLite stores) when `texts` is present, plus an always-present
//! **render** processor ([`BeeperRender`]). [`plan_ingest`] /
//! [`plan_render`] build the per-wave processors the orchestrator drives.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_beeper_config::{BeeperConfig, BeeperSync};

use crate::ingest;

/// Ingest wave: present iff `texts`.
pub fn plan_ingest(ctx: PlanContext, config: BeeperConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.texts {
        if sync.period.is_some() {
            anyhow::bail!(
                "beeper `texts.period` is a render knob — put `period` in the \
                 render step's params instead"
            );
        }
        procs.push(Box::new(BeeperIngest {
            id: format!("beeper/{name}/download"),
            raw_path,
            sync,
        }));
    }
    Ok(procs)
}

/// Beeper's download processor. Owns its raw doltlite store end to end.
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
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = ingest::fetch(ingest::FetchOptions {
            db,
            sources: self.sync.sources.clone(),
            beeper_data_dir: self.sync.path(),
            media: self.sync.media,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "rooms={} users={} events={} blobs={} blob_errors={} enriched={} orphaned={}",
            s.rooms,
            s.users,
            s.events,
            s.blobs,
            s.blob_errors,
            s.events_enriched,
            s.events_orphaned,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
