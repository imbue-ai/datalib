//! Program-A `DataProcessor` for the `hermes` source. Translate-only and
//! file-backed: it reads the export directory at `common.input_path` directly
//! (no raw store, no Extract step) and renders the sessions to Markdown +
//! `grid_rows`. The orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx, SourcePlan};
use frankweiler_etl_hermes_config::HermesConfig;

/// Build the SourcePlan: a single translate processor that reads the export
/// directory and renders it. No extract (file-backed, translate-only).
pub fn plan(ctx: PlanContext, config: HermesConfig) -> Result<SourcePlan> {
    let name = ctx.name;
    // Read straight from the configured export dir; `input_or_raw_path` returns
    // the resolved `input_path` for a file-backed source.
    let input_path = config.common.input_or_raw_path().to_path_buf();
    let mut plan = SourcePlan::new();
    plan.translate.push(Box::new(HermesRender {
        id: format!("hermes/{name}/translate"),
        input_path,
        name,
    }));
    Ok(plan)
}

struct HermesRender {
    id: String,
    input_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for HermesRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render_and_index_md::{parse::parse_export_dir, render::render_all};
        let parsed = parse_export_dir(&self.input_path)
            .with_context(|| format!("hermes parse {}", self.input_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(
            &parsed,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("hermes render_all")?;
        Ok(format!("rendered sessions={}", parsed.sessions.len()))
    }
}
