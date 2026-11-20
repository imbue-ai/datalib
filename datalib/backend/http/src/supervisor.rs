//! The server's side of the supervisor loop (`dag/README.md`
//! §"Two locks, two files"): it runs the loop `datalib-pipeline serve` runs
//! (`datalib_dag::supervisor::serve`) for as long as it is up, keeps what
//! the loop says that no store holds — the prepare pass, whether a sync
//! is running — and is how the handlers write to the loop's mailbox.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datalib_dag::config::{ConfigCheck, DagConfig};
use datalib_dag::supervisor::announce::Listener;
use datalib_dag::supervisor::pass::PassRow;
use datalib_dag::supervisor::serve::{self, ServeEvent, ServeOptions};
use datalib_dag::supervisor::store::{Store, WipeKind};
use datalib_dag::supervisor::wipe::tree_group;
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

/// The prepare pass at this server's start, as `/api/config` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Upgrade {
    /// The pass is running: nothing syncs until it is done.
    pub migrating: bool,
    /// This server has run its pass, or found it had none to run. False
    /// until it holds the runner lock, which another process may hold.
    pub settled: bool,
    /// Every step the pass asks, in the order it asks them.
    pub steps: Vec<PassRow>,
}

/// The newest "Free up disk space" this server has seen asked for, as
/// `/api/config` reports it: the same rows as the prepare pass, each with
/// what its store gave back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FreeSpace {
    pub id: String,
    /// `collect`, or `compact` when old versions of derived data go.
    pub kind: WipeKind,
    /// The loop is still on it.
    pub running: bool,
    /// Why it was not done, once it is over.
    pub error: Option<String>,
    /// Every step it asks, in the order it asks them; empty until the
    /// loop takes it up.
    pub steps: Vec<PassRow>,
}

/// How the rest of the server reaches the loop: whether a sync is
/// running, where to write intent, and how to stop it all.
#[derive(Clone)]
pub struct SyncControl {
    root: Arc<PathBuf>,
    runs_the_loop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    upgrade: Arc<Mutex<Upgrade>>,
    /// When this server started, as the record stamps: a free-space pass
    /// opened before it is not this server's to report.
    started_at_utc: Arc<String>,
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
            started_at_utc: Arc::new(datalib_dag::supervisor::store::now_utc()),
            mailbox: Arc::new(OnceCell::new()),
            stop: Arc::new(watch::channel(false).0),
            exited: Arc::new(watch::channel(false).0),
        }
    }

    /// Is a sync running on this root? This server's answer while it runs
    /// the loop; before it has the lock, whether anyone holds it — a
    /// `datalib-pipeline` that was running when the server started.
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

    /// The newest compaction or collection opened since this server
    /// started, with how each of its steps stands.
    pub async fn free_space(&self) -> anyhow::Result<Option<FreeSpace>> {
        let store = self.mailbox().await?;
        let Some(wipe) = store.latest_shrink_since(&self.started_at_utc).await? else {
            return Ok(None);
        };
        let Some(kind) = wipe.kind else {
            return Ok(None);
        };
        Ok(Some(FreeSpace {
            steps: store.wipe_steps(&wipe.id).await?,
            id: wipe.id,
            kind,
            running: wipe.closed.is_none(),
            error: wipe.closed.flatten(),
        }))
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

    /// Give back the disk no commit reaches in the stores of `targets`, or
    /// of every step that takes the verb when it names none; with
    /// `drop_old_versions_on_derived_data`, fold each render store's history first. Holds
    /// them and what reads them meanwhile.
    pub async fn free_disk_space(
        &self,
        targets: &[String],
        drop_old_versions_on_derived_data: bool,
        by: &str,
    ) -> Result<WipeAnswer, String> {
        let kind = if drop_old_versions_on_derived_data {
            WipeKind::Compact
        } else {
            WipeKind::Collect
        };
        self.wipe(kind, targets, by).await
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
                    WipeKind::Compact | WipeKind::Collect => "free up disk space",
                }
            ));
        }
        let store = self.mailbox().await.map_err(|e| format!("{e:#}"))?;
        // Made before the row, so the loop's close is not missed.
        let mut listener = Listener::new(store, "a wipe's answer");
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
    /// `datalib-pipeline --now` is to one run. For fixtures, whose output must
    /// not depend on the day they were built.
    pub now: Option<String>,
    /// Where the prepare pass says it moved, so the page asks again.
    pub announce: Option<crate::watch::RootTx>,
}

/// Run the loop in this process (`datalib_dag::supervisor::serve`) until
/// [`SyncControl::shutdown`], keeping what it says for the handlers.
pub async fn run(cfg: HostConfig) {
    let control = cfg.control.clone();
    let opts = ServeOptions {
        root: (*control.root).clone(),
        binary_dir: cfg.binary_dir.clone(),
        now: cfg.now.clone(),
        parallelism: None,
        extra_env: backup_secrets(),
    };
    let heard = control.clone();
    let announce = cfg.announce.clone();
    let emit: serve::Emit = Arc::new(move |event| heard.hear(event, announce.as_ref()));
    serve::run(&opts, emit, control.stop.subscribe()).await;
    let _ = control.exited.send(true);
}

