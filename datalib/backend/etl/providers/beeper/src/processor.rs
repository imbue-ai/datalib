//! Program-A `DataProcessor`s for the beeper source. Beeper contributes an
//! **download** processor ([`BeeperDownload`] — reads Beeper Texts' on-disk
//! SQLite stores) when `sync:` is present, plus an always-present
//! **render** processor ([`BeeperRender`]). [`plan_download`] /
//! [`plan_render`] build the per-wave processors the orchestrator drives.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_beeper_config::{BeeperConfig, BeeperSync};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: BeeperConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        if sync.period.is_some() {
            anyhow::bail!(
                "beeper `sync.period` is a render knob — put `period` in the \
                 render step's params instead"
            );
        }
        procs.push(Box::new(BeeperDownload {
            id: format!("beeper/{name}/download"),
            raw_path,
            sync,
        }));
    }
    Ok(procs)
}

/// Beeper's download processor. Owns its raw doltlite store end to end.
struct BeeperDownload {
    id: String,
    raw_path: PathBuf,
    sync: BeeperSync,
}

#[async_trait]
impl DataProcessor for BeeperDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
            db,
            sources: self.sync.sources.clone(),
            beeper_data_dir: self.sync.beeper_data_dir.clone(),
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
