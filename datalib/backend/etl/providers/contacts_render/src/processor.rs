//! The render step's processor for the contacts source: its planner and
//! the [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_contacts::ingest;
use datalib_etl_contacts_config::ContactsRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

/// Always planned: it renders whatever is in the raw store.
pub fn plan_render(
    ctx: PlanContext,
    config: ContactsRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        ContactsRender,
    ))
}

/// Contacts' render processor — reads the raw store and hands one
/// rendered markdown per contact to the `DocCallback`, which stores each
/// as it comes.
pub struct ContactsRender;

#[async_trait]
impl SourceRender for ContactsRender {
    const PROVIDER: &'static str = "contacts";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    fn item_of_entity(&self, source_id: &str, table: &str, id: &str) -> Option<String> {
        crate::render::google::document_of(source_id, table, id)
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse, render};

        let db_path = ingest::db_path_for(raw_path);
        let parsed = parse::parse(&db_path, ctx.raw_range())
            .with_context(|| format!("contacts parse {}", db_path.display()))?;
        let Some(parsed) = parsed else {
            return Ok("no raw store yet".into());
        };

        let mut on_doc = |md| ctx.emit_doc(md);
        let render_units = render::render_all(
            &parsed,
            ctx.name,
            ctx.progress,
            ctx.raw_range(),
            &mut on_doc,
        )
        .context("contacts render_all")?;
        ctx.finish(&render_units, parsed.head.as_deref())?;
        Ok("rendered".into())
    }
}
