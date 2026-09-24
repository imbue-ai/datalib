//! What a process running the loop builds around it, the same whichever
//! process that is (`datalib-dag`, or the app's server): the steps'
//! environment and the run store's record of one busy period, and what it
//! puts right when it takes the lock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::DagConfig;
use crate::run_state::RunState;
use crate::runs_sink::RunStoreSink;
use crate::subprocess::{ENV_CHECKPOINT_CADENCE, ENV_NOW, ENV_RUN_ID};
use crate::supervisor::store::Store;

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

/// What [`take_over`] found to put right.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TakenOver {
    /// A `system/dag_state.json` it brought into the store.
    pub imported_legacy: bool,
    /// The run a dead loop left open, which it closed.
    pub closed_run: Option<String>,
    /// How many of that loop's invocations it closed as stopped.
    pub closed_invocations: u64,
}

/// For the process that has just taken `runner-lock`, before its first
/// loop: bring in a root's `dag_state.json` if it still has one, and close
/// what a loop that died holding the lock left open — its run, in the
/// record and in the run store, so no row reads its steps as live, and its
/// invocations. Holding the lock is what makes anything open a thing a
/// dead loop left.
pub async fn take_over(store: &Store, data_root: &Path) -> Result<TakenOver> {
    let imported_legacy = store
        .import_legacy_record(data_root)
        .await
        .context("import system/dag_state.json")?;
    let why = "the loop running this ended without closing it; the next to take the lock did";
    let saved = store.load_record().await.context("load the record")?;
    let mut state = saved.clone();
    let closed_run = match state.current_run.as_mut() {
        Some(run) if run.finished_at.is_none() => {
            run.finished_at = Some(crate::scheduler::now_stamp());
            Some(run.run_id.clone())
        }
        _ => None,
    };
    store.save_record(&saved, &state).await?;
    if let Some(run_id) = &closed_run {
        datalib_runs::close_abandoned_run(data_root, run_id, RunState::Stopped.as_str(), why)
            .await
            .with_context(|| format!("close run {run_id} in the run store"))?;
    }
    let closed_invocations = store.close_abandoned_invocations(why).await?;
    Ok(TakenOver {
        imported_legacy,
        closed_run,
        closed_invocations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CurrentRun, DagState};
    use crate::supervisor::record::InvocationRow;

    #[tokio::test]
    async fn what_a_dead_loop_left_open_is_closed_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let open = DagState {
            current_run: Some(CurrentRun {
                run_id: "r1".into(),
                started_at: "2026-09-23T10:00:00+00:00".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        store
            .save_record(&DagState::default(), &open)
            .await
            .unwrap();
        store
            .open_invocation(&InvocationRow {
                id: "i1".into(),
                step: "a/raw".into(),
                run_id: "r1".into(),
                started_at_utc: "2026-09-23T10:00:01+00:00".into(),
            })
            .await
            .unwrap();

        let taken = take_over(&store, root.path()).await.unwrap();
        assert_eq!(
            taken,
            TakenOver {
                imported_legacy: false,
                closed_run: Some("r1".into()),
                closed_invocations: 1
            }
        );
        let after = store.load_record().await.unwrap();
        assert!(after.current_run.unwrap().finished_at.is_some());
        assert!(store.running_invocations().await.unwrap().is_empty());
        assert_eq!(
            take_over(&store, root.path()).await.unwrap(),
            TakenOver::default()
        );
    }
}
