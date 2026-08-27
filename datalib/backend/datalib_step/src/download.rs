//! The download step driver: one source's download wave.
//!
//! Same machinery the retired sync orchestrator installed —
//! ambient metrics, rate-limit guard, diagnostics — around the
//! provider's download `DataProcessor`s (planned per-provider by
//! [`crate::dispatch`]), which own their store
//! (open/DDL/commit/checkpoint). The step reports its raw store's
//! doltlite HEAD commits as the output version: doltlite only advances
//! HEAD when a commit changed something, so a poll that found nothing
//! new reports the same string as last run and the render skips.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::processor::{CheckpointSink, RunCtx};

use crate::dispatch::PlannedSource;
use crate::events::{Emitter, OutputClaim};

pub async fn run(
    planned: &PlannedSource,
    data_root: &Path,
    now: &str,
    control: &datalib_etl::control::DownloadControl,
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
    let metrics = datalib_etl::download_metrics::DownloadMetrics::new();
    let diagnostics = datalib_obs::diagnostics::Diagnostics::new();
    // Shared with the SIGINT handler: providers register their commit
    // hooks here as they open their stores, so an interrupt can seal
    // partial state with a proper dolt commit.
    let checkpoints = std::sync::Arc::new(CheckpointSink::new());
    let _ = crate::CHECKPOINTS.set(checkpoints.clone());
    let control = control.clone();
    let empty_fingerprints: HashMap<String, String> = HashMap::new();
    let guard = datalib_etl::retry::RetryGuard::from_params(&planned.download_params);

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
            );
            let summary = proc
                .run(&ctx)
                .await
                .with_context(|| format!("processor {}", proc.id()))?;
            tracing::info!(source = %planned.name, summary = %summary, "download: done");
        }
        Ok::<_, anyhow::Error>(())
    };
    datalib_obs::diagnostics::scope(
        diagnostics.clone(),
        datalib_etl::retry::scope(
            guard,
            datalib_etl::download_metrics::scope(metrics.clone(), body),
        ),
    )
    .await?;

    let Some(rel) = planned.canonical_rel(data_root, "raw") else {
        // raw_path overridden away from the canonical layout: no claim.
        return Ok(vec![]);
    };
    // Never fail the step here: the download itself has completed and
    // committed. A version we cannot read is a reason to fall back to
    // the runner's hash, not to throw away hours of successful work and
    // block every downstream step.
    match raw_store_version(&data_root.join(&rel)).await {
        Ok(Some(version)) => Ok(vec![OutputClaim { path: rel, version }]),
        // Stock-sqlite dev build, or nothing materialized yet: no
        // version we can vouch for, so let the runner hash instead.
        Ok(None) => Ok(vec![]),
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "download: could not read the raw store version;                  the runner will content-hash the tree instead"
            );
            Ok(vec![])
        }
    }
}

/// A content version for one source's `raw/` tree: the HEAD commit of
/// each doltlite store under it.
///
/// doltlite only advances HEAD when a commit actually changed
/// something, so a poll that found nothing new leaves both hashes
/// alone and the version is byte-identical to last run's. That is what
/// lets the runner skip the render without the download having to
/// assert anything — and it avoids reading the whole store (blob CAS
/// included) just to hash it.
async fn raw_store_version(raw_dir: &Path) -> Result<Option<String>> {
    use datalib_etl::doltlite_raw::head_commit_at_path;
    let entities = head_commit_at_path(&datalib_etl::raw_layout::entities_db(raw_dir))
        .await
        .context("entities head")?;
    let blobs = head_commit_at_path(&datalib_etl::raw_layout::blobs_db(raw_dir))
        .await
        .context("blobs head")?;
    if entities.is_none() && blobs.is_none() {
        return Ok(None);
    }
    Ok(Some(format!(
        "entities:{} blobs:{}",
        entities.as_deref().unwrap_or("-"),
        blobs.as_deref().unwrap_or("-")
    )))
}
