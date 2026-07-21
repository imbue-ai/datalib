//! The download step driver: one source's download wave.
//!
//! Same machinery the retired sync orchestrator installed —
//! ambient metrics, rate-limit guard, diagnostics — around the
//! provider's download `DataProcessor`s (planned per-provider by
//! [`crate::dispatch`]), which own their store
//! (open/DDL/commit/checkpoint). The step's change claim comes from
//! the provider-assembled [`DownloadReport`]: empty report → outputs
//! unchanged; non-empty → changed. A provider that publishes no
//! report gets no claim, and the scheduler content-hashes the raw
//! tree instead.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use frankweiler_etl::processor::{CheckpointSink, ReportCell, RunCtx};

use crate::dispatch::PlannedSource;
use crate::events::{Emitter, OutputClaim};

pub async fn run(
    planned: &PlannedSource,
    data_root: &Path,
    now: &str,
    control: &frankweiler_etl::control::DownloadControl,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    anyhow::ensure!(
        !planned.processors.is_empty(),
        "source {:?} (type={}) has no download work — it needs a `sync:` block \
         (or a staged input_path for file-backed sources)",
        planned.name,
        planned.type_str
    );

    let progress = emitter.progress();
    let metrics = frankweiler_etl::download_metrics::DownloadMetrics::new();
    let diagnostics = frankweiler_obs::diagnostics::Diagnostics::new();
    // Shared with the SIGINT handler: providers register their commit
    // hooks here as they open their stores, so an interrupt can seal
    // partial state with a proper dolt commit.
    let checkpoints = std::sync::Arc::new(CheckpointSink::new());
    let _ = crate::CHECKPOINTS.set(checkpoints.clone());
    let control = control.clone();
    let report_cell = ReportCell::new();
    let empty_fingerprints: HashMap<String, String> = HashMap::new();
    let guard = frankweiler_etl::retry::RetryGuard::from_params(&planned.download_params);

    let body = async {
        for proc in &planned.processors {
            let ctx = RunCtx::for_download(
                &planned.name,
                &planned.raw_path,
                now,
                &progress,
                &control,
                &empty_fingerprints,
                &checkpoints,
                metrics.clone(),
                diagnostics.clone(),
                &report_cell,
            );
            let summary = proc
                .run(&ctx)
                .await
                .with_context(|| format!("processor {}", proc.id()))?;
            tracing::info!(source = %planned.name, summary = %summary, "download: done");
        }
        Ok::<_, anyhow::Error>(())
    };
    frankweiler_obs::diagnostics::scope(
        diagnostics.clone(),
        frankweiler_etl::retry::scope(
            guard,
            frankweiler_etl::download_metrics::scope(metrics.clone(), body),
        ),
    )
    .await?;

    let report = report_cell.take();
    let Some(rel) = planned.canonical_rel(data_root, "raw") else {
        // raw_path overridden away from the canonical layout: no claim.
        return Ok(vec![]);
    };
    Ok(vec![OutputClaim {
        path: rel,
        // Only claim when the provider published a report; its
        // emptiness is the honest change signal. No report → the
        // scheduler hashes the tree.
        changed: report.as_ref().map(|r| !r.is_empty()),
        version: None,
    }])
}
