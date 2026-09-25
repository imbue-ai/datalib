//! The render wave for the sms_backup_restore source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use datalib_etl_sms_backup_restore_config::SmsBackupRestoreRenderConfig;
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: SmsBackupRestoreRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(ctx, config.common.raw_path(), SmsRender))
}

/// sms_backup_restore's render processor — renders the texts + calls as one
/// chat per phone number through the fused-Load callback.
struct SmsRender;

#[async_trait]
impl SourceRender for SmsRender {
    const PROVIDER: &'static str = "sms_backup_restore";

    fn render_version(&self) -> u32 {
        crate::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        let mut on_doc = |md| ctx.emit_doc(md);
        let outcome = crate::render::render(
            raw_path,
            ctx.root,
            ctx.name,
            ctx.progress,
            &mut on_doc,
            ctx.raw_range(),
        )
        .context("sms_backup_restore render")?;

        ctx.finish(&outcome.buckets, outcome.new_head.as_deref())?;
        Ok(format!(
            "rendered={} skipped={}",
            outcome.rendered, outcome.skipped
        ))
    }
}
