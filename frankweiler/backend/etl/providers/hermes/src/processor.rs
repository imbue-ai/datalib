//! Program-A `DataProcessor` for the `hermes` source. Translate-only: it
//! produces a rendered transcript tree without a raw store or Extract step.
//!
//! Two modes, chosen by config:
//! * `config.sync = Some(..)` → **managed local import**: discover the
//!   Hermes/OpenClaw agent history on this machine and read each root's
//!   `state.db` (read-only) + legacy `sessions/*.json`. See [`crate::local`].
//! * `config.sync = None` → **export directory**: read `common.input_path`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx, SourcePlan};
use frankweiler_etl_hermes_config::{HermesConfig, HermesSync};

/// Build the SourcePlan: a single translate processor. No extract (translate-only).
pub fn plan(ctx: PlanContext, config: HermesConfig) -> Result<SourcePlan> {
    let name = ctx.name;
    let mode = match &config.sync {
        // Managed local discovery/import (primary UX).
        Some(sync) => Mode::Local(sync.clone()),
        // Explicit export directory (advanced fallback).
        None => Mode::Export(config.common.input_or_raw_path().to_path_buf()),
    };
    let mut plan = SourcePlan::new();
    plan.translate.push(Box::new(HermesRender {
        id: format!("hermes/{name}/translate"),
        mode,
        name,
    }));
    Ok(plan)
}

enum Mode {
    /// Managed local discovery/import.
    Local(HermesSync),
    /// Read a pre-exported directory of session files.
    Export(PathBuf),
}

struct HermesRender {
    id: String,
    mode: Mode,
    name: String,
}

#[async_trait]
impl DataProcessor for HermesRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render_and_index_md::{parse::parse_export_dir, render::render_all};

        let (parsed, summary) = match &self.mode {
            Mode::Local(sync) => {
                let (parsed, stats) = crate::local::import_local(sync)
                    .await
                    .context("hermes local import")?;
                let summary = format!(
                    "imported local: sessions={} roots={} dbs={} legacy_dirs={} openclaw_files={}",
                    stats.sessions, stats.roots, stats.dbs, stats.legacy_dirs, stats.openclaw_files
                );
                (parsed, summary)
            }
            Mode::Export(input_path) => {
                let parsed = parse_export_dir(input_path)
                    .with_context(|| format!("hermes parse {}", input_path.display()))?;
                let summary = format!("rendered sessions={}", parsed.sessions.len());
                (parsed, summary)
            }
        };

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
        Ok(summary)
    }
}
