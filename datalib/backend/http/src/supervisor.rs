//! The server's side of the supervisor loop (`docs/dev/plans/supervisor.md`
//! §2.8): it holds `runner-lock` for as long as it is up, runs the loop
//! whenever a request or a reset or purge is open, and between busy
//! periods settles a step turned off or on into the record. The first time
//! a build runs on the root it asks every step to migrate before the first
//! request (`docs/dev/plans/upgrade_on_launch.md`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datalib_dag::config::{ConfigCheck, DagConfig};
use datalib_dag::supervisor::announce::Listener;
use datalib_dag::supervisor::host;
use datalib_dag::supervisor::reload::ConfigFile;
use datalib_dag::supervisor::store::{RequestOutcome, Store, WipeKind};
use datalib_dag::supervisor::wipe::tree_group;
use datalib_dag::{EventSink, Runner};
use tokio::sync::{watch, OnceCell};

/// Whether a reset or purge is done, or still waits for its steps to stop.
#[derive(Debug, PartialEq, Eq)]
pub enum WipeAnswer {
    Done,
    Queued,
}

/// How long a reset or purge's answer waits for the loop before it says
/// "queued": long enough for its steps to stop and the loop to do it.
const WIPE_WAIT: Duration = Duration::from_secs(10);

/// The launch's migrate pass, as `/api/config` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Upgrade {
    /// The pass is running: nothing syncs until it is done.
    pub migrating: bool,
    /// This server has run its pass, or found it had none to run. False
    /// until it holds the runner lock, which another process may hold.
    pub settled: bool,
    /// Every step the pass asks, in the order it asks them.
    pub steps: Vec<MigrateRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MigrateRow {
    pub step: String,
    pub state: MigrateState,
    pub error: Option<String>,
}

/// How the migrate pass stands with one step. Mirrored by hand in
/// `datalib/ui/src/api.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrateState {
    Waiting,
    Running,
    Done,
    Failed,
}

/// How the rest of the server reaches the loop: whether a sync is
/// running, where to write intent, and how to stop it all.
#[derive(Clone)]
pub struct SyncControl {
    root: Arc<PathBuf>,
    runs_the_loop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    upgrade: Arc<Mutex<Upgrade>>,
    /// The handlers' own connection, beside the loop's.
    mailbox: Arc<OnceCell<Store>>,
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
            upgrade: Arc::new(Mutex::new(Upgrade::default())),
            mailbox: Arc::new(OnceCell::new()),
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

    pub fn upgrade(&self) -> Upgrade {
        self.upgrade
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub async fn mailbox(&self) -> anyhow::Result<&Store> {
        self.mailbox
            .get_or_try_init(|| Store::open(&self.root))
            .await
    }

    /// Empty what `targets` wrote, and sync what reads them so the
    /// emptiness reaches the grid. The loop does it as soon as the targets
    /// have stopped, whatever else is syncing.
    pub async fn reset(&self, targets: &[String], by: &str) -> Result<WipeAnswer, String> {
        self.wipe(WipeKind::Reset, targets, by).await
    }

    /// Delete the trees of `groups`, which the config no longer names, and
    /// forget their steps ever ran, so a group re-added under the same id
    /// starts from nothing. Checked against the config at once; done by the
    /// loop as soon as the groups' steps have stopped.
    pub async fn purge(&self, groups: &[String], by: &str) -> Result<WipeAnswer, String> {
        let checked = load_config(&self.root)?;
        if let Some(why) = purge_refusal(&checked.cfg, groups) {
            return Err(why);
        }
        self.wipe(WipeKind::Purge, groups, by).await
    }

    /// Ask the loop for a wipe and wait, up to [`WIPE_WAIT`], for it to
    /// say how it went.
    async fn wipe(
        &self,
        kind: WipeKind,
        targets: &[String],
        by: &str,
    ) -> Result<WipeAnswer, String> {
        if !self.runs_the_loop.load(Ordering::SeqCst) {
            return Err(format!(
                "another process is running syncs on this root; {} once it is done",
                match kind {
                    WipeKind::Reset => "reset",
                    WipeKind::Purge => "delete",
                }
            ));
        }
        let store = self.mailbox().await.map_err(|e| format!("{e:#}"))?;
        // Made before the row, so the loop's close is not missed.
        let mut listener = Listener::new(store, "a reset or purge's answer");
        let id = store
            .open_wipe(kind, targets, by)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let answer = async {
            loop {
                let row = store.wipe(&id).await.map_err(|e| format!("{e:#}"))?;
                if let Some(closed) = row.and_then(|r| r.closed) {
                    return closed.map_or(Ok(()), Err);
                }
                listener.next().await;
            }
        };
        match tokio::time::timeout(WIPE_WAIT, answer).await {
            Ok(Ok(())) => Ok(WipeAnswer::Done),
            Ok(Err(why)) => Err(why),
            Err(_) => Ok(WipeAnswer::Queued),
        }
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
    /// Every busy period's "now", in place of the clock: what
    /// `datalib-dag --now` is to one run. For fixtures, whose output must
    /// not depend on the day they were built.
    pub now: Option<String>,
    /// Where the migrate pass says it moved, so the page asks again.
    pub announce: Option<crate::watch::RootTx>,
}

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
    let mut listener = Listener::new(&store, "the server's loop");
    let Some(_lock) = take_the_lock(cfg, &mut listener, &mut stop).await else {
        store.close().await;
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
    migrate_on_launch(cfg, &store).await;
    cfg.control
        .upgrade
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .settled = true;
    if let Some(tx) = &cfg.announce {
        let _ = tx.send(crate::watch::RootEvent::UpgradeChanged.into());
    }
    tracing::info!("supervisor: running the loop on {}", root.display());
    host::run_idle(&store, &mut listener, &mut ServerPeriods { cfg }, &mut stop).await;
    store.close().await;
}

struct ServerPeriods<'a> {
    cfg: &'a HostConfig,
}

impl host::Periods for ServerPeriods<'_> {
    async fn busy_period(&mut self, store: &Store) {
        serve_period(self.cfg, store).await;
    }

