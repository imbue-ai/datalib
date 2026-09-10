//! The render wave for the contacts source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_contacts::ingest;
use datalib_etl_contacts_config::ContactsRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderPass, RenderProcessor};
use std::path::PathBuf;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: ContactsRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(ContactsRender {
        id: format!("carddav/{name}/render"),
        raw_path,
        name,
    })])
}

/// Carddav's render processor — reads the raw store and emits one
/// rendered markdown per contact through the fused-Load callback.
pub struct ContactsRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for ContactsRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse, render};

        let db_path = ingest::db_path_for(&self.raw_path);
        let parsed = parse::parse(&db_path)
            .with_context(|| format!("carddav parse {}", db_path.display()))?;
        let Some(parsed) = parsed else {
            // No store yet: nothing was walked, so the sweep must not run.
            return Ok("no raw store yet".into());
        };

        // This renderer walks the whole raw store every run, so the set it
        // considered is the complete one: anything else the render store
        // holds is a document whose source is gone. The driver sweeps.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut on_doc = |md| ctx.emit_doc(md);
        render::render_all(
            &parsed,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut seen,
        )
        .context("carddav render_all")?;
        // `render_all` itself has no early return, and the one bail above
        // returned already — so reaching here means the store was there and
        // this pass walked all of it.
        ctx.retain_documents(RenderPass::Walked, &seen);
        Ok("rendered".into())
    }
}
