//! Program-A `DataProcessor`s for the `sms_backup_restore` source.

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_sms_backup_restore_config::SmsBackupRestoreConfig;

use crate::ingest;

pub fn plan_ingest(
    ctx: PlanContext,
    config: SmsBackupRestoreConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let input_path = config
        .backup
        .ok_or_else(|| anyhow!("sms_backup_restore source {name} missing `backup.path`"))?
        .path();
    Ok(vec![Box::new(SmsIngest {
        id: format!("sms_backup_restore/{name}/download"),
        raw_path,
        input_path,
    })])
}

/// sms_backup_restore's download processor. Owns its raw doltlite store end to
/// end (open, register interrupt hook, ingest the export, commit+close).
struct SmsIngest {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
}

#[async_trait]
impl DataProcessor for SmsIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = ingest::fetch(ingest::FetchOptions {
            cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
            db,
            input_path: self.input_path.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "sms={} mms={} calls={} attachments={} blobs={} parse_errors={}",
            s.sms, s.mms, s.calls, s.attachments, s.blobs_stored, s.parse_errors,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