    async fn settle(&mut self, store: &Store) -> Option<BTreeMap<String, String>> {
        settle(&self.cfg.control.root, store).await
    }
}

/// The first time this build runs on the root, ask every step that takes
/// the verb to migrate, one at a time, before the loop takes any request:
/// no render then reads a raw store in an old shape. What a step cannot
/// migrate in place it answers `needs_rerun`, which the page offers to run.
/// A request opened meanwhile waits for the loop. A step that fails is
/// reported and the rest go on.
async fn migrate_on_launch(cfg: &HostConfig, store: &Store) {
    let root = cfg.control.root.clone();
    let Ok(checked) = load_config(&root) else {
        return;
    };
    let build = datalib_dag::supervisor::upgrade::this_build();
    match store.launch_pass_done(&build).await {
        Ok(false) => {}
        Ok(true) => return,
        Err(e) => {
            tracing::error!("supervisor: could not read the launch passes, so none runs: {e:#}");
            return;
        }
    }
    let asked = datalib_dag::supervisor::upgrade::steps_to_ask(&checked.graph);
    tracing::info!(
        build,
        steps = asked.len(),
        "supervisor: asking each step to migrate for this build"
    );
    let set = |f: &dyn Fn(&mut Upgrade)| {
        f(&mut cfg
            .control
            .upgrade
            .lock()
            .unwrap_or_else(|e| e.into_inner()));
        if let Some(tx) = &cfg.announce {
            let _ = tx.send(crate::watch::RootEvent::UpgradeChanged.into());
        }
    };
    set(&|u| {
        *u = Upgrade {
            migrating: true,
            settled: false,
            steps: asked
                .iter()
                .map(|step| MigrateRow {
                    step: step.clone(),
                    state: MigrateState::Waiting,
                    error: None,
                })
                .collect(),
        }
    });
    cfg.control.busy.store(true, Ordering::SeqCst);
    let run_id = datalib_dag::scheduler::new_run_id();
    let now = now(cfg);
    let runner = host::step_env(
        &checked.cfg,
        cfg.binary_dir.as_deref(),
        &extra_path(),
        &now,
        &run_id,
    )
    .map(|env| {
        let sink: Arc<dyn EventSink> = match host::start_record(&root, &checked.cfg, &run_id, &now)
        {
            Some(record) => Arc::new(record),
            None => Arc::new(datalib_dag::events::NoopSink),
        };
        Runner::new(root.as_path()).sink(sink).child_env(env.vars)
    });
    for (i, step) in asked.iter().enumerate() {
        set(&|u| u.steps[i].state = MigrateState::Running);
        let answer = match &runner {
            Ok(runner) => match runner
                .migrate(&checked.graph, std::slice::from_ref(step))
                .await
            {
                Ok(done) => done
                    .into_iter()
                    .next()
                    .map(|m| m.answer)
                    .unwrap_or(Ok(false)),
                Err(e) => Err(format!("{e:#}")),
            },
            Err(e) => Err(format!("{e:#}")),
        };
        match &answer {
            Ok(false) => {}
            Ok(true) => tracing::info!(step, "supervisor: needs to run again for this build"),
            Err(why) => tracing::error!(step, "supervisor: could not migrate: {why}"),
        }
        set(&|u| {
            (u.steps[i].state, u.steps[i].error) = match &answer {
                Ok(_) => (MigrateState::Done, None),
                Err(why) => (MigrateState::Failed, Some(why.clone())),
            };
        });
    }
    if let Err(e) = store.record_launch_pass(&build).await {
        tracing::error!(
            "supervisor: could not record the launch pass, so the next launch asks again: {e:#}"
        );
    }
    cfg.control.busy.store(false, Ordering::SeqCst);
    set(&|u| u.migrating = false);
}

