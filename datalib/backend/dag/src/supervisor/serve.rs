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
use crate::supervisor::loop_host::LoopHost;
use crate::supervisor::reload::ConfigFile;
use crate::supervisor::store::{RequestOutcome, Store};

/// What the process running the loop needs beyond the data root.
#[derive(Clone, Default)]
pub struct ServeOptions {
    pub root: PathBuf,
    /// The step binaries' directory, ahead of the config's `binary_dir`.
    pub binary_dir: Option<PathBuf>,
    /// Every invocation's "now", in place of the time its request was
    /// opened. For fixtures, whose output must not depend on the day they
    /// were built.
    pub now: Option<String>,
    /// At most this many steps at once; the runner's default when `None`.
    pub parallelism: Option<usize>,
    /// Where every event also goes, beside the run store: a
    /// `datalib-pipeline` verb's stderr.
    pub events: Option<Arc<dyn crate::events::EventSink>>,
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
    /// The loop has taken on work: it is busy until [`ServeEvent::Idle`].
    Busy,
    /// Nothing is open and nothing runs.
    Idle,
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
/// the loop. The caller holds `runner-lock`. The last busy period's
/// report, or why the loop could not go on.
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
/// then run the loop. One held until `stop` starts again at the next
/// change to the store or the config when it fails; one held until idle
/// says why and returns.
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
        Ok(taken) if taken != host::TakenOver::default() => tracing::warn!(
            invocations = taken.closed_invocations,
            in_history = taken.closed_in_history,
            "supervisor: closed the invocations a dead loop left open"
        ),
        Ok(_) => {}
        Err(e) => tracing::error!("supervisor: could not take over from the last loop: {e:#}"),
    }
    tracing::info!("supervisor: running the loop on {}", root.display());
    // One record for as long as this process holds the loop; it closes
    // when it is dropped, at the end of this function.
    let record = record_of(opts);
    let host = Host {
        opts,
        emit,
        last_env: Mutex::new(None),
        record: record.clone(),
    };
    let mut said_no_pass = false;
    loop {
        let ran = match load_config_at(&opts.root, &opts.config_path()) {
            Ok(checked) => {
                run_the_loop(opts, &checked, store, &host, record.clone(), stop, until).await
            }
            Err(why) => {
                // With no config there is no step to ask: the pass is over,
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
        // Its requests would otherwise be taken on at once, and fail the
        // same way; the next change to the store or the config is the
        // next try.
        host.busy(false);
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

/// A prepare pass with nothing to ask, open and closed at once.
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

/// The run store's record of what the loop's invocations do, for as long
/// as this process runs the loop. `None` when the store could not be
/// opened: a sync without a record beats one that refuses to start over
/// an unwritable status file.
fn record_of(opts: &ServeOptions) -> Option<Arc<crate::runs_sink::RunStoreSink>> {
    let retention = load_config_at(&opts.root, &opts.config_path())
        .ok()
        .and_then(|c| c.cfg.run_history)
        .map(|h| h.retention())
        .unwrap_or_default();
    let commit = datalib_runs::git_hash_and_origin().map(|(hash, _)| hash);
    let record = crate::runs_sink::RunStoreSink::start(&opts.root, commit, retention);
    if record.is_none() {
        tracing::warn!("supervisor: run store unavailable; nothing will be recorded");
    }
    record.map(Arc::new)
}

/// The loop on `checked`'s graph, until `stop` or an error it cannot go
/// on from. It re-reads the config as it goes.
async fn run_the_loop(
    opts: &ServeOptions,
    checked: &ConfigCheck,
    store: &Store,
    host: &Host<'_>,
    record: Option<Arc<crate::runs_sink::RunStoreSink>>,
    stop: &watch::Receiver<bool>,
    until: Until,
) -> Result<Option<crate::scheduler::RunReport>, String> {
    let root = opts.root.as_path();
    let sinks: Vec<Arc<dyn crate::events::EventSink>> = opts
        .events
        .iter()
        .cloned()
        .chain(record.map(|r| r as Arc<dyn crate::events::EventSink>))
        .collect();
    let mut runner = Runner::new(root)
        .sink(Arc::new(crate::events::FanOutSink(sinks)))
        .stop_on(stop.clone())
        .pin_now(opts.now.clone())
        .reload_from(Arc::new(ConfigFile::new(opts.config_path())));
    if let Some(n) = opts.parallelism {
        runner = runner.parallelism(n);
    }
    let ran = match until {
        Until::Stopped => runner
            .run_forever(&checked.graph, store, host)
            .await
            .map(|()| None),
        Until::Idle => runner.run_until_idle(&checked.graph, store, host).await,
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

/// The environment a step started now gets, from `cfg`.
fn steps_env(opts: &ServeOptions, cfg: &crate::config::DagConfig) -> anyhow::Result<host::StepEnv> {
    host::step_env(cfg, opts.binary_dir.as_deref(), &extra_path(&opts.root))
}

/// Where the loop's steps find the library's own programs, whether or not
/// the directory exists yet: an agent may make it between runs, and a
/// missing entry costs nothing.
fn extra_path(root: &Path) -> Vec<PathBuf> {
    vec![root.join(datalib_runtime::layout::BIN_DIR_REL)]
}

/// What the loop asks of this process, and what it says to the parent.
struct Host<'a> {
    opts: &'a ServeOptions,
    emit: Emit,
    /// The environment built from the last config that loaded, for while
    /// the config cannot be read.
    last_env: Mutex<Option<BTreeMap<String, String>>>,
    record: Option<Arc<crate::runs_sink::RunStoreSink>>,
}

impl LoopHost for Host<'_> {
    fn step_env(&self) -> Result<BTreeMap<String, String>, String> {
        let built = load_config_at(&self.opts.root, &self.opts.config_path()).and_then(|checked| {
            steps_env(self.opts, &checked.cfg)
                .map(|env| env.vars)
                .map_err(|e| format!("{e:#}"))
        });
        let mut last = self.last_env.lock().expect("the last environment's lock");
        match built {
            Ok(env) => {
                *last = Some(env.clone());
                Ok(env)
            }
            Err(why) => last.clone().ok_or(why),
        }
    }

    fn busy(&self, busy: bool) {
        if busy {
            (self.emit)(ServeEvent::Busy);
            tracing::info!("supervisor: the loop took on work");
        } else {
            // Written before it is said: a reader told the loop is idle
            // reads how every invocation ended.
            if let Some(record) = &self.record {
                record.flush_blocking();
            }
            (self.emit)(ServeEvent::Idle);
            tracing::info!("supervisor: nothing is open and nothing runs");
            say_runner_statements();
        }
    }
}

/// What the runner's own statements did while the loop was busy, by name,
/// as one line: the runner never ends, so its work is reported a stretch
/// of work at a time rather than as a step's metrics are.
fn say_runner_statements() {
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
        statements = %statements,
        "the runner's statements since the loop was last idle: {} names",
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
            ServeEvent::Busy,
            ServeEvent::Idle,
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
