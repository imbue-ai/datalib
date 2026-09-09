//! Program-A `DataProcessor`s for the `signal_backup` source. A managed
//! signal source (`sync:` present) contributes download + render; the
//! render processor is always present (renders whatever is in the raw
//! store). The source owns its raw store (open/commit/checkpoint); the
//! orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_signal_config::{SignalConfig, SignalSync};

use crate::download;

pub fn plan_download(
    ctx: PlanContext,
    config: SignalConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let sync = config
        .sync
        .ok_or_else(|| anyhow!("signal_backup source {name} missing sync.snapshot_dir"))?;
    if sync.period.is_some() {
        anyhow::bail!(
            "signal `sync.period` is a render knob — put `period` in the \
             render step's params instead"
        );
    }
    Ok(vec![Box::new(SignalDownload {
        id: format!("signal/{name}/download"),
        raw_path,
        sync,
    })])
}

/// Signal's download processor. Owns its raw doltlite store end to end: opens
/// it, registers an opaque interrupt-commit hook, decrypts the newest snapshot
/// under `snapshot_dir`, commits, closes.
struct SignalDownload {
    id: String,
    raw_path: PathBuf,
    sync: SignalSync,
}

#[async_trait]
impl DataProcessor for SignalDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
            db_path: self.raw_path.clone(),
            db,
            cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
            snapshot_root: self.sync.snapshot_dir.clone(),
            // Default: `<snapshot_root>/files/XX/<name>` — the layout Signal
            // Android produces. Override via a future SignalSync knob if it
            // matters.
            files_root: None,
            aep_env_var: self.sync.aep_env_var.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "recipients={} chats={} chat_items={} media_files={} snapshot={}",
            s.recipients, s.chats, s.chat_items, s.media_files, s.snapshot,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
