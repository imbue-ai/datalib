//! The render wave for the beeper source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::periodize::Period;
use datalib_etl::processor::PlanContext;
use datalib_etl_beeper_config::BeeperRenderConfig;
use datalib_etl_render::processor::{RenderCtx, RenderPass, RenderProcessor};
use std::path::PathBuf;

pub fn plan_render(
    ctx: PlanContext,
    config: BeeperRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let period = Period::from_config(config.period.as_deref()).context("parse beeper period")?;
    Ok(vec![Box::new(BeeperRender {
        id: format!("beeper/{name}/render"),
        raw_path,
        name,
        period,
    })])
}

/// Beeper's render processor — reads the raw store and emits one rendered
/// markdown per `(room, period)` through the fused-Load callback.
struct BeeperRender {
    id: String,
    raw_path: PathBuf,
    name: String,
    period: Period,
}

#[async_trait]
impl RenderProcessor for BeeperRender {
    fn id(&self) -> &str {
        &self.id
    }

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let parsed = parse(&self.raw_path, self.period)
            .with_context(|| format!("beeper parse {}", self.raw_path.display()))?;
        let raw_db_path = datalib_etl::doltlite_raw::db_path_for(&self.raw_path);
        let mut on_doc = |md| ctx.emit_doc(md);
        let summary = render_all(
            &parsed,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &raw_db_path,
        )
        .context("beeper render_all")?;
        // Whole-store: every document the walk considered is what the
        // source holds; the rest is gone. A parse that read no store
        // walked nothing and says so.
        let pass = if parsed.walked {
            RenderPass::Walked
        } else {
            RenderPass::Skipped
        };
        ctx.retain_documents(pass, &summary.documents.iter().cloned().collect());
        Ok("rendered".into())
    }
}
