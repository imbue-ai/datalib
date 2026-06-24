//! Program-A `DataProcessor`s for the `sms_backup_restore` source.
//!
//! `sms_backup_restore` is purely file-backed: there is no API and no `sync:`
//! block, so it always contributes both an **extract** ([`SmsExtract`] — ingest
//! the `sms-*.xml` / `calls-*.xml` export at `input_path`) and a **translate**
//! ([`SmsRender`] — one chat per phone number). The orchestrator only drives
//! `plan().extract` when the source is managed; for an unmanaged file source it
//! drives translate alone. The source owns its raw store end to end
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};
use frankweiler_etl_sms_backup_restore_config::SmsBackupRestoreConfig;

use crate::extract;

/// Build the SourcePlan: always an extract (file-backed ingest of the export at
/// `input_path`) plus a translate. The orchestrator only calls `plan().extract`
/// when the source is managed; the config carries no knobs (sms has none).
pub fn plan(common: PlanCommon, _config: SmsBackupRestoreConfig) -> Result<SourcePlan> {
    let PlanCommon {
        name,
        raw_path,
        input_path,
        ..
    } = common;

    let mut plan = SourcePlan::new();
    plan.extract.push(Box::new(SmsExtract {
        id: format!("sms_backup_restore/{name}/extract"),
        raw_path: raw_path.clone(),
        input_path,
    }));
    plan.translate.push(Box::new(SmsRender {
        id: format!("sms_backup_restore/{name}/translate"),
        raw_path,
        name,
    }));
    Ok(plan)
}

/// sms_backup_restore's extract processor. Owns its raw doltlite store end to
/// end (open, register interrupt hook, ingest the export, commit+close).
struct SmsExtract {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
}

#[async_trait]
impl DataProcessor for SmsExtract {
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

/// sms_backup_restore's translate processor — renders the texts + calls as one
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
        crate::render_and_index_md::render(
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
