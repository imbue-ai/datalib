//! The render wave for the sms_backup_restore source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use datalib_etl_sms_backup_restore_config::SmsBackupRestoreRenderConfig;
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: SmsBackupRestoreRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(SmsRender {
        id: format!("sms_backup_restore/{name}/render"),
        raw_path,
        name,
    })])
}

/// sms_backup_restore's render processor — renders the texts + calls as one
/// chat per phone number through the fused-Load callback.
struct SmsRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for SmsRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
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
