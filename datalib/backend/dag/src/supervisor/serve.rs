//! The loop that runs for the whole life of the process that runs it,
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
use crate::supervisor::loop_host::{LoopHost, OpenRunSink};
use crate::supervisor::reload::ConfigFile;
use crate::supervisor::store::{RequestOutcome, Store};

/// What the process running the loop needs beyond the data root.
#[derive(Clone, Default)]
pub struct ServeOptions {
    pub root: PathBuf,
    /// The step binaries' directory, ahead of the config's `binary_dir`.
    pub binary_dir: Option<PathBuf>,
    /// Every run's "now", in place of the clock. For fixtures, whose
    /// output must not depend on the day they were built.
    pub now: Option<String>,
    /// At most this many steps at once; the runner's default when `None`.
    pub parallelism: Option<usize>,
    /// Where every event also goes, beside the open run's sink in the run
    /// store: a `datalib-pipeline` verb's stderr.
    pub events: Option<Arc<dyn crate::events::EventSink>>,
    /// The first run's id, in place of a fresh one. For a
    /// fixture that finds its run by id.
    pub run_id: Option<String>,
    /// The config file, when it is not the root's own `config.toml`: a
    /// `datalib-pipeline` verb may be given one that names its root.
    pub config: Option<PathBuf>,
}

impl ServeOptions {
    fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(|| crate::config::root_config_path(&self.root))
    }
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
    let _ = hold(opts, emit, &store, &mut listener, &mut stop, Until::Stopped).await;
    store.close().await;
}

