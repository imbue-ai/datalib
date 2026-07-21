//! Program-A `DataProcessor` for the yolink source. Yolink is EXTRACT-ONLY:
//! it pulls per-device CSVs into a doltlite raw store and is queried directly
//! downstream, so there is NO render path — `plan_render` returns nothing
//! (download-only is structural, not a flag). The source owns its raw store
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_yolink_config::YolinkRenderConfig;
use frankweiler_etl_yolink_config::{YolinkConfig, YolinkSync};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: YolinkConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(YolinkDownload {
            id: format!("yolink/{name}/download"),
            raw_path,
            sync,
        }));
    }
    Ok(procs)
}

/// Render wave: yolink is download-only today (device history lands in
/// the raw store; no markdown render yet), so this is always empty.
pub fn plan_render(
    ctx: PlanContext,
    config: YolinkRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let _ = (ctx, config);
    Ok(Vec::new())
}

struct YolinkDownload {
    id: String,
    raw_path: PathBuf,
    sync: YolinkSync,
}

#[async_trait]
impl DataProcessor for YolinkDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            sync: self.sync.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "devices={} windows={} readings={} errors={} requests={}",
            s.devices, s.windows, s.readings, s.errors, s.requests,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
