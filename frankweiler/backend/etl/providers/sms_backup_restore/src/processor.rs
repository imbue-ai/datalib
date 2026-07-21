//! Program-A `DataProcessor`s for the `sms_backup_restore` source.
//!
//! `sms_backup_restore` is purely file-backed: there is no API and no `sync:`
//! block, so it always contributes both an **download** ([`SmsDownload`] — ingest
//! the `sms-*.xml` / `calls-*.xml` export at `input_path`) and a **render**
//! ([`SmsRender`] — one chat per phone number). The orchestrator only drives
//! `plan_download` returns it only when the source is managed; for an unmanaged file source it
//! drives render alone. The source owns its raw store end to end
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_sms_backup_restore_config::SmsBackupRestoreConfig;

use crate::download;

/// Download wave: walk the export dir at input_path into the raw store.
pub fn plan_download(
    ctx: PlanContext,
    config: SmsBackupRestoreConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let input_path = config.common.input_or_raw_path().to_path_buf();
    Ok(vec![Box::new(SmsDownload {
        id: format!("sms_backup_restore/{name}/download"),
        raw_path,
        input_path,
    })])
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: SmsBackupRestoreConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(SmsRender {
        id: format!("sms_backup_restore/{name}/render"),
        raw_path,
        name,
    })])
}

/// sms_backup_restore's download processor. Owns its raw doltlite store end to
/// end (open, register interrupt hook, ingest the export, commit+close).
struct SmsDownload {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
}

#[async_trait]
impl DataProcessor for SmsDownload {
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

/// sms_backup_restore's render processor — renders the texts + calls as one
/// chat per phone number through the fused-Load callback.
struct SmsRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for SmsRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let mut on_doc = |md| ctx.emit_doc(md);
        crate::render::render(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("sms_backup_restore render")?;
        Ok("rendered".into())
    }
}
