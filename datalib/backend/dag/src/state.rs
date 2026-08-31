//! Persisted scheduler state, and the only record of what the pipeline
//! is doing.
//!
//! Two things live here, written by the runner and by nothing else:
//!
//! * **Change-detection bookkeeping** — per step, the input/output
//!   artifact versions and the fingerprint as of its last successful
//!   run. This is what makes "is this step still up to date with the
//!   inputs and the config it would run under?" answerable across
//!   process restarts.
//! * **Run state** — what each step did last ([`LastRun`]) and what the
//!   run in flight is doing right now ([`CurrentRun`]). The UI reads
//!   this rather than inferring from the job queue, which records whole
//!   *runs* and so can only guess at a step.
//!
//! The second exists because the runner is the only process that knows
//! the plan. A step knows what it did; only the scheduler knows what is
//! queued, what is blocked, and on what. Putting it here means a run
//! started from a terminal is as visible as one the UI kicked off —
//! `datalib-dag` writes this either way.
//!
//! **State transitions, not progress ticks.** A step going
//! running→succeeded lands here; "347 of 900 messages" does not. That
//! keeps writes at O(steps) per run, the same as before this file
//! carried run state, and leaves live progress to the NDJSON event
//! stream where a subscriber already gets it push-shaped. A CLI run
//! showing "running" with no bar is honest rather than impoverished.
//!
//! Lives at `<data_root>/system/dag_state.json` — alongside the other
//! operational (non-rebuildable-from-raw) state per the layout doc.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::step::StepId;

pub const STATE_REL_PATH: &str = "system/dag_state.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DagState {
    #[serde(default)]
    pub steps: BTreeMap<StepId, StepState>,
    /// The run in flight, or the one that finished last.
    ///
    /// Not cleared when a run ends — it is stamped with `finished_at`
    /// instead, so "nothing is running, and here is what happened last"
    /// and "nothing has ever run" stay distinguishable. A reader treats
    /// `finished_at == None` as live.
    ///
    /// A crashed runner leaves this without a `finished_at` forever,
    /// which reads as "still running" and is the same lie
    /// `sync_runs.status = 'running'` tells for the same reason. The
    /// runner's lock is what bounds it: a reader that finds no lock
    /// holder knows nothing is actually running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run: Option<CurrentRun>,
}

/// What one run is doing, or did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CurrentRun {
    /// Identifies this run in logs and in the UI. Not a UUID: the
    /// start timestamp is unique enough for a single-writer store and
    /// is readable in a filename or an error message.
    pub run_id: String,
    pub started_at: String,
    /// `None` while the run is in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Every step this run will consider, in topological order — the
    /// same list [`crate::events::Event::RunPlan`] announces. Steps
    /// outside a `--sync` subset are included: "not selected" is a
    /// state worth showing.
    #[serde(default)]
    pub plan: Vec<StepId>,
    /// step id → what it is doing in *this* run. Absent from the map
    /// until the scheduler reaches it, which reads as pending.
    #[serde(default)]
    pub states: BTreeMap<StepId, String>,
}

/// What a step did the last time a run reached it. Distinct from
/// [`StepState`]'s change-detection fields: those answer "is it up to
/// date", this answers "what happened, and when".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LastRun {
    pub started_at: String,
    /// `None` while it is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// The serialized [`crate::scheduler::StepStatus`] discriminant:
    /// `succeeded` | `skipped_up_to_date` | `blocked` | `failed` |
    /// `not_selected`. Empty while running.
    #[serde(default)]
    pub status: String,
    /// How many attempts this took, retries included.
    #[serde(default)]
    pub attempts: u32,
    /// The failure message, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepState {
    /// Concrete input artifact path → version observed when this step
    /// last *succeeded*. A failed run never updates this, so the step
    /// stays dirty until it completes.
    #[serde(default)]
    pub input_versions: BTreeMap<String, String>,
    /// Declared output path → version after the last run that touched
    /// it (successful or not — a failed incremental step may still
    /// have committed partial output, and honesty here is what lets
    /// the next run see it).
    #[serde(default)]
    pub output_versions: BTreeMap<String, String>,
    /// Whether the step has ever completed successfully.
    #[serde(default)]
    pub succeeded: bool,
    /// The step's own fingerprint as of its last success: a hash over
    /// its definition — argv (which carries `--params`), env overrides,
    /// and the artifact patterns it declares. Not the contents of what
    /// it reads; those are `input_versions`. A step whose fingerprint no longer matches is
    /// stale even when every input is untouched, which is how a config
    /// edit takes effect. Empty for state written before fingerprints
    /// existed; treated as "unknown", which forces one re-run.
    #[serde(default)]
    pub fingerprint: String,
    /// What happened the last time a run reached this step, whatever
    /// the outcome. This is what lets the UI say "last synced" per step
    /// exactly, instead of attributing a whole run's timestamp to every
    /// source it named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<LastRun>,
}

