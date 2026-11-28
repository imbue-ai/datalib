//! The ingest step driver: one source's ingest processors, written to the
//! tree the step id names.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::processor::RunCtx;
use datalib_etl::progress::Progress;
use datalib_schema::problems::{Severity, METRIC};

use crate::events::{Emitter, OutputClaim};
use crate::planned_source::{PlannedSource, Processors};

pub async fn run(
    planned: &PlannedSource,
    tree_rel: &str,
    now: &str,
    control: &datalib_etl::control::DownloadControl,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let processors = match &planned.processors {
        Processors::Ingest(processors) => processors,
        Processors::Kept => return keep(planned, tree_rel, emitter).await,
        Processors::Render(_) => anyhow::bail!(
            "the download driver was handed source {:?}'s render processors",
            planned.name
        ),
    };
    anyhow::ensure!(
        !processors.is_empty(),
        "source {:?} (type={}) has no download work — its params name no ingest method",
        planned.name,
        planned.source_type
    );

    tracing::info!(
        source = %planned.name,
        reach = planned.reach.map(|r| r.as_str()),
        "the ingest method the provider declared",
    );
    let progress = emitter.progress();
    let metrics = datalib_etl::download_metrics::DownloadMetrics::publishing_to(progress.clone());
    // Every processor of this source's ingest step writes the one raw
    // store, so the step can only claim what all of them can support. `all` on an empty
    // iterator is `true`, which is why the emptiness check above matters.
    emitter.declare_streams_output(processors.iter().all(|p| p.streams_output()));
    let guard = datalib_etl_web::retry::RetryGuard::from_params(
        &planned.download_params,
        control.stop.clone(),
    );

    let body = async {
        for proc in processors {
            let ctx = RunCtx::new(
                &planned.name,
                &planned.raw_path,
                now,
                &progress,
                control,
                metrics.clone(),
            );
            let summary = proc
                .run(&ctx)
                .await
                .with_context(|| format!("processor {}", proc.id()))?;
            tracing::info!(source = %planned.name, summary = %summary, "the download is done");
            // The Manage row keeps the last message it was sent, which
            // mid-walk is about one channel or one phase, not the run.
            progress.set_message(&summary.to_string());
        }
        Ok::<_, anyhow::Error>(())
    };
    let (ran, end) = datalib_etl::run_problems::noting_run_end(datalib_etl_web::retry::scope(
        guard.clone(),
        datalib_etl::download_metrics::scope(metrics.clone(), body),
    ))
    .await;
    // A source that stopped answering ends the run however the processor
    // took its last refused request, even when that was its last request.
    let stopped_answering = guard.gave_up().map(str::to_string);
    match (ran, &stopped_answering) {
        (Err(e), Some(_)) => tracing::info!("after giving up: {e:#}"),
        (ran, _) => ran?,
    }
    let gave_up = end.gave_up.or(stopped_answering);
    // A local file that will not parse fails the same way every run until
    // someone changes it: its error row says so, and nothing is owed.
    let unfinished = end
        .unfinished
        .filter(|_| planned.reach == Some(datalib_source_common::Reach::Origin));

    report_problems(&progress, &planned.raw_path).await;

    // What it got is committed; failing now is how the Manage row says
    // the rest is not, and the outcome still reports the version.
    if let Some(said) = gave_up {
        return Err(datalib_etl::run_problems::GaveUp(said).into());
    }
    if let Some(said) = unfinished {
        return Err(datalib_etl::run_problems::Unfinished(said).into());
    }

    published(planned, tree_rel).await
}

/// A source that no longer downloads: what its last download left is what
/// it publishes, so everything reading it stays up to date.
async fn keep(
    planned: &PlannedSource,
    tree_rel: &str,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let store = datalib_etl::raw_layout::entities_db(&planned.raw_path);
    anyhow::ensure!(
        store.exists(),
        "source {:?} does not download (`common.download = false`), and has no raw store to \
         keep at {}",
        planned.name,
        store.display()
    );
    tracing::info!(source = %planned.name, "not downloading: the raw store is kept as it is");
    let progress = emitter.progress();
    progress.set_message("Not downloading: keeps what it has");
    report_problems(&progress, &planned.raw_path).await;
    published(planned, tree_rel).await
}

/// What the download could not fetch, counted the way every render step
/// counts its own: the Manage row's errors and warnings. Whole store, zero
/// included; never a reason to fail the step.
async fn report_problems(progress: &Progress, raw_path: &Path) {
    match datalib_etl::doltlite_raw::problem_counts_at_path(&datalib_etl::raw_layout::entities_db(
        raw_path,
    ))
    .await
    {
        Ok(counts) => {
            for severity in [Severity::Error, Severity::Warning] {
                progress.metric(
                    METRIC,
                    &[severity.metric_label()],
                    counts.get(&severity).copied().unwrap_or(0),
                );
            }
        }
        Err(e) => tracing::warn!(
            error = %format!("{e:#}"),
            "could not count the raw store's problems; the Manage row shows none"
        ),
    }
}

/// The raw store's version, as the step's output claim. A version that
/// cannot be read costs the readers a run they may not have needed, so
/// it never fails the step.
async fn published(planned: &PlannedSource, tree_rel: &str) -> Result<Vec<OutputClaim>> {
    match raw_store_version(&planned.raw_path).await {
        Ok(Some(version)) => Ok(vec![OutputClaim {
            path: tree_rel.to_string(),
            version,
            // The last segment's size is inside the session that sealed
            // it; a consumer's queue from a download reads the seals
            // and treats the finish as "nothing more to come".
            rows: None,
        }]),
        // Stock-sqlite dev build, or nothing materialized yet: no
        // version we can vouch for, so every success reads as new.
        Ok(None) => Ok(vec![]),
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "could not read the raw store version; every step reading it runs again"
            );
            Ok(vec![])
        }
    }
}

/// The entities store's version: the last commit at which content moved,
/// spelled as a seal spells it. Every edge row names bytes the blob CAS
/// already holds, so the entities store's commit is the whole of what a
/// render can read.
pub async fn raw_store_version(raw_dir: &Path) -> Result<Option<String>> {
    datalib_etl::doltlite_raw::content_commit_at_path(&datalib_etl::raw_layout::entities_db(
        raw_dir,
    ))
    .await
    .context("entities version")
}