impl SyncControl {
    /// Keep what the loop said, and tell the page what it changed.
    fn hear(&self, event: ServeEvent, announce: Option<&crate::watch::RootTx>) {
        let tell = |event: crate::watch::RootEvent| {
            if let Some(tx) = announce {
                let _ = tx.send(event.into());
            }
        };
        // Every row's Sync is disabled while the pass runs and enabled
        // after: a change to the rows as much as to `upgrade`.
        let rows_changed = || {
            tell(crate::watch::RootEvent::TableChanged {
                table: crate::watch::Table::ManageRows,
            })
        };
        let upgrade = || self.upgrade.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            ServeEvent::Holding => self.runs_the_loop.store(true, Ordering::SeqCst),
            ServeEvent::PreparePlan { steps } => {
                *upgrade() = Upgrade {
                    migrating: true,
                    settled: false,
                    steps: steps.iter().map(|step| PassRow::waiting(step)).collect(),
                };
                tell(crate::watch::RootEvent::PassChanged);
                rows_changed();
                self.busy.store(true, Ordering::SeqCst);
            }
            ServeEvent::PrepareStep { step, state, error } => {
                if let Some(row) = upgrade().steps.iter_mut().find(|r| r.step == step) {
                    row.state = state;
                    row.error = error;
                }
                tell(crate::watch::RootEvent::PassChanged);
            }
            ServeEvent::PrepareDone => {
                let was_migrating = std::mem::replace(&mut upgrade().migrating, false);
                if was_migrating {
                    self.busy.store(false, Ordering::SeqCst);
                    rows_changed();
                }
                upgrade().settled = true;
                tell(crate::watch::RootEvent::PassChanged);
            }
            ServeEvent::SyncStarted { .. } => self.busy.store(true, Ordering::SeqCst),
            ServeEvent::SyncOver { .. } => self.busy.store(false, Ordering::SeqCst),
            // This process's own run-store hook already says so
            // (`boot.rs`), for every writer in it.
            ServeEvent::RunStoreWritten => {}
        }
    }
}

/// The backup secrets `datalib-step`'s WhatsApp and Signal sources read
/// when the config names no variable of its own, when this server was
/// started with them. A step is handed only an allowlist of the runner's
/// environment (`datalib_env::Child::Step`), which leaves these out.
fn backup_secrets() -> std::collections::BTreeMap<String, String> {
    datalib_env::BACKUP_SECRET_KEYS
        .iter()
        .filter_map(|key| Some((key.to_string(), std::env::var(key).ok()?)))
        .collect()
}

/// The config as the loop would run it, or why it cannot.
pub fn load_config(root: &Path) -> Result<ConfigCheck, String> {
    serve::load_config(root)
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
        assert!(control.running(), "a datalib-pipeline holds it");

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

    /// The prepare pass reaches `/api/config` as the loop says it: every
    /// step waiting, then each as it runs and ends, Sync paused until the
    /// pass is over, and `settled` only then.
    #[test]
    fn the_prepare_pass_is_reported_as_the_loop_says_it() {
        use datalib_dag::supervisor::pass::PassState;
        let td = tempfile::tempdir().unwrap();
        let control = SyncControl::new(Arc::new(td.path().to_path_buf()));
        control.hear(ServeEvent::Holding, None);
        assert!(control.runs_the_loop.load(Ordering::SeqCst));
        control.hear(
            ServeEvent::PreparePlan {
                steps: vec!["a/ingest".into(), "a/render".into()],
            },
            None,
        );
        let u = control.upgrade();
        assert!(u.migrating && !u.settled && control.running());
        assert_eq!(u.steps.len(), 2);
        assert!(u.steps.iter().all(|r| r.state == PassState::Waiting));
        control.hear(
            ServeEvent::PrepareStep {
                step: "a/render".into(),
                state: PassState::Failed,
                error: Some("why".into()),
            },
            None,
        );
        let u = control.upgrade();
        assert_eq!(u.steps[0].state, PassState::Waiting);
        assert_eq!(u.steps[1].state, PassState::Failed);
        assert_eq!(u.steps[1].error.as_deref(), Some("why"));
        control.hear(ServeEvent::PrepareDone, None);
        let u = control.upgrade();
        assert!(!u.migrating && u.settled && !control.running());
        assert_eq!(u.steps[1].state, PassState::Failed, "kept for the pill");

        control.hear(ServeEvent::SyncStarted { run_id: "r".into() }, None);
        assert!(control.running());
        control.hear(ServeEvent::SyncOver { run_id: "r".into() }, None);
        assert!(!control.running());
    }

    /// A root whose config has one group, `keep`, with one step.
    fn root_keeping_one_group() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            datalib_dag::config::root_config_path(td.path()),
            "[[groups]]\nid = \"keep\"\n[[steps]]\nid = \"keep/raw\"\ncommand = \"x\"\n",
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
        for bad in ["", "..", "system", "SYSTEM", "Keep", "a/b", "../elsewhere"] {
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