impl DagState {
    pub fn path(data_root: &Path) -> PathBuf {
        data_root.join(STATE_REL_PATH)
    }

    pub fn load(data_root: &Path) -> Result<DagState> {
        let p = Self::path(data_root);
        if !p.exists() {
            return Ok(DagState::default());
        }
        let bytes = std::fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", p.display()))
    }

    /// Atomic (write-temp-then-rename) save, honoring the same
    /// valid-or-absent rule we ask of step outputs.
    pub fn save(&self, data_root: &Path) -> Result<()> {
        let p = Self::path(data_root);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &p).with_context(|| format!("rename to {}", p.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let td = tempfile::tempdir().unwrap();
        let mut st = DagState::default();
        st.steps.insert(
            "slack/raw".into(),
            StepState {
                input_versions: BTreeMap::new(),
                output_versions: BTreeMap::from([("slack/raw".into(), "abc".into())]),
                succeeded: true,
                fingerprint: "fp-1".into(),
                last_run: Some(LastRun {
                    started_at: "2026-08-31T10:00:00+01:00".into(),
                    finished_at: Some("2026-08-31T10:00:09+01:00".into()),
                    status: "succeeded".into(),
                    attempts: 1,
                    error: None,
                }),
            },
        );
        st.current_run = Some(CurrentRun {
            run_id: "2026-08-31T10:00:00+01:00".into(),
            started_at: "2026-08-31T10:00:00+01:00".into(),
            finished_at: Some("2026-08-31T10:00:10+01:00".into()),
            plan: vec!["slack/raw".into(), "slack/rendered_md".into()],
            states: BTreeMap::from([("slack/raw".into(), "succeeded".into())]),
        });
        st.save(td.path()).unwrap();

        let back = DagState::load(td.path()).unwrap();
        let step = &back.steps["slack/raw"];
        assert!(step.succeeded);
        assert_eq!(step.output_versions["slack/raw"], "abc");
        assert_eq!(step.fingerprint, "fp-1");
        let last = step.last_run.as_ref().expect("last_run survives the trip");
        assert_eq!(last.status, "succeeded");
        assert_eq!(last.attempts, 1);
        assert_eq!(
            last.finished_at.as_deref(),
            Some("2026-08-31T10:00:09+01:00")
        );

        let run = back.current_run.expect("current_run survives the trip");
        assert_eq!(run.plan.len(), 2);
        assert_eq!(run.states["slack/raw"], "succeeded");
        assert!(run.finished_at.is_some(), "a closed run stays closed");
    }

    /// State written before this file carried run records still loads:
    /// every new field is `#[serde(default)]`, and a missing `last_run`
    /// reads as "we have no record", not as an error. Without this the
    /// first run after an upgrade would fail to load its own state.
    #[test]
    fn state_without_run_records_still_loads() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join("system")).unwrap();
        std::fs::write(
            DagState::path(td.path()),
            r#"{"steps":{"slack/raw":{"succeeded":true,"fingerprint":"fp-1"}}}"#,
        )
        .unwrap();
        let back = DagState::load(td.path()).unwrap();
        assert!(back.steps["slack/raw"].succeeded);
        assert!(back.steps["slack/raw"].last_run.is_none());
        assert!(back.current_run.is_none());
    }

    #[test]
    fn missing_file_is_empty_state() {
        let td = tempfile::tempdir().unwrap();
        assert!(DagState::load(td.path()).unwrap().steps.is_empty());
    }
}
