//! Program-A `DataProcessor` for the yolink source. Yolink is EXTRACT-ONLY:
//! it pulls per-device CSVs into a doltlite raw store and is queried directly
//! downstream, so there is NO translate path — `plan().translate` stays empty
//! (extract-only is structural, not a flag). The source owns its raw store
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};
use frankweiler_etl_yolink_config::{YolinkConfig, YolinkSync};

use crate::extract;

/// Build the SourcePlan: an extract processor when `sync:` is present;
/// `translate` is always empty (yolink has no render path).
pub fn plan(common: PlanCommon, config: YolinkConfig) -> Result<SourcePlan> {
    let PlanCommon { name, raw_path, .. } = common;
    let mut plan = SourcePlan::new();
    if let Some(sync) = config.sync {
        plan.extract.push(Box::new(YolinkExtract {
            id: format!("yolink/{name}/extract"),
            raw_path,
            sync,
        }));
    }
    Ok(plan)
}

struct YolinkExtract {
    id: String,
    raw_path: PathBuf,
    sync: YolinkSync,
}

#[async_trait]
impl DataProcessor for YolinkExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = extract::db_path_for(&self.raw_path);
        let db = extract::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = extract::fetch(extract::FetchOptions {
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
