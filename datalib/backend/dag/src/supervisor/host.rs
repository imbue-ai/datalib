//! What a process running the loop builds around it, the same whichever
//! process that is (`datalib-dag`, or the app's server): the steps'
//! environment and the run store's record of one busy period, and the
//! books a loop that died holding the lock left open.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::DagConfig;
use crate::run_state::RunState;
use crate::runs_sink::RunStoreSink;
use crate::state::DagState;
use crate::subprocess::{ENV_CHECKPOINT_CADENCE, ENV_NOW, ENV_RUN_ID};

/// The environment every step of one busy period gets on top of the
/// host's own, and the log filter it was built with.
pub struct StepEnv {
    pub vars: BTreeMap<String, String>,
    pub log_filter: String,
}

/// `binary_dir` is the host's override of the config's `binary_dir`;
/// `extra_path` goes on `PATH` after it, ahead of the host's own `PATH`.
/// `now` and `run_id` are pinned for the busy period: every stamped
/// output agrees on them.
pub fn step_env(
    cfg: &DagConfig,
    binary_dir: Option<&Path>,
    extra_path: &[PathBuf],
    now: &str,
    run_id: &str,
) -> Result<StepEnv> {
    let mut vars = BTreeMap::new();
    // So a `command` can name `datalib-step` bare.
    let dirs: Vec<PathBuf> = crate::config::resolve_binary_dir(cfg, binary_dir)
        .into_iter()
        .chain(extra_path.iter().cloned())
        .collect();
    if !dirs.is_empty() {
        let host_path = std::env::var_os("PATH");
        let all = dirs
            .into_iter()
            .chain(host_path.iter().flat_map(std::env::split_paths));
        let joined = std::env::join_paths(all).context("build the steps' PATH")?;
        vars.insert("PATH".into(), joined.to_string_lossy().into_owned());
    }
    vars.insert(ENV_RUN_ID.into(), run_id.to_string());
    vars.insert(ENV_NOW.into(), now.to_string());
    // A step's stdout is a pipe, and Python block-buffers a pipe by
    // default: its progress lines would arrive in 4KB lumps, long after
    // the stderr they belong beside. Rust and sh need no help.
    vars.insert("PYTHONUNBUFFERED".into(), "1".into());
    if let Some(cadence) = cfg.checkpoint_cadence {
        vars.insert(ENV_CHECKPOINT_CADENCE.into(), cadence.encode());
    }
    // A `RUST_LOG` already in the environment is a person's choice and
    // wins; else the config's level.
    let log_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| cfg.log_filter());
    vars.insert("RUST_LOG".into(), log_filter.clone());
    Ok(StepEnv { vars, log_filter })
}

/// The run store's record of one busy period. `None` when the store
/// could not be opened: a sync without a record beats one that refuses
/// to start over an unwritable status file.
pub fn start_record(
    data_root: &Path,
    cfg: &DagConfig,
    run_id: &str,
    now: &str,
) -> Option<RunStoreSink> {
    let retention = cfg.run_history.map(|h| h.retention()).unwrap_or_default();
    let commit = datalib_runs::git_hash_and_origin().map(|(hash, _)| hash);
    RunStoreSink::start(data_root, run_id, now, commit, retention)
}

/// Close the run a loop left open when it died holding `runner-lock`:
/// in `dag_state.json`, so no row reads its steps as live, and in the
/// run store, so none reads `running` for ever. Only for the holder of
/// the lock, which is what makes an open run a dead one. The run's id,
/// if there was one to close.
pub async fn close_dead_loop(data_root: &Path) -> Result<Option<String>> {
    let mut state = DagState::load(data_root).context("load dag state")?;
    let Some(run) = state
        .current_run
        .as_mut()
        .filter(|r| r.finished_at.is_none())
    else {
        return Ok(None);
    };
    run.finished_at = Some(crate::scheduler::now_stamp());
    let run_id = run.run_id.clone();
    state.save(data_root).context("save dag state")?;
    datalib_runs::close_abandoned_run(
        data_root,
        &run_id,
        RunState::Stopped.as_str(),
        "the loop running this ended without closing it; the next to take the lock did",
    )
    .await
    .with_context(|| format!("close run {run_id} in the run store"))?;
    Ok(Some(run_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CurrentRun;

    #[tokio::test]
    async fn a_run_a_dead_loop_left_open_is_closed_and_a_closed_one_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let open = DagState {
            current_run: Some(CurrentRun {
                run_id: "r1".into(),
                started_at: "2026-09-23T10:00:00+00:00".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        open.save(root.path()).unwrap();

        assert_eq!(
            close_dead_loop(root.path()).await.unwrap().as_deref(),
            Some("r1")
        );
        let after = DagState::load(root.path()).unwrap();
        assert!(after.current_run.unwrap().finished_at.is_some());
        assert_eq!(close_dead_loop(root.path()).await.unwrap(), None);
    }
}
