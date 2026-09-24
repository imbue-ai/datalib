//! The server's side of the supervisor loop (`docs/dev/plans/supervisor.md`
//! §2.8): it holds `runner-lock` for as long as it is up, runs the loop
//! whenever a request is open, and between busy periods settles a pause
//! or a resume into the record and runs a clear.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datalib_dag::config::ConfigCheck;
use datalib_dag::scheduler::ResetTarget;
use datalib_dag::supervisor::host;
use datalib_dag::supervisor::store::{RequestOutcome, Store};
use datalib_dag::{EventSink, Runner};
use tokio::sync::{oneshot, watch, Notify, OnceCell};

/// A clear someone asked for, waiting for the loop to be idle.
struct Clear {
    targets: Vec<ResetTarget>,
    by: String,
    done: oneshot::Sender<Result<(), String>>,
}

/// How the rest of the server reaches the loop: whether a sync is
/// running, where to write intent, and how to stop it all.
#[derive(Clone)]
pub struct SyncControl {
    root: Arc<PathBuf>,
    runs_the_loop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    wake: Arc<Notify>,
    /// The handlers' own connection. Never the loop's: the loop notices
    /// new rows by `PRAGMA data_version`, which a write on its own
    /// connection does not move.
    mailbox: Arc<OnceCell<Store>>,
    clears: Arc<Mutex<Vec<Clear>>>,
    stop: Arc<watch::Sender<bool>>,
    exited: Arc<watch::Sender<bool>>,
}

impl SyncControl {
    /// A control with no loop behind it until [`run`] is spawned with it.
    pub fn new(root: Arc<PathBuf>) -> Self {
        SyncControl {
            root,
            runs_the_loop: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            mailbox: Arc::new(OnceCell::new()),
            clears: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(watch::channel(false).0),
            exited: Arc::new(watch::channel(false).0),
        }
    }

    /// Is a sync running on this root? This server's answer while it runs
    /// the loop; before it has the lock, whether anyone holds it — a
    /// `datalib-dag` that was running when the server started.
    pub fn running(&self) -> bool {
        if self.runs_the_loop.load(Ordering::SeqCst) {
            self.busy.load(Ordering::SeqCst)
        } else {
            datalib_dag::lock::runner_is_held(&self.root)
        }
    }

    pub async fn mailbox(&self) -> anyhow::Result<&Store> {
        self.mailbox
            .get_or_try_init(|| Store::open(&self.root))
            .await
    }

    /// Something for the loop to look at: a request, a pause, a clear.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Empty what `targets` wrote (`docs/dev/plans/supervisor.md` §2.10),
    /// once no sync is running, and sync what reads them, so the emptiness
    /// reaches the grid. It needs the root to itself, so it is refused
    /// while a sync runs rather than left waiting behind one that may take
    /// an hour.
    pub async fn clear(&self, targets: &[String], by: &str) -> Result<(), String> {
        if !self.runs_the_loop.load(Ordering::SeqCst) {
            return Err(
                "another process is running syncs on this root; clear once it is done".into(),
            );
        }
        if self.running() {
            return Err("a sync is running; clear once it is over".into());
        }
        let (done, answer) = oneshot::channel();
        let targets = targets.iter().map(|t| ResetTarget::parse(t)).collect();
        self.clears
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Clear {
                targets,
                by: by.to_string(),
                done,
            });
        self.wake();
        answer
            .await
            .unwrap_or_else(|_| Err("the server stopped before the clear ran".into()))
    }

    /// Stop the loop's steps (SIGINT each, and their requests stay open
    /// for the next boot) and wait, up to `within`, for it to let go.
    /// Whether it did.
    pub async fn shutdown(&self, within: Duration) -> bool {
        let _ = self.stop.send(true);
        let mut exited = self.exited.subscribe();
        tokio::time::timeout(within, exited.wait_for(|done| *done))
            .await
            .is_ok_and(|seen| seen.is_ok())
    }
}

/// What the host runs the loop with.
pub struct HostConfig {
    pub control: SyncControl,
    /// The step binaries' directory, ahead of the config's `binary_dir`.
    pub binary_dir: Option<PathBuf>,
}

/// How often an idle host looks for intent nobody woke it for: a request
/// or a pause a `datalib-dag` client wrote.
const IDLE_POLL: Duration = Duration::from_secs(1);

pub async fn run(cfg: HostConfig) {
    let control = cfg.control.clone();
    host(&cfg).await;
    let _ = control.exited.send(true);
}

