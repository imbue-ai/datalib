//! The render wave for the contacts source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_contacts::ingest;
use datalib_etl_contacts_config::ContactsRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
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
        let parsed = parse::parse(&db_path, ctx.raw_range())
            .with_context(|| format!("carddav parse {}", db_path.display()))?;
        let Some(parsed) = parsed else {
            return Ok("no raw store yet".into());
        };

        let mut on_doc = |md| ctx.emit_doc(md);
        let buckets = render::render_all(
            &parsed,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.raw_range(),
            &mut on_doc,
        )
        .context("carddav render_all")?;
        for bucket in &buckets {
            ctx.declare_bucket(&bucket.key, &bucket.inputs)?;
        }
        if let Some(head) = parsed.head.as_deref() {
            ctx.consumed(head);
        }
        Ok("rendered".into())
    }
}
