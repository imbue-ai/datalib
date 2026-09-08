//! Program-A `DataProcessor`s for the `sms_backup_restore` source.

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_sms_backup_restore_config::SmsBackupRestoreConfig;
use datalib_etl_sms_backup_restore_config::SmsBackupRestoreRenderConfig;

use crate::download;

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
    config: SmsBackupRestoreRenderConfig,
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
            cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
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

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, &self.name);
        let cursor = datalib_etl::render_cursor::read_for_params(
            &cursor_path,
            &datalib_etl::render_cursor::no_params(),
        )
        .with_context(|| format!("read sms render cursor {}", cursor_path.display()))?;

        let mut on_doc = |md| ctx.emit_doc(md);
        let outcome = crate::render::render(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .context("sms_backup_restore render")?;

        // Named, not swept: the render above is narrowed by the diff, so
        // what it emitted is only what changed.
        let mut dropped = 0usize;
        for chat_uuid in &outcome.vanished {
            dropped += ctx.remove_conversation(chat_uuid)?;
        }

        if let Some(head) = outcome.new_head.as_deref() {
            datalib_etl::render_cursor::write(
                &cursor_path,
                head,
                outcome.scan_elapsed,
                &datalib_etl::render_cursor::no_params(),
            )
            .with_context(|| format!("write sms render cursor {}", cursor_path.display()))?;
        }
        Ok(format!(
            "rendered={} skipped={} dropped={}",
            outcome.rendered, outcome.skipped, dropped
        ))
    }
}
