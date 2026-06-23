//! Program-A `DataProcessor` for the yolink source. Yolink is EXTRACT-ONLY:
//! it pulls per-device CSVs into a doltlite raw store and is queried directly
//! downstream, so there is NO translate path — `plan().translate` stays empty
//! (extract-only is structural, not a flag). The source owns its raw store
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};
use frankweiler_etl::raw_store::PoolCheckpoint;
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

/// Convert this crate's schema-only `YolinkSync` into the `frankweiler_core`
/// type that `extract::fetch`'s `FetchOptions.sync` still expects, field by
/// field. The two structs are intentionally distinct (the config crate has no
/// core dependency); this is the seam where they meet.
fn to_core_sync(s: &YolinkSync) -> frankweiler_core::config::YolinkSync {
    frankweiler_core::config::YolinkSync {
        overlap_minutes: s.overlap_minutes,
        window_days: s.window_days,
        devices: s
            .devices
            .iter()
            .map(|d| frankweiler_core::config::YolinkDevice {
                name: d.name.clone(),
                kind: d.kind.clone(),
                start: d.start.clone(),
                family_device_id: d.family_device_id.clone(),
                device_udid: d.device_udid.clone(),
            })
            .collect(),
    }
}

#[async_trait]
impl DataProcessor for YolinkExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let db = extract::RawDb::open(&extract::db_path_for(&self.raw_path)).await?;
        let pool = db.pool().clone();
        ctx.register_checkpoint(
            &self.id,
            PoolCheckpoint::new(
                pool.clone(),
                format!("extract {}: interrupted (Ctrl-C)", ctx.name),
            ),
        );
        let s = extract::fetch(extract::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            sync: to_core_sync(&self.sync),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "devices={} windows={} readings={} errors={} requests={}",
            s.devices, s.windows, s.readings, s.errors, s.requests,
        );
        Ok(frankweiler_etl::raw_store::commit_and_close(pool, ctx.name, summary).await)
    }
}
