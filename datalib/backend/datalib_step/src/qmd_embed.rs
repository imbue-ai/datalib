//! The `qmd_embed` function: vectors for one group's collection, so
//! semantic search reaches it. Optional per source — a group without
//! this step is keyword-searchable and nothing more.
//!
//! Runs `qmd embed -c <group>` until the collection has nothing pending,
//! or until `params.budget_minutes` runs out, in which case the step
//! stops with an `incomplete` outcome and the next run resumes it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_qmd_indexer::{EmbedGauge, EmbedOptions, EmbedProgress};

use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;

/// The step's own knobs, from `[steps.params]`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
    /// Wall-clock cap for one run of this step. `0` (the default) runs
    /// until the collection is fully embedded.
    #[serde(default)]
    pub budget_minutes: f64,
}

/// The error a run ends with when its budget ran out first. Classified
/// as `incomplete`, which is not a failure (`hints::classify`).
#[derive(Debug)]
pub struct BudgetSpent {
    pub pending: u64,
}

impl std::fmt::Display for BudgetSpent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "budget spent with {} documents still to embed; the next run resumes",
            self.pending
        )
    }
}

impl std::error::Error for BudgetSpent {}

struct StepProgress(datalib_etl::progress::Progress);

impl EmbedProgress for StepProgress {
    fn gauge(&self, g: &EmbedGauge) {
        self.0.metric("queued", &[], g.pending as i64);
        self.0.metric("done", &[], g.embedded() as i64);
        self.0.metric("documents", &[], g.active as i64);
        self.0.metric("chunks", &[], g.chunks as i64);
    }
    fn message(&self, m: &str) {
        self.0.set_message(m);
    }
}

pub async fn run(
    data_root: &Path,
    env: &StepEnv,
    params: serde_json::Value,
    models_dir: Option<PathBuf>,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        serde_json::from_value(params).context("qmd_embed params")?
    };
    let progress = emitter.progress();
    progress.set_message("qmd embed");
    let qmd_version = datalib_qmd_indexer::DEFAULT_QMD_VERSION;
    let models_dir = models_dir.unwrap_or_else(datalib_qmd_indexer::default_models_dir);
    datalib_qmd_indexer::prepare_store(data_root, &models_dir)?;

    let opts = EmbedOptions {
        root: data_root.to_path_buf(),
        group: env.group.clone(),
        qmd_version: qmd_version.to_string(),
        budget: (params.budget_minutes > 0.0)
            .then(|| Duration::from_secs_f64(params.budget_minutes * 60.0)),
        pull_if_missing: true,
        models_dir,
    };
    let reporter = StepProgress(progress.clone());
    // Blocking: holds the embed lock and waits on qmd.
    let outcome =
        tokio::task::spawn_blocking(move || datalib_qmd_indexer::embed_group(&opts, &reporter))
            .await
            .context("embed task panicked")?
            .with_context(|| format!("embed group {}", env.group))?;
    StepProgress(progress).gauge(&outcome.gauge);
    tracing::info!(
        group = %env.group,
        sessions = outcome.sessions,
        complete = outcome.complete,
        embedded = outcome.gauge.embedded(),
        pending = outcome.gauge.pending,
        "qmd_embed: done"
    );

    // As with `qmd_index`: the vectors live in the shared store, and the
    // step's own tree is an empty directory.
    std::fs::create_dir_all(data_root.join(&env.step))?;

    if !outcome.complete {
        return Err(BudgetSpent {
            pending: outcome.gauge.pending,
        }
        .into());
    }
    // Nothing consumes this tree, so the version only has to be honest:
    // what the collection's vectors cover, which is what the step is
    // for.
    Ok(vec![OutputClaim {
        path: env.step.clone(),
        version: format!(
            "{}:{}/{}",
            qmd_version,
            outcome.gauge.embedded(),
            outcome.gauge.active
        ),
        rows: Some(outcome.gauge.embedded()),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_default_to_no_budget_and_refuse_unknown_keys() {
        let p: Params = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(p.budget_minutes, 0.0);
        let p: Params = serde_json::from_value(serde_json::json!({"budget_minutes": 30})).unwrap();
        assert_eq!(p.budget_minutes, 30.0);
        assert!(serde_json::from_value::<Params>(serde_json::json!({"budget": 3})).is_err());
    }
}
