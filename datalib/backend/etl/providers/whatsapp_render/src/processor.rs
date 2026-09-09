//! The render wave for the whatsapp source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::periodize::Period;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use datalib_etl_whatsapp_config::WhatsappRenderConfig;
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: WhatsappRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(WhatsappRender {
        id: format!("whatsapp/{name}/render"),
        raw_path,
        name,
    })])
}

/// WhatsApp's render processor — reads the raw store and emits rendered
/// markdown through the fused-Load callback.
struct WhatsappRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for WhatsappRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse, render_all};
        // WhatsApp doesn't expose a `period` knob on its sync block today —
        // default to month bucketing, same as signal.
        let period = Period::from_config(None).context("default whatsapp period")?;
        let parsed = parse(&self.raw_path, period, &self.name)
            .with_context(|| format!("whatsapp parse {}", self.raw_path.display()))?;
        let mut dropped = 0usize;
        let mut on_doc = |md| ctx.emit_doc(md);
        let mut on_chat_gone = |chat_jid: &str| -> Result<()> {
            dropped +=
                ctx.remove_conversation(&crate::render::whatsapp_chat_uuid(&self.name, chat_jid))?;
            Ok(())
        };
        render_all(
            &parsed.chats,
            &parsed.blobs_by_chat,
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut on_chat_gone,
        )
        .context("whatsapp render_all")?;
        Ok(if dropped == 0 {
            "rendered".into()
        } else {
            format!("rendered, {dropped} document(s) gone upstream")
        })
    }
}
