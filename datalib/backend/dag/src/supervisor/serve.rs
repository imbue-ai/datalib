//! The loop held for the whole life of the process that runs it,
//! `datalib-pipeline serve`, which the app's server starts as a child. It
//! takes `runner-lock` (waiting while another process holds
//! it), closes what a dead loop left, then runs the loop, whose prepare
//! pass goes first, until told to stop. What a parent needs to
//! hear and the stores do not hold, it says as [`ServeEvent`]s.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::config::ConfigCheck;
use crate::lock::RunnerLock;
use crate::scheduler::Runner;
use crate::supervisor::announce::Listener;
use crate::supervisor::host;
use crate::supervisor::loop_host::{LoopHost, PeriodSink};
use crate::supervisor::reload::ConfigFile;
use crate::supervisor::store::{RequestOutcome, Store};

/// What the process running the loop needs beyond the data root.
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    pub root: PathBuf,
    /// The step binaries' directory, ahead of the config's `binary_dir`.
    pub binary_dir: Option<PathBuf>,
    /// Every run's "now", in place of the clock. For fixtures, whose
    /// output must not depend on the day they were built.
    pub now: Option<String>,
    /// At most this many steps at once; the runner's default when `None`.
    pub parallelism: Option<usize>,
}

/// One thing the loop says to whoever started it, one NDJSON line each
/// on `datalib-pipeline serve`'s stderr. Only what no store holds: everything
/// else a parent reads from `system/supervisor.sqlite` and the run store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServeEvent {
    /// This process holds `runner-lock`: the loop is its from here on.
    Holding,
    /// A run began: the loop is busy until [`ServeEvent::SyncOver`].
    SyncStarted {
        run_id: String,
    },
    SyncOver {
        run_id: String,
    },
    /// This process wrote to the run store, so a reader showing it should
    /// read it again. A filesystem watch would say so too, but late.
    RunStoreWritten,
}

/// Where [`run`] sends what it says.
pub type Emit = Arc<dyn Fn(ServeEvent) + Send + Sync>;

/// Run the loop on `opts.root` until `stop` turns true. Returns at once
/// when the request store cannot be opened or the lock cannot be taken
/// for any reason but another holder, having logged why.
pub async fn run(opts: &ServeOptions, emit: Emit, mut stop: watch::Receiver<bool>) {
    let root = opts.root.as_path();
    let store = match Store::open(root).await {
        Ok(store) => store,
        Err(e) => {
            tracing::error!(
                "supervisor: cannot open the request store, so nothing will sync: {e:#}"
            );
            return;
        }
    };
    let mut listener = Listener::new(&store, "the loop");
    let Some(_lock) = take_the_lock(root, &mut listener, &mut stop).await else {
        store.close().await;
        return;
    };
    emit(ServeEvent::Holding);
    match host::take_over(&store, root).await {
        Ok(taken) => {
            if let Some(run) = &taken.closed_run {
                tracing::warn!(
                    run,
                    invocations = taken.closed_invocations,
                    "supervisor: closed a run a dead loop left open"
                );
            }
        }
        Err(e) => tracing::error!("supervisor: could not take over from the last loop: {e:#}"),
    }
    tracing::info!("supervisor: running the loop on {}", root.display());
    let periods = Periods {
        opts,
        emit: emit.clone(),
        record: Arc::new(PeriodSink::default()),
        open: Mutex::new(None),
    };
    while !*stop.borrow() {
        let failed = match load_config(root) {
            Ok(checked) => run_the_loop(opts, &checked, &store, &periods, &stop).await,
            Err(why) => Err(why),
        };
        let Err(why) = failed else { break };
        // Its requests would otherwise be taken on at once, and fail the
        // same way; the next change to the store or the config is the
        // next try.
        periods.close_any();
        fail_open_requests(&store, &why).await;
        if let Err(e) = host::take_over(&store, root).await {
            tracing::error!("supervisor: could not close the failed run: {e:#}");
        }
        tokio::select! {
            _ = listener.next() => {}
            _ = stop.changed() => {}
        }
    }
    store.close().await;
}

/// The loop on `checked`'s graph, until `stop` or an error it cannot go
/// on from. It re-reads the config as it goes.
async fn run_the_loop(
    opts: &ServeOptions,
    checked: &ConfigCheck,
    store: &Store,
    periods: &Periods<'_>,
    stop: &watch::Receiver<bool>,
) -> Result<(), String> {
    let root = opts.root.as_path();
    let mut runner = Runner::new(root)
        .sink(periods.record.clone())
        .stop_on(stop.clone())
        .reload_from(Arc::new(ConfigFile::new(crate::config::root_config_path(
            root,
        ))));
    if let Some(n) = opts.parallelism {
        runner = runner.parallelism(n);
    }
    runner
        .run_forever(&checked.graph, store, periods)
        .await
        .map_err(|e| {
            tracing::error!("supervisor: the loop failed: {e:#}");
            format!("the sync failed: {e:#}")
        })
}

/// The config as the loop would run it, or why it cannot.
pub fn load_config(root: &Path) -> Result<ConfigCheck, String> {
    let path = crate::config::root_config_path(root);
    if !path.is_file() {
        return Err(format!(
            "no config at {} — create one from the Setup tab before syncing",
            path.display()
        ));
    }
    let (checked, _) = crate::config::load_graded(&path).map_err(|e| format!("{e:#}"))?;
    if checked.is_fatal() {
        return Err(format!(
            "{} is not a config: nothing in it could be read\n{}",
            path.display(),
            checked.render(&path)
        ));
    }
    Ok(checked)
}