/// The loop for as long as anything is open, for a `datalib-pipeline`
/// verb that wrote its request or job and found no other process running
/// the loop. The caller holds `runner-lock`. The last run's report, or
/// why the loop could not go on.
pub async fn run_until_idle(
    opts: &ServeOptions,
    store: &Store,
    mut stop: watch::Receiver<bool>,
) -> Result<Option<crate::scheduler::RunReport>, String> {
    let mut listener = Listener::new(store, "the loop");
    let emit: Emit = Arc::new(|_| {});
    hold(opts, emit, store, &mut listener, &mut stop, Until::Idle).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Until {
    Stopped,
    Idle,
}

/// What a process holding the lock does: close what a dead loop left,
/// then run the loop. Run until `stop`, a loop that fails starts again at
/// the next change to the store or the config; run until idle, it says
/// why and returns.
async fn hold(
    opts: &ServeOptions,
    emit: Emit,
    store: &Store,
    listener: &mut Listener,
    stop: &mut watch::Receiver<bool>,
    until: Until,
) -> Result<Option<crate::scheduler::RunReport>, String> {
    let root = opts.root.as_path();
    match host::take_over(store, root).await {
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
    let run_host = RunHost {
        opts,
        emit,
        run_sink: Arc::new(OpenRunSink::default()),
        open: Mutex::new(None),
        first_run_id: Mutex::new(opts.run_id.clone()),
    };
    let mut said_no_pass = false;
    loop {
        let ran = match load_config_at(&opts.root, &opts.config_path()) {
            Ok(checked) => run_the_loop(opts, &checked, store, &run_host, stop, until).await,
            Err(why) => {
                // With no config there is no step to run `--prepare` for: the pass is over,
                // and a root waiting for its first config is not waiting
                // on it. A closed job with no steps says so.
                if !std::mem::replace(&mut said_no_pass, true) {
                    if let Err(e) = no_pass(store).await {
                        tracing::error!("supervisor: could not record the empty pass: {e:#}");
                    }
                }
                Err(why)
            }
        };
        let why = match ran {
            Ok(report) => return Ok(report),
            Err(why) => why,
        };
        // Its requests would otherwise be served again at once, and fail the
        // same way; the next change to the store or the config is the
        // next try.
        run_host.close_any();
        fail_open_requests(store, &why).await;
        if let Err(e) = host::take_over(store, root).await {
            tracing::error!("supervisor: could not close the failed run: {e:#}");
        }
        if until == Until::Idle {
            return Err(why);
        }
        tokio::select! {
            _ = listener.next() => {}
            _ = stop.changed() => {}
        }
        if *stop.borrow() {
            return Ok(None);
        }
    }
}

/// A prepare pass with no step to run `--prepare` for, open and closed at
/// once.
async fn no_pass(store: &Store) -> anyhow::Result<()> {
    let job = store
        .open_job(
            crate::supervisor::store::JobKind::Prepare,
            &[],
            crate::supervisor::round::LOOP_OPENER,
        )
        .await?;
    store.close_job(&job, None).await
}

/// The loop on `checked`'s graph, until `stop` or an error it cannot go
/// on from. It re-reads the config as it goes.
async fn run_the_loop(
    opts: &ServeOptions,
    checked: &ConfigCheck,
    store: &Store,
    run_host: &RunHost<'_>,
    stop: &watch::Receiver<bool>,
    until: Until,
) -> Result<Option<crate::scheduler::RunReport>, String> {
    let root = opts.root.as_path();
    let record: Arc<dyn crate::events::EventSink> = run_host.run_sink.clone();
    let sink: Arc<dyn crate::events::EventSink> = match &opts.events {
        Some(events) => Arc::new(crate::events::FanOutSink(vec![events.clone(), record])),
        None => record,
    };
    let mut runner = Runner::new(root)
        .sink(sink)
        .stop_on(stop.clone())
        .reload_from(Arc::new(ConfigFile::new(opts.config_path())));
    if let Some(n) = opts.parallelism {
        runner = runner.parallelism(n);
    }
    let ran = match until {
        Until::Stopped => runner
            .run_forever(&checked.graph, store, run_host)
            .await
            .map(|()| None),
        Until::Idle => runner.run_until_idle(&checked.graph, store, run_host).await,
    };
    ran.map_err(|e| {
        tracing::error!("supervisor: the loop failed: {e:#}");
        format!("the sync failed: {e:#}")
    })
}

/// The config as the loop would run it, or why it cannot.
pub fn load_config(root: &Path) -> Result<ConfigCheck, String> {
    load_config_at(root, &crate::config::root_config_path(root))
}

/// The config at `path`, for the library at `root`, as the loop would run it, or why it cannot.
fn load_config_at(root: &Path, path: &Path) -> Result<ConfigCheck, String> {
    if !path.is_file() {
        return Err(format!(
            "no config at {} — create one before syncing: in the app's Setup tab, or from \
             a shell with `datalib-pipeline init {}`, then add a source to it",
            path.display(),
            root.display()
        ));
    }
    let (checked, _) = crate::config::load_graded(path).map_err(|e| format!("{e:#}"))?;
    if checked.is_fatal() {
        return Err(format!(
            "{} is not a config: nothing in it could be read\n{}",
            path.display(),
            checked.render(path)
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
    host::step_env(cfg, opts.binary_dir.as_deref(), &opts.root, now, run_id)
}

fn now(opts: &ServeOptions) -> String {
    opts.now
        .clone()
        .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs())
}

/// Each run recorded in the run store, and said to the parent.
struct RunHost<'a> {
    opts: &'a ServeOptions,
    emit: Emit,
    /// The runner's sink: the open run's sink in the run store, or
    /// nothing.
    run_sink: Arc<OpenRunSink>,
    /// The id of the run open now.
    open: Mutex<Option<String>>,
    /// [`ServeOptions::run_id`], until the first run takes it.
    first_run_id: Mutex<Option<String>>,
}

impl RunHost<'_> {
    /// A loop that failed may have left its run open.
    fn close_any(&self) {
        let open = self.open.lock().expect("the open period's lock").clone();
        if let Some(run_id) = open {
            self.close(&run_id);
        }
    }
}

impl LoopHost for RunHost<'_> {
    fn open(&self, start: datalib_runs::RunStart) -> Result<BTreeMap<String, String>, String> {
        let root = self.opts.root.as_path();
        let checked = load_config_at(&self.opts.root, &self.opts.config_path())?;
        let run_id = self
            .first_run_id
            .lock()
            .expect("the first run id's lock")
            .take()
            .unwrap_or_else(crate::scheduler::new_run_id);
        let now = now(self.opts);
        let env =
            steps_env(self.opts, &checked.cfg, &now, &run_id).map_err(|e| format!("{e:#}"))?;
        match host::start_record(root, &checked.cfg, &run_id, &now, start) {
            Some(record) => drop(self.run_sink.point_at(Some(Arc::new(record)))),
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

    /// The run store closes the run when its sink is dropped, here:
    /// before the loop says the sync is over, so no one reads "over"
    /// before it is.
    fn close(&self, run_id: &str) {
        drop(self.run_sink.point_at(None));
        *self.open.lock().expect("the open period's lock") = None;
        (self.emit)(ServeEvent::SyncOver {
            run_id: run_id.to_string(),
        });
        tracing::info!(run = %run_id, "supervisor: the sync is over");
        say_runner_statements(run_id);
    }
}

/// What the runner's own statements did during the sync that just ended,
/// by name, as one line: the runner never ends, so its work is reported
/// a sync at a time rather than as a step's metrics are.
fn say_runner_statements(run_id: &str) {
    static LAST: std::sync::Mutex<datalib_sql_work::ByTag> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    let now = datalib_sql_work::totals();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let ran = datalib_sql_work::since(&now, &last);
    *last = now;
    if ran.is_empty() {
        return;
    }
    let statements = datalib_sql_work::header_value(&ran);
    tracing::debug!(
        target: datalib_sql_work::TARGET,
        run = %run_id,
        statements = %statements,
        "the runner's statements this sync: {} names",
        ran.len()
    );
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
