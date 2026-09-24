//! The loop's record of what the pipeline did and is doing, as the loop
//! holds it in memory. It lives in `system/supervisor.sqlite`
//! (`supervisor::store`); [`changes`] is what a save has to write there.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::run_state::RunState;
use crate::step::StepId;

/// Where the record lived before the store held it. A root that still
/// has one has it imported, once, by whoever next takes the lock.
pub const LEGACY_JSON_REL_PATH: &str = "system/dag_state.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DagState {
    #[serde(default)]
    pub steps: BTreeMap<StepId, StepState>,
    /// The run in flight, or the one that finished last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run: Option<CurrentRun>,
}

/// What one run is doing, or did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// step id → what it is doing in *this* run, as
    /// [`RunState::as_str`]. Absent from the map until the scheduler
    /// reaches it, which reads as pending. Read it back with
    /// [`CurrentRun::state_of`] rather than comparing strings.
    #[serde(default)]
    pub states: BTreeMap<StepId, String>,
}

impl CurrentRun {
    /// What `step` is doing in this run. `None` both when the
    /// scheduler has not reached it and when the file names a state
    /// this build does not know.
    pub fn state_of(&self, step: &str) -> Option<RunState> {
        self.states.get(step).and_then(|s| RunState::parse(s))
    }
}

/// What a step did the last time a run reached it. Distinct from
/// [`StepState`]'s change-detection fields: those answer "is it up to
/// date", this answers "what happened, and when".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    /// The run this happened in — the key into `system/runs/runs.sqlite`,
    /// where the step's log lines and metrics for it live. Empty for a
    /// record written before runs had ids.
    #[serde(default)]
    pub run_id: String,
    pub started_at: String,
    /// `None` while it is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// The terminal [`RunState`], as [`RunState::as_str`]. Empty
    /// while running — read it back with [`LastRun::state`].
    #[serde(default)]
    pub status: String,
    /// How many attempts this took, retries included.
    #[serde(default)]
    pub attempts: u32,
    /// The failure message, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl LastRun {
    /// How the last run of this step ended. `None` while it is still
    /// running (the empty status), and for a state this build does not
    /// know.
    pub fn state(&self) -> Option<RunState> {
        RunState::parse(&self.status)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// its definition — argv, params, env overrides, and the artifact
    /// patterns it declares. Not the contents of what
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
    /// When a run last left this step current: succeeded, or checked
    /// and found up to date. Kept here rather than read from the run
    /// store, which ages runs out — and a source that has been failing
    /// for longer than that is the one whose last success matters most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<String>,
}

impl DagState {
    /// The record a root kept as `system/dag_state.json`, if it has one.
    pub fn read_legacy_json(data_root: &Path) -> Result<Option<DagState>> {
        let p = data_root.join(LEGACY_JSON_REL_PATH);
        if !p.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        let state =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", p.display()))?;
        Ok(Some(state))
    }
}

/// One thing the store must write to hold `next` where it held `prev`.
#[derive(Debug, PartialEq)]
pub enum Change<'a> {
    /// The run, and every step's state in it.
    Run(&'a CurrentRun),
    Step(&'a str, &'a StepState),
    /// A step whose record was dropped, by a reset.
    Forget(&'a str),
}

/// What changed between two records, so a save writes that and nothing
/// else: the loop saves after every event, and most change one step.
pub fn changes<'a>(prev: &'a DagState, next: &'a DagState) -> Vec<Change<'a>> {
    let mut out = Vec::new();
    if let Some(run) = next.current_run.as_ref() {
        if prev.current_run.as_ref() != Some(run) {
            out.push(Change::Run(run));
        }
    }
    for (id, step) in &next.steps {
        if prev.steps.get(id) != Some(step) {
            out.push(Change::Step(id, step));
        }
    }
    for id in prev.steps.keys() {
        if !next.steps.contains_key(id) {
            out.push(Change::Forget(id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(fingerprint: &str) -> StepState {
        StepState {
            succeeded: true,
            fingerprint: fingerprint.into(),
            output_versions: BTreeMap::from([("a/raw".into(), "v1".into())]),
            ..Default::default()
        }
    }

    /// The loop saves after every event; a save that rewrote the whole
    /// record each time would write every step on every tick.
    #[test]
    fn a_save_writes_what_changed_and_nothing_else() {
        let prev = DagState {
            steps: BTreeMap::from([
                ("a/raw".into(), step("fp-a")),
                ("b/raw".into(), step("fp-b")),
                ("c/raw".into(), step("fp-c")),
            ]),
            current_run: Some(CurrentRun {
                run_id: "r1".into(),
                ..Default::default()
            }),
        };
        assert!(changes(&prev, &prev).is_empty());

        let mut next = prev.clone();
        next.steps.insert("b/raw".into(), step("fp-b2"));
        next.steps.remove("c/raw");
        next.current_run.as_mut().unwrap().states =
            BTreeMap::from([("b/raw".into(), "running".into())]);
        assert_eq!(
            changes(&prev, &next),
            [
                Change::Run(next.current_run.as_ref().unwrap()),
                Change::Step("b/raw", &next.steps["b/raw"]),
                Change::Forget("c/raw"),
            ]
        );
    }

    /// A root from before the store still has its record as JSON, and
    /// state written before this file carried run records still reads:
    /// every field defaults.
    #[test]
    fn a_legacy_json_record_reads_whatever_it_lacks() {
        let td = tempfile::tempdir().unwrap();
        assert!(DagState::read_legacy_json(td.path()).unwrap().is_none());
        std::fs::create_dir_all(td.path().join("system")).unwrap();
        std::fs::write(
            td.path().join(LEGACY_JSON_REL_PATH),
            r#"{"steps":{"slack/raw":{"succeeded":true,"fingerprint":"fp-1"}}}"#,
        )
        .unwrap();
        let back = DagState::read_legacy_json(td.path()).unwrap().unwrap();
        assert!(back.steps["slack/raw"].succeeded);
        assert!(back.steps["slack/raw"].last_run.is_none());
        assert!(back.current_run.is_none());
    }
}
