//! The ingest step driver: one source's ingest wave, written to the
//! tree the step id names.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::processor::{CheckpointSink, RunCtx};

use crate::dispatch::{PlannedSource, Wave};
use crate::events::{Emitter, OutputClaim};

pub async fn run(
    planned: &PlannedSource,
    tree_rel: &str,
    now: &str,
    control: &datalib_etl::control::DownloadControl,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let Wave::Ingest(processors) = &planned.processors else {
        anyhow::bail!(
            "the download driver was handed source {:?}'s render wave",
            planned.name
        );
    };
    anyhow::ensure!(
        !processors.is_empty(),
        "source {:?} (type={}) has no download work — its params name no ingest method",
        planned.name,
        planned.source_type
    );

    tracing::info!(
        source = %planned.name,
        reach = ?planned.reach,
        "download: ingest method declared by the provider",
    );
    let progress = emitter.progress();
    let metrics = datalib_etl::download_metrics::DownloadMetrics::publishing_to(progress.clone());
    let diagnostics = datalib_obs::diagnostics::Diagnostics::new();
    // Shared with the SIGINT handler: providers register their commit
    // hooks here as they open their stores, so an interrupt can seal
    // partial state with a proper dolt commit.
    let checkpoints = std::sync::Arc::new(CheckpointSink::new());
    let _ = crate::CHECKPOINTS.set(checkpoints.clone());
    // `always_clear_before_ingest` is the same wipe `--reset-and-redownload`
    // performs, asked for by config rather than by a flag: every provider
    // already truncates its entity tables and clears its cursors on that
    // knob, so a source whose input is a complete snapshot gets deletions
    // by re-writing from scratch.
    let control = datalib_etl::control::DownloadControl {
        reset_and_redownload: control.reset_and_redownload || planned.always_clear_before_ingest,
        ..control.clone()
    };
    if planned.always_clear_before_ingest {
        tracing::info!(
            source = %planned.name,
            "download: always_clear_before_ingest — wiping this source's entity \
             tables so anything its input has dropped falls out (the old rows \
             stay in doltlite history)",
        );
    }
    // Every processor in this source's wave writes the one raw store, so the
    // step can only claim what all of them can support. `all` on an empty
    // iterator is `true`, which is why the emptiness check above matters.
    emitter.declare_streams_output(processors.iter().all(|p| p.streams_output()));
    let guard = datalib_etl::retry::RetryGuard::from_params(&planned.download_params);

    let body = async {
        for proc in processors {
            let ctx = RunCtx::new(
                &planned.name,
                &planned.raw_path,
                now,
                &progress,
                &control,
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

    // Never fail the step here: the download itself has completed and
    // committed. A version we cannot read is a reason to fall back to
    // the runner's hash, not to throw away hours of successful work and
    // block every downstream step.
    match raw_store_version(&planned.raw_path).await {
        Ok(Some(version)) => Ok(vec![OutputClaim {
            path: tree_rel.to_string(),
            version,
        }]),
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
