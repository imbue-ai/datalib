//! The render wave for the `garmin` source.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_garmin_config::GarminRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderPass, RenderProcessor};

/// Always planned: the page's own HEAD-vs-cursor check decides whether
/// there is work, at the cost of one `dolt_log()` query on a no-op run.
pub fn plan_render(
    ctx: PlanContext,
    config: GarminRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(GarminRender {
        id: format!("garmin/{name}/render"),
        raw_path,
        name,
    })])
}

struct GarminRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl RenderProcessor for GarminRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::parse::{parse, Parsed};
        use crate::render::render::render_all;

        match parse(&self.raw_path, ctx.raw_cursor)
            .with_context(|| format!("garmin parse {}", self.raw_path.display()))?
        {
            Parsed::UpToDate { head } => {
                tracing::info!(
                    event = "garmin_render_skipped",
                    source = %self.name,
                    head = %head,
                    "raw store HEAD unchanged since last render",
                );
                ctx.consumed(&head);
                Ok(format!("up to date at {head}"))
            }
            Parsed::Fresh(parsed) => {
                let mut on_doc = |md| ctx.emit_doc(md);
                let s = render_all(
                    &parsed,
                    ctx.root,
                    &self.name,
                    ctx.progress,
                    ctx.prior_fingerprints,
                    &mut on_doc,
                )
                .context("garmin render_all")?;
                // The whole store was read, so the set considered is the
                // complete one; the driver sweeps what it does not name.
                ctx.retain_documents(RenderPass::Walked, &s.seen);
                if let Some(head) = parsed.head.as_deref() {
                    ctx.consumed(head);
                }
                Ok(format!(
                    "weigh_ins={} devices={} plots={} emitted={}",
                    s.weigh_ins, s.devices, s.plots, s.emitted
                ))
            }
        }
    }
}