/// The lock, once whoever holds it lets go. Until then a `datalib-dag`
/// runs the loop, and serves the UI's requests too.
async fn take_the_lock(
    cfg: &HostConfig,
    listener: &mut Listener,
    stop: &mut watch::Receiver<bool>,
) -> Option<datalib_dag::lock::RunnerLock> {
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
            _ = listener.next() => {}
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

/// One tick with nothing open, so a step turned off or on while the loop
/// is idle reaches the record, and so do the steps a dead loop left
/// running.
async fn settle(root: &Path, store: &Store) -> Option<BTreeMap<String, String>> {
    let checked = match load_config(root) {
        Ok(checked) => checked,
        Err(why) => {
            tracing::warn!("supervisor: cannot settle the record: {why}");
            return None;
        }
    };
    match Runner::new(root).settle(&checked.graph, store).await {
        Ok(turned_off) => Some(turned_off),
        Err(e) => {
            tracing::error!("supervisor: could not settle the record: {e:#}");
            None
        }
    }
}

fn now(cfg: &HostConfig) -> String {
    cfg.now
        .clone()
        .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs())
}

/// One busy period: a run opened, and the loop served until no request
/// it can place is open. The loop re-reads the config as it goes; the
/// step environment is the one built here.
async fn serve_period(cfg: &HostConfig, store: &Store) {
    let root = cfg.control.root.clone();
    let checked = match load_config(&root) {
        Ok(checked) => checked,
        Err(why) => return fail_open_requests(store, &why).await,
    };
    let run_id = datalib_dag::scheduler::new_run_id();
    let now = now(cfg);
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
        .stop_on(cfg.control.stop.subscribe())
        .reload_from(Arc::new(ConfigFile::new(
            datalib_dag::config::root_config_path(&root),
        )));

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

/// Why the trees of `groups` may not be deleted, or `None`. Each must be
/// an id a config could name as a group — so `<root>/<id>` is that
/// group's tree and nothing else — and one `cfg` no longer names.
fn purge_refusal(cfg: &DagConfig, groups: &[String]) -> Option<String> {
    if groups.is_empty() {
        return Some("no groups to delete".into());
    }
    if let Some(bad) = groups
        .iter()
        .find(|g| !datalib_dag::config::usable_group_id(g))
    {
        return Some(format!("{bad:?} is not a group id"));
    }
    let named: BTreeSet<&str> = cfg
        .groups
        .iter()
        .map(|g| g.id.as_str())
        .chain(cfg.steps.iter().map(|s| tree_group(&s.id)))
        .collect();
    groups
        .iter()
        .find(|g| named.contains(g.as_str()))
        .map(|g| format!("the config still has {g:?}; remove it from the config first"))
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

    /// A reset is not refused while a sync runs: the loop does it as soon
    /// as its step has stopped. Only another process's loop refuses it.
    #[tokio::test]
    async fn a_reset_is_refused_only_when_another_process_runs_the_loop() {
        let td = tempfile::tempdir().unwrap();
        let control = SyncControl::new(Arc::new(td.path().to_path_buf()));
        control.busy.store(true, Ordering::SeqCst);
        let err = control.reset(&["a/ingest".into()], "ui").await.unwrap_err();
        assert!(err.contains("another process"), "{err}");
    }

    /// A root whose config has one group, `keep`, with one step.
    fn root_keeping_one_group() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            datalib_dag::config::root_config_path(td.path()),
            "[[groups]]\nid = \"keep\"\n[[steps]]\ngroup = \"keep\"\nfunction = \"raw\"\ncommand = \"x\"\n",
        )
        .unwrap();
        td
    }

    /// A purge deletes a directory under the data root, so it takes only
    /// a group id — never a path, never `system` — and never a group the
    /// config still runs.
    #[test]
    fn a_purge_takes_only_a_group_the_config_no_longer_names() {
        let td = root_keeping_one_group();
        let cfg = load_config(td.path()).unwrap().cfg;
        let refusal = |groups: &[&str]| {
            let groups: Vec<String> = groups.iter().map(|g| g.to_string()).collect();
            purge_refusal(&cfg, &groups)
        };
        assert_eq!(refusal(&["gone"]), None);
        assert!(refusal(&[]).is_some());
        assert!(refusal(&["gone", "keep"])
            .unwrap()
            .contains("still has \"keep\""));
        for bad in ["", "..", "system", "a/b", "../elsewhere"] {
            assert!(
                refusal(&[bad]).unwrap().contains("not a group id"),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_purge_is_refused_when_another_process_runs_the_loop() {
        let td = root_keeping_one_group();
        let control = SyncControl::new(Arc::new(td.path().to_path_buf()));
        let err = control.purge(&["gone".into()], "ui").await.unwrap_err();
        assert!(err.contains("another process"), "{err}");
    }
}