async fn host(cfg: &HostConfig) {
    let root = cfg.control.root.clone();
    let mut stop = cfg.control.stop.subscribe();
    let store = match Store::open(&root).await {
        Ok(store) => store,
        Err(e) => {
            tracing::error!(
                "supervisor: cannot open the request store, so nothing will sync: {e:#}"
            );
            return;
        }
    };
    let Some(_lock) = take_the_lock(cfg, &mut stop).await else {
        return;
    };
    cfg.control.runs_the_loop.store(true, Ordering::SeqCst);
    match host::take_over(&store, &root).await {
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

    // The pauses the record was last settled against; `None` until the
    // first settle, which also clears what a dead loop left running.
    let mut settled: Option<BTreeMap<String, String>> = None;
    while !*stop.borrow() {
        let clears =
            std::mem::take(&mut *cfg.control.clears.lock().unwrap_or_else(|e| e.into_inner()));
        for clear in clears {
            let result = run_clear(cfg, &store, &clear.targets, &clear.by).await;
            let _ = clear.done.send(result);
        }
        let open = match store.open_requests().await {
            Ok(open) => open,
            Err(e) => {
                tracing::error!("supervisor: could not read the open requests: {e:#}");
                Vec::new()
            }
        };
        if open.iter().any(|r| r.stop_requested_by.is_none()) {
            serve_period(cfg, &store).await;
            continue;
        }
        // Asked to stop before any loop took them on: closed where they
        // stand, since a busy period for them would do nothing.
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
                settle(&root, &store).await;
                settled = Some(paused);
            }
            Ok(_) => {}
            Err(e) => tracing::error!("supervisor: could not read the pauses: {e:#}"),
        }
        tokio::select! {
            _ = cfg.control.wake.notified() => {}
            _ = tokio::time::sleep(IDLE_POLL) => {}
            _ = stop.changed() => {}
        }
    }
    store.close().await;
}

/// The lock, once whoever holds it lets go. Until then a `datalib-dag`
/// runs the loop, and serves the UI's requests too.
async fn take_the_lock(
    cfg: &HostConfig,
    stop: &mut watch::Receiver<bool>,
) -> Option<datalib_dag::lock::FileLock> {
    let mut announced = false;
    loop {
        match datalib_dag::lock::try_acquire_runner(&cfg.control.root) {
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
            _ = tokio::time::sleep(IDLE_POLL) => {}
            _ = stop.changed() => return None,
        }
    }
}

/// The config as the loop would run it, or why it cannot.
pub fn load_config(root: &Path) -> Result<ConfigCheck, String> {
    let path = datalib_dag::config::root_config_path(root);
    if !path.is_file() {
        return Err(format!(
            "no config at {} — create one from the Setup tab before syncing",
            path.display()
        ));
    }
    let (checked, _) = datalib_dag::config::load_graded(&path).map_err(|e| format!("{e:#}"))?;
    if checked.is_fatal() {
        return Err(format!(
            "{} is not a config: nothing in it could be read\n{}",
            path.display(),
            checked.render(&path)
        ));
    }
    Ok(checked)
}

/// Where the loop's steps find `datalib-step` and a person's own tools.
fn extra_path() -> Vec<PathBuf> {
    // `~/.datalib/bin`, whether or not it exists yet: an agent may make
    // it between runs, and a missing entry costs nothing.
    crate::user_bin_dir().into_iter().collect()
}

/// One tick with nothing open, so a pause or a resume made while the loop
/// is idle reaches the record, and so do the steps a dead loop left
/// running.
async fn settle(root: &Path, store: &Store) {
    let checked = match load_config(root) {
        Ok(checked) => checked,
        Err(why) => return tracing::warn!("supervisor: cannot settle the record: {why}"),
    };
    if let Err(e) = Runner::new(root).settle(&checked.graph, store).await {
        tracing::error!("supervisor: could not settle the record: {e:#}");
    }
}

