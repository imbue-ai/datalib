//! What a process running the loop builds around it, the same whichever
//! process that is (`datalib-pipeline`, or the app's server): the steps'
//! environment, the sentence that says what a request or a job is for,
//! and what it puts right when it takes the lock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::DagConfig;
use crate::run_state::RunState;
use crate::supervisor::store::{JobKind, JobRow, RequestRow, Store};

/// The environment every step gets on top of the host's own, and the log
/// filter it was built with. Each invocation's id and "now" go on top.
pub struct StepEnv {
    pub vars: BTreeMap<String, String>,
    pub log_filter: String,
}

/// `binary_dir` is the host's override of the config's `binary_dir`;
/// `extra_path` goes on `PATH` after it, ahead of the host's own `PATH`.
pub fn step_env(
    cfg: &DagConfig,
    binary_dir: Option<&Path>,
    extra_path: &[PathBuf],
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

/// How many steps a reason names before it only counts the rest.
const REASON_NAMES: usize = 3;

/// What a request is for, as a sentence: who asked, and for which steps.
pub fn request_reason(request: &RequestRow) -> String {
    let roots: Vec<&str> = request.roots.iter().map(String::as_str).collect();
    format!(
        "a sync asked for by {}: {}",
        request.opened_by,
        named(&roots)
    )
}

/// What a job is for, as a sentence: what it does, who asked, and for
/// which steps or groups.
pub fn job_reason(job: &JobRow) -> String {
    let by = &job.opened_by;
    let targets: Vec<&str> = job.targets.iter().map(String::as_str).collect();
    let scope = if targets.is_empty() {
        "every step".to_string()
    } else {
        named(&targets)
    };
    match job.kind {
        Some(JobKind::Collect) => format!("freeing disk space, asked for by {by}: {scope}"),
        Some(JobKind::Compact) => format!(
            "freeing disk space and dropping old versions of rendered documents, \
             asked for by {by}: {scope}"
        ),
        Some(JobKind::Reset) => format!("a reset asked for by {by}: {scope}"),
        Some(JobKind::Purge) => {
            format!("deleting what a removed source left, asked for by {by}: {scope}")
        }
        Some(JobKind::Prepare) => PREPARE_REASON.to_string(),
        None => format!("a kind of job this build does not know, asked for by {by}"),
    }
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
    /// How many of that loop's invocations it closed as stopped, in the
    /// record.
    pub closed_invocations: u64,
    /// How many the run store still read as running, which it closed too.
    pub closed_in_history: u64,
}

/// For the process that has just taken `runner-lock`, before its first
/// loop: close the invocations a loop that died holding the lock left
/// open, in the record and in the run store, so no row reads its steps as
/// live. Holding the lock is what makes anything open a thing a dead loop
/// left.
pub async fn take_over(store: &Store, data_root: &Path) -> Result<TakenOver> {
    let why = "the loop running this ended without closing it; the next to take the lock did";
    let closed_invocations = store.close_abandoned_invocations(why).await?;
    let closed_in_history =
        datalib_runs::close_abandoned_invocations(data_root, RunState::Stopped.as_str(), why)
            .await
            .context("close the run store's open invocations")?;
    Ok(TakenOver {
        closed_invocations,
        closed_in_history,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::record::InvocationRow;

    fn request(by: &str, roots: &[&str]) -> RequestRow {
        RequestRow {
            id: "r".into(),
            roots: roots.iter().map(|r| r.to_string()).collect(),
            opened_by: by.into(),
            opened_at: "2026-09-23T10:00:00+00:00".into(),
            closed_at: None,
            stop_requested_by: None,
            closed: None,
            failed_step: None,
        }
    }

    fn job(kind: JobKind, targets: &[&str]) -> JobRow {
        JobRow {
            id: "w".into(),
            kind: Some(kind),
            targets: targets.iter().map(|t| t.to_string()).collect(),
            opened_by: "ui".into(),
            opened_at: "2026-09-23T10:00:00+00:00".into(),
            closed_at: None,
            closed: None,
        }
    }

    /// Guards the line a Runs list shows for a request: who asked, and for
    /// what, the first few steps named.
    #[test]
    fn a_requests_reason_names_who_asked_and_for_what() {
        assert_eq!(
            request_reason(&request("ui", &["gmail/ingest"])),
            "a sync asked for by ui: gmail/ingest"
        );
        assert_eq!(
            request_reason(&request(
                "upgrade",
                &[
                    "a/ingest",
                    "b/ingest",
                    "c/render_markdown",
                    "d/render_markdown"
                ]
            )),
            "a sync asked for by upgrade: a/ingest, b/ingest, c/render_markdown and 1 more"
        );
    }

    /// Guards the free-disk run that was logged as a sync "with no sync
    /// request open": a job says what it does and who asked.
    #[test]
    fn a_jobs_reason_says_what_it_does_and_who_asked() {
        assert_eq!(
            job_reason(&job(JobKind::Collect, &[])),
            "freeing disk space, asked for by ui: every step"
        );
        assert_eq!(
            job_reason(&job(JobKind::Reset, &["slack/ingest"])),
            "a reset asked for by ui: slack/ingest"
        );
    }

    #[tokio::test]
    async fn what_a_dead_loop_left_open_is_closed_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).await.unwrap();
        store
            .open_invocation(&InvocationRow {
                id: "i1".into(),
                step: "a/raw".into(),
                started_at_utc: "2026-09-23T10:00:01+00:00".into(),
            })
            .await
            .unwrap();

        let taken = take_over(&store, root.path()).await.unwrap();
        assert_eq!(taken.closed_invocations, 1);
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
        let env = step_env(&with, None, &[]).unwrap();
        assert_eq!(env.vars["SOME_KNOB"], "8,256");

        let without = crate::config::parse("").unwrap();
        let env = step_env(&without, None, &[]).unwrap();
        assert!(!env.vars.contains_key("SOME_KNOB"));
    }
}
