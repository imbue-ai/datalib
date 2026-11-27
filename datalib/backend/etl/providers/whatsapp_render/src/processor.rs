//! The render step for the whatsapp source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_render::processor::{
    plan_source_render, ReadScope, RenderCtx, RenderProcessor, SourceRender,
};
use datalib_etl_whatsapp_config::WhatsappRenderConfig;
use datalib_schema::problems::{Outcome, ProblemRow, Scope, Stage};
use std::path::Path;

/// One render processor, always: it renders whatever is in the raw store.
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

/// WhatsApp's render processor — reads the raw store and emits each
/// rendered document through `RenderCtx::emit_doc`.
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
        let parsed = parse(raw_path, ctx.name, ctx.raw_range())
            .with_context(|| format!("whatsapp parse {}", raw_path.display()))?;
        let Some(parsed) = parsed else {
            return Ok("no msgstore mirrored yet".into());
        };
        let dropped: Vec<ProblemRow> = parsed
            .dropped
            .iter()
            .map(|d| {
                ProblemRow::new(
                    ctx.name,
                    Stage::Parse,
                    Scope::Entity(&d.entity_id()),
                    None,
                    Outcome::Dropped,
                    d.problem.clone(),
                    Some(self.render_version()),
                )
            })
            .collect();
        ctx.report_entity_problems(
            &ReadScope::Whole(crate::render::parse::READ_WHOLE.to_vec()),
            &dropped,
        )?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let (consumed, render_units) = render_all(
            &parsed.chats,
            &parsed.commit,
            raw_path,
            ctx.name,
            ctx.progress,
            ctx.raw_range(),
            ctx.page_size.bytes,
            &mut on_doc,
        )
        .context("whatsapp render_all")?;
        ctx.finish(&render_units, consumed.as_deref())?;
        Ok("rendered".into())
    }
}
