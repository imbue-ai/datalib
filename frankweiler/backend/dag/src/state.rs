//! Persisted scheduler state: per step, the input/output artifact
//! versions as of its last successful run. This is what makes "did my
//! inputs change since I last ran?" answerable across process
//! restarts.
//!
//! Lives at `<data_root>/system/state/dag_state.json` — alongside the
//! other operational (non-rebuildable-from-raw) state per the layout
//! doc. JSON for now; the open question of moving this into a
//! `pipeline_runs` table in the control DB is noted in the design doc.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::step::StepId;

pub const STATE_REL_PATH: &str = "system/state/dag_state.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DagState {
    #[serde(default)]
    pub steps: BTreeMap<StepId, StepState>,
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
            "slack.download".into(),
            StepState {
                input_versions: BTreeMap::new(),
                output_versions: BTreeMap::from([("slack/raw".into(), "abc".into())]),
                succeeded: true,
            },
        );
        st.save(td.path()).unwrap();
        let back = DagState::load(td.path()).unwrap();
        assert!(back.steps["slack.download"].succeeded);
        assert_eq!(
            back.steps["slack.download"].output_versions["slack/raw"],
            "abc"
        );
    }

    #[test]
    fn missing_file_is_empty_state() {
        let td = tempfile::tempdir().unwrap();
        assert!(DagState::load(td.path()).unwrap().steps.is_empty());
    }
}
