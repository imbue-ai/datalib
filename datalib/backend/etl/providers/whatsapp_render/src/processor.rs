//! The render wave for the whatsapp source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::periodize::Period;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use datalib_etl_whatsapp_config::WhatsappRenderConfig;
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: WhatsappRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        WhatsappRender,
    ))
}

/// WhatsApp's render processor — reads the raw store and emits rendered
/// markdown through the fused-Load callback.
struct WhatsappRender;

#[async_trait]
impl SourceRender for WhatsappRender {
    const PROVIDER: &'static str = "whatsapp";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse, render_all};
        // WhatsApp doesn't expose a `period` knob on its sync block today —
        // default to month bucketing, same as signal.
        let period = Period::from_config(None).context("default whatsapp period")?;
        let parsed = parse(raw_path, period, ctx.name, ctx.raw_range())
            .with_context(|| format!("whatsapp parse {}", raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let (consumed, buckets) = render_all(
            &parsed.chats,
            &parsed.blobs_by_chat,
            raw_path,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.raw_range(),
            &mut on_doc,
        )
        .context("whatsapp render_all")?;
        ctx.finish(&buckets, consumed.as_deref())?;
        Ok("rendered".into())
    }
}
