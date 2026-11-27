//! What a process running the loop builds around it, the same whichever
//! process that is (`datalib-pipeline`, or the app's server): the steps'
//! environment and the run store's record of one busy period, and what it
//! puts right when it takes the lock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_runs::{RunKind, RunStart};

use crate::config::DagConfig;
use crate::run_state::RunState;
use crate::runs_sink::RunStoreSink;
use crate::supervisor::store::{JobKind, JobRow, RequestRow, Store};
use datalib_step_protocol::env::{DATALIB_DAG_NOW, DATALIB_DAG_RUN_ID};
use strum::VariantArray;

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
    // So a `command` can name a program in the binary directory bare.
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
    vars.insert(DATALIB_DAG_RUN_ID.into(), run_id.to_string());
    vars.insert(DATALIB_DAG_NOW.into(), now.to_string());
    // A step's stdout is a pipe, and Python block-buffers a pipe by
    // default: its progress lines would arrive in 4KB lumps, long after
    // the stderr they belong beside. Rust and sh need no help.
    vars.insert("PYTHONUNBUFFERED".into(), "1".into());
    // The config's `[env]`, below each step's own `env`.
    vars.extend(cfg.env.clone());
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
    started: RunStart,
) -> Option<RunStoreSink> {
    let retention = cfg.run_history.map(|h| h.retention()).unwrap_or_default();
    let commit = datalib_runs::git_hash_and_origin().map(|(hash, _)| hash);
    RunStoreSink::start(data_root, run_id, now, commit, retention, Some(started))
}

/// How many steps a run's reason names before it only counts the rest.
const REASON_NAMES: usize = 3;

/// Why a busy period started, from the requests and wipes open as it did.
/// A period with nothing but compactions and collections to do is a
/// shrink; any other is a sync.
pub fn period_start(requests: &[RequestRow], wipes: &[JobRow]) -> RunStart {
    let mut parts = Vec::new();
    if !requests.is_empty() {
        let roots: Vec<&str> = requests
            .iter()
            .flat_map(|r| r.roots.iter().map(String::as_str))
            .collect();
        parts.push(format!(
            "a sync asked for by {}: {}",
            who(requests.iter().map(|r| r.opened_by.as_str())),
            named(&roots)
        ));
    }
    for kind in JobKind::VARIANTS {
        let these: Vec<&JobRow> = wipes.iter().filter(|w| w.kind == Some(*kind)).collect();
        if these.is_empty() {
            continue;
        }
        let by = who(these.iter().map(|w| w.opened_by.as_str()));
        let targets: Vec<&str> = these
            .iter()
            .flat_map(|w| w.targets.iter().map(String::as_str))
            .collect();
        let scope = if targets.is_empty() {
            "every step".to_string()
        } else {
            named(&targets)
        };
        parts.push(match kind {
            JobKind::Collect => format!("freeing disk space, asked for by {by}: {scope}"),
            JobKind::Compact => format!(
                "freeing disk space and dropping old versions of rendered documents, \
                 asked for by {by}: {scope}"
            ),
            JobKind::Reset => format!("a reset asked for by {by}: {scope}"),
            JobKind::Purge => {
                format!("deleting what a removed source left, asked for by {by}: {scope}")
            }
            JobKind::Prepare => PREPARE_REASON.to_string(),
        });
    }
    let only = |kinds: &[JobKind]| {
        requests.is_empty()
            && !wipes.is_empty()
            && wipes
                .iter()
                .all(|w| w.kind.is_some_and(|k| kinds.contains(&k)))
    };
    RunStart {
        kind: if only(&[JobKind::Prepare]) {
            RunKind::Migrate
        } else if only(&[JobKind::Collect, JobKind::Compact]) {
            RunKind::Shrink
        } else {
            RunKind::Sync
        },
        reason: if parts.is_empty() {
            "the loop found work with no sync request open".to_string()
        } else {
            parts.join("; ")
        },
    }
}

fn who<'a>(by: impl Iterator<Item = &'a str>) -> String {
    let mut by: Vec<&str> = by.collect();
    by.sort_unstable();
    by.dedup();
    by.join(", ")
}

/// The first few names, then a count of the rest.
fn named(names: &[&str]) -> String {
    let shown = names
        .iter()
        .take(REASON_NAMES)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    match names.len().saturating_sub(REASON_NAMES) {
        0 => shown,
        rest => format!("{shown} and {rest} more"),
    }
}

