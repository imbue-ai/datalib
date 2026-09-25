//! What a process running the loop builds around it, the same whichever
//! process that is (`datalib-dag`, or the app's server): the steps'
//! environment and the run store's record of one busy period, what it
//! puts right when it takes the lock, and the loop's idle side.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::sync::watch;

use crate::config::DagConfig;
use crate::run_state::RunState;
use crate::runs_sink::RunStoreSink;
use crate::subprocess::{ENV_CHECKPOINT_CADENCE, ENV_NOW, ENV_RUN_ID};
use crate::supervisor::announce::Listener;
use crate::supervisor::store::{RequestOutcome, Store};

/// What an idle host does at each turn of [`run_idle`].
pub trait Periods {
    /// Serve the open requests until none is left.
    fn busy_period(&mut self, store: &Store) -> impl Future<Output = ()> + Send;
    /// The pauses moved while nothing ran: bring the record to them.
    fn settle(&mut self, store: &Store) -> impl Future<Output = ()> + Send;
    /// What only an idle loop may do, such as a reset.
    fn idle_work(&mut self, store: &Store) -> impl Future<Output = ()> + Send;
    /// Resolves when something in this process wants a look that no
    /// announcement carries.
    fn nudged(&self) -> impl Future<Output = ()> + Send;
}

/// The loop between busy periods, for a host holding `runner-lock`: a busy
/// period whenever a request is open, requests asked to stop before any
/// period took them closed where they stand, the record settled when the
/// pauses move, and otherwise a wait for an announcement, a nudge or
/// `stop`. `listener` is made before the first look, so nothing announced
/// after it is missed.
pub async fn run_idle(
    store: &Store,
    listener: &mut Listener,
    periods: &mut impl Periods,
    stop: &mut watch::Receiver<bool>,
) {
    // The pauses the record was last settled against; `None` until the
    // first settle, which also clears what a dead loop left running.
    let mut settled: Option<BTreeMap<String, String>> = None;
    while !*stop.borrow() {
        periods.idle_work(store).await;
        let open = match store.open_requests().await {
            Ok(open) => open,
            Err(e) => {
                tracing::error!("supervisor: could not read the open requests: {e:#}");
                Vec::new()
            }
        };
        if open.iter().any(|r| r.stop_requested_by.is_none()) {
            periods.busy_period(store).await;
            continue;
        }
        for request in open {
            if let Err(e) = store
                .close_request(&request.id, RequestOutcome::Stopped, None)
                .await
            {
                tracing::error!(request = %request.id, "supervisor: could not close it: {e:#}");
            }
        }
        match store.paused().await {
            Ok(paused) if settled.as_ref() != Some(&paused) => {
                periods.settle(store).await;
                settled = Some(paused);
            }
            Ok(_) => {}
            Err(e) => tracing::error!("supervisor: could not read the pauses: {e:#}"),
        }
        tokio::select! {
            _ = listener.next(store) => {}
            () = periods.nudged() => {}
            _ = stop.changed() => {}
        }
    }
}

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
        closed_run,
        closed_invocations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::record::InvocationRow;
    use crate::supervisor::record::{CurrentRun, Record};

    /// Each call `run_idle` makes, in order, bar the idle turns: those
    /// come once per wake, and how many wakes a burst of announcements
    /// makes is not the host's to promise. A busy period holds until
    /// released, then closes what it was asked to serve.
    struct Fake {
        calls: tokio::sync::mpsc::UnboundedSender<&'static str>,
        release: std::sync::Arc<tokio::sync::Notify>,
        nudge: std::sync::Arc<tokio::sync::Notify>,
        /// Set by the test beside a nudge: the in-memory work a nudge is for.
        queued: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Periods for Fake {
        async fn busy_period(&mut self, store: &Store) {
            let _ = self.calls.send("busy");
            self.release.notified().await;
            for r in store.open_requests().await.unwrap() {
                if r.stop_requested_by.is_none() {
                    store
                        .close_request(&r.id, RequestOutcome::Done, None)
                        .await
                        .unwrap();
                }
            }
        }
        async fn settle(&mut self, _: &Store) {
            let _ = self.calls.send("settle");
        }
        async fn idle_work(&mut self, _: &Store) {
            if self.queued.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let _ = self.calls.send("work");
            }
        }
        async fn nudged(&self) {
            self.nudge.notified().await;
        }
    }

    /// The idle side, woken only by announcements and nudges (its backstop
    /// is an hour): a request starts a busy period, one asked to stop
    /// before any period took it is closed without one, a pause settles
    /// the record, a nudge runs the work it was for, and a stop ends it.
    #[tokio::test]
    async fn the_idle_host_answers_each_kind_of_wake_and_nothing_else() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(root.path()).await.unwrap());
        let other = Store::open(root.path()).await.unwrap();
        let (tx, mut calls) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let nudge = Arc::new(tokio::sync::Notify::new());
        let queued = Arc::new(AtomicBool::new(false));
        let (stop_tx, mut stop) = watch::channel(false);
        let mut fake = Fake {
            calls: tx,
            release: release.clone(),
            nudge: nudge.clone(),
            queued: queued.clone(),
        };
        let host = {
            let store = store.clone();
            tokio::spawn(async move {
                let mut listener = Listener::new(&store, "test")
                    .await
                    .backstop(std::time::Duration::from_secs(3600));
                run_idle(&store, &mut listener, &mut fake, &mut stop).await;
            })
        };
        let mut next = async |want: &str| {
            let got = tokio::time::timeout(std::time::Duration::from_secs(10), calls.recv())
                .await
                .unwrap_or_else(|_| panic!("no {want} within 10s"));
            assert_eq!(got, Some(want));
        };
        next("settle").await;

        let served = other.open_request(&["a/x".into()], "ui").await.unwrap();
        next("busy").await;
        let stopped = other.open_request(&["b/x".into()], "ui").await.unwrap();
        other.request_stop(&stopped, "ui").await.unwrap();
        release.notify_one();

        // A busy period for the stopped request would come before this.
        other.pause("a/x", "ui").await.unwrap();
        next("settle").await;
        let closed = async |id: &str| other.request(id).await.unwrap().unwrap().closed;
        assert_eq!(closed(&served).await, Some(Some(RequestOutcome::Done)));
        assert_eq!(closed(&stopped).await, Some(Some(RequestOutcome::Stopped)));

        queued.store(true, Ordering::SeqCst);
        nudge.notify_one();
        next("work").await;
        stop_tx.send(true).unwrap();
        host.await.unwrap();
        assert_eq!(calls.recv().await, None, "nothing more after the stop");
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
}