/// One busy period: the config loaded once, a run opened, and the loop
/// served until no request it can place is open.
async fn serve_period(cfg: &HostConfig, store: &Store) {
    let root = cfg.control.root.clone();
    let checked = match load_config(&root) {
        Ok(checked) => checked,
        Err(why) => return fail_open_requests(store, &why).await,
    };
    let run_id = datalib_dag::scheduler::new_run_id();
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs();
    let env = match host::step_env(
        &checked.cfg,
        cfg.binary_dir.as_deref(),
        &extra_path(),
        &now,
        &run_id,
    ) {
        Ok(env) => env,
        Err(e) => return fail_open_requests(store, &format!("{e:#}")).await,
    };
    let sink: Arc<dyn EventSink> = match host::start_record(&root, &checked.cfg, &run_id, &now) {
        Some(record) => Arc::new(record),
        None => {
            tracing::warn!(run = %run_id, "supervisor: run store unavailable; nothing recorded this run");
            Arc::new(datalib_dag::events::NoopSink)
        }
    };
    let runner = Runner::new(root.as_path())
        .sink(sink)
        .child_env(env.vars)
        .stop_on(cfg.control.stop.subscribe());

    cfg.control.busy.store(true, Ordering::SeqCst);
    tracing::info!(run = %run_id, "supervisor: a sync started");
    let served = runner.serve(&checked.graph, store).await;
    // The run store's record closes when its sink goes, which is the
    // runner's; before the flag drops, so no one reads "over" before it is.
    drop(runner);
    if let Err(e) = served {
        tracing::error!(run = %run_id, "supervisor: the loop failed: {e:#}");
        // Its requests would otherwise start the next busy period at once,
        // and fail the same way.
        fail_open_requests(store, &format!("the sync failed: {e:#}")).await;
        if let Err(e) = host::take_over(store, &root).await {
            tracing::error!("supervisor: could not close the failed run: {e:#}");
        }
    }
    cfg.control.busy.store(false, Ordering::SeqCst);
    tracing::info!(run = %run_id, "supervisor: the sync is over");
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

/// A clear, between busy periods: each target's step invoked with
/// `DATALIB_DAG_RESET`, in a run of its own; then a request rooted at what
/// reads them, opened for whoever asked, which the loop takes on next.
async fn run_clear(
    cfg: &HostConfig,
    store: &Store,
    targets: &[ResetTarget],
    by: &str,
) -> Result<(), String> {
    let root = cfg.control.root.clone();
    let checked = load_config(&root)?;
    let run_id = datalib_dag::scheduler::new_run_id();
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs();
    let env = host::step_env(
        &checked.cfg,
        cfg.binary_dir.as_deref(),
        &extra_path(),
        &now,
        &run_id,
    )
    .map_err(|e| format!("{e:#}"))?;
    let sink: Arc<dyn EventSink> = match host::start_record(&root, &checked.cfg, &run_id, &now) {
        Some(record) => Arc::new(record),
        None => Arc::new(datalib_dag::events::NoopSink),
    };
    cfg.control.busy.store(true, Ordering::SeqCst);
    let result = Runner::new(root.as_path())
        .sink(sink)
        .child_env(env.vars)
        .reset(&checked.graph, targets)
        .await;
    cfg.control.busy.store(false, Ordering::SeqCst);
    result.map_err(|e| format!("{e:#}"))?;
    let readers = readers_of(&checked.graph, targets);
    if !readers.is_empty() {
        store
            .open_request(&readers, by)
            .await
            .map_err(|e| format!("could not sync what reads it: {e:#}"))?;
    }
    Ok(())
}

/// The steps that read a cleared step and were not cleared themselves:
/// where the emptiness goes next.
fn readers_of(graph: &datalib_dag::Graph, targets: &[ResetTarget]) -> Vec<String> {
    let cleared: BTreeSet<&str> = targets.iter().map(|t| t.step.as_str()).collect();
    let mut readers: BTreeSet<String> = BTreeSet::new();
    for step in &cleared {
        let Some(&i) = graph.by_id.get(*step) else {
            continue;
        };
        for &d in &graph.dependents[i] {
            let id = &graph.steps[d].id;
            if !cleared.contains(id.as_str()) {
                readers.insert(id.clone());
            }
        }
    }
    readers.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With the server holding `runner-lock` for its life, "the lock is
    /// held" would say a sync is running for ever. Running the loop, the
    /// answer is the host's own; before that, the lock's.
    #[test]
    fn a_sync_is_running_while_the_loop_is_busy_not_while_the_lock_is_held() {
        let td = tempfile::tempdir().unwrap();
        let control = SyncControl::new(Arc::new(td.path().to_path_buf()));
        assert!(!control.running(), "idle root");
        let held = datalib_dag::lock::acquire_runner(td.path()).expect("claim");
        assert!(control.running(), "a datalib-dag holds it");

        control.runs_the_loop.store(true, Ordering::SeqCst);
        assert!(!control.running(), "the server holds it, idle");
        control.busy.store(true, Ordering::SeqCst);
        assert!(control.running(), "in a busy period");
        drop(held);
    }

    /// Asking must not leave a trace. A probe on a timer that created
    /// the lock file would make every never-synced root sprout one, and
    /// one that rewrote it would erase what a live holder said about
    /// itself — which is the only thing a refused runner has to go on.
    #[test]
    fn asking_whether_a_sync_is_running_leaves_no_trace() {
        let td = tempfile::tempdir().unwrap();
        let lock = td.path().join(datalib_dag::lock::RUNNER_LOCK_REL_PATH);
        let control = SyncControl::new(Arc::new(td.path().to_path_buf()));
        assert!(!control.running());
        assert!(!lock.exists(), "the probe created {}", lock.display());
    }

    /// A clear needs the root to itself; asked for while a sync runs it
    /// says so at once rather than hanging behind the sync.
    #[tokio::test]
    async fn a_clear_is_refused_while_a_sync_runs() {
        let td = tempfile::tempdir().unwrap();
        let control = SyncControl::new(Arc::new(td.path().to_path_buf()));
        control.runs_the_loop.store(true, Ordering::SeqCst);
        control.busy.store(true, Ordering::SeqCst);
        let err = control.clear(&["a/ingest".into()], "ui").await.unwrap_err();
        assert!(err.contains("a sync is running"), "{err}");
    }
}