/// The lock, once whoever holds it lets go. Until then that process runs
/// the loop, and serves every request written to the store.
async fn take_the_lock(
    root: &Path,
    listener: &mut Listener,
    stop: &mut watch::Receiver<bool>,
) -> Option<RunnerLock> {
    let mut announced = false;
    loop {
        match crate::lock::try_acquire_runner(root) {
            Ok(lock) => return Some(lock),
            Err(e) if e.is_held() => {
                if !announced {
                    announced = true;
                    tracing::warn!(
                        "supervisor: another process runs the loop on this root{}; \
                         taking over when it is done",
                        e.holder().map(|h| format!(" ({h})")).unwrap_or_default()
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    "supervisor: cannot take the runner lock, so nothing will sync: {e}"
                );
                return None;
            }
        }
        tokio::select! {
            _ = listener.next() => {}
            _ = stop.changed() => return None,
        }
    }
}

/// Who opens the request that runs what the prepare pass could not bring
/// up to date in place.
pub const UPGRADE_REQUESTER: &str = "upgrade";

/// The environment every step of one run gets.
fn steps_env(
    opts: &ServeOptions,
    cfg: &crate::config::DagConfig,
    now: &str,
    run_id: &str,
) -> anyhow::Result<host::StepEnv> {
    host::step_env(
        cfg,
        opts.binary_dir.as_deref(),
        &extra_path(&opts.root),
        now,
        run_id,
    )
}

/// Where the loop's steps find the library's own programs, whether or not
/// the directory exists yet: an agent may make it between runs, and a
/// missing entry costs nothing.
fn extra_path(root: &Path) -> Vec<PathBuf> {
    vec![root.join(datalib_runtime::layout::BIN_DIR_REL)]
}

fn now(opts: &ServeOptions) -> String {
    opts.now
        .clone()
        .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs())
}

/// Each busy period recorded in the run store, and said to the parent.
struct Periods<'a> {
    opts: &'a ServeOptions,
    emit: Emit,
    /// The runner's sink: the open period's record, or nothing.
    record: Arc<PeriodSink>,
    /// The run id of the period open now.
    open: Mutex<Option<String>>,
}

impl Periods<'_> {
    /// A loop that failed may have left its period open.
    fn close_any(&self) {
        let open = self.open.lock().expect("the open period's lock").clone();
        if let Some(run_id) = open {
            self.close(&run_id);
        }
    }
}

impl LoopHost for Periods<'_> {
    fn open(&self, start: datalib_runs::RunStart) -> Result<BTreeMap<String, String>, String> {
        let root = self.opts.root.as_path();
        let checked = load_config(root)?;
        let run_id = crate::scheduler::new_run_id();
        let now = now(self.opts);
        let env =
            steps_env(self.opts, &checked.cfg, &now, &run_id).map_err(|e| format!("{e:#}"))?;
        match host::start_record(root, &checked.cfg, &run_id, &now, start) {
            Some(record) => drop(self.record.point_at(Some(Arc::new(record)))),
            None => tracing::warn!(
                run = %run_id,
                "supervisor: run store unavailable; nothing recorded this run"
            ),
        }
        *self.open.lock().expect("the open period's lock") = Some(run_id.clone());
        (self.emit)(ServeEvent::SyncStarted {
            run_id: run_id.clone(),
        });
        tracing::info!(run = %run_id, "supervisor: a sync started");
        Ok(env.vars)
    }

    /// The run store's record closes when its sink is dropped, here:
    /// before the loop says the sync is over, so no one reads "over"
    /// before it is.
    fn close(&self, run_id: &str) {
        drop(self.record.point_at(None));
        *self.open.lock().expect("the open period's lock") = None;
        (self.emit)(ServeEvent::SyncOver {
            run_id: run_id.to_string(),
        });
        tracing::info!(run = %run_id, "supervisor: the sync is over");
    }
}

/// Close every open request: the loop cannot run them, for `why`.
async fn fail_open_requests(store: &Store, why: &str) {
    tracing::warn!("supervisor: cannot sync: {why}");
    for request in store.open_requests().await.unwrap_or_default() {
        let outcome = match request.stop_requested_by {
            Some(_) => RequestOutcome::Stopped,
            None => RequestOutcome::Failed,
        };
        let _ = store.close_request(&request.id, outcome, None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parent reads these off a pipe, so the words are a protocol: each
    /// line names its event in snake case, and reads back as it was sent.
    #[test]
    fn a_serve_event_reads_back_as_it_was_written() {
        let events = [
            ServeEvent::Holding,
            ServeEvent::SyncStarted { run_id: "r".into() },
            ServeEvent::SyncOver { run_id: "r".into() },
            ServeEvent::RunStoreWritten,
        ];
        for event in events {
            let line = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<ServeEvent>(&line).unwrap(), event);
        }
        assert_eq!(
            serde_json::to_string(&ServeEvent::RunStoreWritten).unwrap(),
            r#"{"event":"run_store_written"}"#
        );
    }
}