/// Why the launch's pass over the steps is running.
const PREPARE_REASON: &str = "the loop started: each step whose program takes --prepare \
     makes what it stored usable, and nothing is downloaded";

/// What [`take_over`] found to put right.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TakenOver {
    /// The run a dead loop left open, which it closed.
    pub closed_run: Option<String>,
    /// How many of that loop's invocations it closed as stopped.
    pub closed_invocations: u64,
}

/// For the process that has just taken `runner-lock`, before its first
/// loop: close what a loop that died holding the lock left open — its
/// run, in the record and in the run store, so no row reads its steps as
/// live, and its invocations. Holding the lock is what makes anything open
/// a thing a dead loop left.
pub async fn take_over(store: &Store, data_root: &Path) -> Result<TakenOver> {
    let why = "the loop running this ended without closing it; the next to take the lock did";
    let saved = store.load_record().await.context("load the record")?;
    let mut state = saved.clone();
    let closed_run = match state.current_run.as_mut() {
        Some(run) if run.finished_at.is_none() => {
            run.finished_at = Some(datalib_time::closed_now(&run.started_at).to_rfc3339_micros());
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
        closed_run,
        closed_invocations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::record::InvocationRow;
    use crate::supervisor::record::{CurrentRun, Record};

    /// Guards reading a run's cause off a server log line and the absence
    /// of a request: a sync names who asked and for what.
    #[test]
    fn a_syncs_reason_names_who_asked_and_for_what() {
        let request = |by: &str, roots: &[&str]| RequestRow {
            id: "r".into(),
            roots: roots.iter().map(|r| r.to_string()).collect(),
            opened_by: by.into(),
            stop_requested_by: None,
            closed: None,
            failed_step: None,
        };
        let one = period_start(&[request("ui", &["gmail/ingest"])], &[]);
        assert_eq!(one.kind, RunKind::Sync);
        assert_eq!(one.reason, "a sync asked for by ui: gmail/ingest");
        let many = period_start(
            &[
                request("ui", &["a/ingest", "b/ingest"]),
                request("upgrade", &["c/render_markdown", "d/render_markdown"]),
                request("ui", &["e/ingest"]),
            ],
            &[],
        );
        assert_eq!(
            many.reason,
            "a sync asked for by ui, upgrade: a/ingest, b/ingest, c/render_markdown and 2 more"
        );
        assert_eq!(
            period_start(&[], &[]).reason,
            "the loop found work with no sync request open"
        );
    }

    /// Guards the free-disk run that was logged as a sync "with no sync
    /// request open": a period that only frees disk says so, and is a
    /// shrink.
    #[test]
    fn a_period_that_only_frees_disk_is_a_shrink_and_says_who_asked() {
        let wipe = |kind: JobKind, targets: &[&str]| JobRow {
            id: "w".into(),
            kind: Some(kind),
            targets: targets.iter().map(|t| t.to_string()).collect(),
            opened_by: "ui".into(),
            closed: None,
        };
        let collect = period_start(&[], &[wipe(JobKind::Collect, &[])]);
        assert_eq!(collect.kind, RunKind::Shrink);
        assert_eq!(
            collect.reason,
            "freeing disk space, asked for by ui: every step"
        );
        let reset = period_start(&[], &[wipe(JobKind::Reset, &["slack/ingest"])]);
        assert_eq!(reset.kind, RunKind::Sync);
        assert_eq!(reset.reason, "a reset asked for by ui: slack/ingest");
    }

    #[tokio::test]
    async fn what_a_dead_loop_left_open_is_closed_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        let open = Record {
            current_run: Some(CurrentRun {
                run_id: "r1".into(),
                started_at: "2026-09-23T10:00:00+00:00".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        store.save_record(&Record::default(), &open).await.unwrap();
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

    /// A config's `[env]` reaches every step; a config without one adds
    /// nothing.
    #[test]
    fn the_configs_env_reaches_every_step() {
        let with = crate::config::parse("[env]\nSOME_KNOB = \"8,256\"\n").unwrap();
        let env = step_env(&with, None, &[], "now", "run").unwrap();
        assert_eq!(env.vars["SOME_KNOB"], "8,256");

        let without = crate::config::parse("").unwrap();
        let env = step_env(&without, None, &[], "now", "run").unwrap();
        assert!(!env.vars.contains_key("SOME_KNOB"));
    }
}
