//! The server's side of the supervisor loop (`dag/README.md`
//! §"Two locks, two files"): it keeps `datalib-pipeline serve` running as
//! a child for as long as it is up, keeps what the child says on its
//! stderr that no store holds — the prepare pass, whether a sync is
//! running — and is how the handlers write the rows anyone may write
//! in `system/supervisor.sqlite` (`requests`, `turned_off`, `jobs`).

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use datalib_dag::config::{ConfigCheck, DagConfig};
use datalib_dag::supervisor::announce::Listener;
use datalib_dag::supervisor::pass::PassRow;
use datalib_dag::supervisor::serve::{self, ServeEvent};
use datalib_dag::supervisor::store::{JobKind, Store};
use datalib_dag::supervisor::wipe::tree_group;
use tokio::io::AsyncBufReadExt;
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
    /// The pass is running: a step it has not reached, and what reads
    /// one, waits for it.
    pub migrating: bool,
    /// This server's runner has run its pass, or found it had none to
    /// run. False until the runner holds the runner lock, which another
    /// process may hold.
    pub settled: bool,
    /// Every step the pass runs `--prepare` for, in the order it runs
    /// them.
    pub steps: Vec<PassRow>,
}

/// The newest "Free up disk space" this server has seen asked for, as
/// `/api/config` reports it: the same rows as the prepare pass, each with
/// what its store gave back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FreeSpace {
    pub id: String,
    /// `collect`, or `compact` when old versions of derived data go.
    pub kind: JobKind,
    /// The loop is still on it.
    pub running: bool,
    /// Why it was not done, once it is over.
    pub error: Option<String>,
    /// Every step it runs with `--compact` or `--collect`, in order;
    /// empty until the loop starts on it.
    pub steps: Vec<PassRow>,
}

/// How the rest of the server reaches the loop: whether a sync is
/// running, where to write intent, and how to stop it all.
#[derive(Clone)]
pub struct SyncControl {
    root: Arc<PathBuf>,
    /// This server's runner holds `runner-lock`.
    runs_the_loop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    /// Why nothing can sync from this server: no runner found, or one
    /// that keeps stopping. Every Sync button says it.
    trouble: Arc<Mutex<Option<String>>>,
    /// When this server started, stamped as the loop's store stamps a
    /// row: a prepare pass, compaction or collection opened before it is
    /// not this server's to report.
    started_at_utc: Arc<String>,
    /// The handlers' own connection, beside the loop's.
    supervisor_store: Arc<OnceCell<Store>>,
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
            trouble: Arc::new(Mutex::new(None)),
            started_at_utc: Arc::new(datalib_dag::supervisor::store::now_utc()),
            supervisor_store: Arc::new(OnceCell::new()),
            stop: Arc::new(watch::channel(false).0),
            exited: Arc::new(watch::channel(false).0),
        }
    }

    /// Is a sync running on this root? While this server's runner holds
    /// the lock, whether that runner last reported a run under way;
    /// before that, whether anyone holds it — a `datalib-pipeline` that
    /// was running when the server started.
    pub fn running(&self) -> bool {
        if self.runs_the_loop.load(Ordering::SeqCst) {
            self.busy.load(Ordering::SeqCst)
        } else {
            datalib_dag::lock::runner_is_held(&self.root)
        }
    }

    /// The prepare pass of the loop running since this server started,
    /// as the store holds it: its row in `jobs`, open while it runs, with
    /// a row in `job_steps` per step it runs `--prepare` for.
    pub async fn upgrade(&self) -> anyhow::Result<Upgrade> {
        let store = self.supervisor_store().await?;
        let Some(job) = store.latest_prepare_since(&self.started_at_utc).await? else {
            return Ok(Upgrade::default());
        };
        Ok(Upgrade {
            migrating: job.closed.is_none(),
            settled: job.closed.is_some(),
            steps: store.job_steps(&job.id).await?,
        })
    }

    /// Why nothing can sync from this server, when nothing can.
    pub fn trouble(&self) -> Option<String> {
        self.trouble
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_trouble(&self, why: Option<String>) -> bool {
        let mut trouble = self.trouble.lock().unwrap_or_else(|e| e.into_inner());
        let changed = *trouble != why;
        *trouble = why;
        changed
    }

    /// The newest compaction or collection opened since this server
    /// started, with how each of its steps stands.
    pub async fn free_space(&self) -> anyhow::Result<Option<FreeSpace>> {
        let store = self.supervisor_store().await?;
        let Some(wipe) = store.latest_shrink_since(&self.started_at_utc).await? else {
            return Ok(None);
        };
        let Some(kind) = wipe.kind else {
            return Ok(None);
        };
        Ok(Some(FreeSpace {
            steps: store.job_steps(&wipe.id).await?,
            id: wipe.id,
            kind,
            running: wipe.closed.is_none(),
            error: wipe.closed.flatten(),
        }))
    }

    pub async fn supervisor_store(&self) -> anyhow::Result<&Store> {
        self.supervisor_store
            .get_or_try_init(|| Store::open(&self.root))
            .await
    }

    /// Empty what `targets` wrote, and sync what reads them so the
    /// emptiness reaches the grid. The loop does it as soon as the targets
    /// have stopped, whatever else is syncing.
    pub async fn reset(&self, targets: &[String], by: &str) -> Result<WipeAnswer, String> {
        self.wipe(JobKind::Reset, targets, by).await
    }

    /// Delete the trees of `groups`, which the config no longer names, and
    /// their steps' rows in the loop's `steps` table, so a group re-added
    /// under the same id
    /// starts from nothing. Checked against the config at once; done by the
    /// loop as soon as the groups' steps have stopped.
    pub async fn purge(&self, groups: &[String], by: &str) -> Result<WipeAnswer, String> {
        let checked = load_config(&self.root)?;
        if let Some(why) = purge_refusal(&checked.cfg, groups) {
            return Err(why);
        }
        self.wipe(JobKind::Purge, groups, by).await
    }

    /// Give back the disk no commit reaches in the stores of `targets`, or
    /// of every step that takes the verb when it names none; with
    /// `drop_old_versions_on_derived_data`, fold each render store's
    /// history first. The loop starts none of them, nor what reads them,
    /// meanwhile.
    pub async fn free_disk_space(
        &self,
        targets: &[String],
        drop_old_versions_on_derived_data: bool,
        by: &str,
    ) -> Result<WipeAnswer, String> {
        let kind = if drop_old_versions_on_derived_data {
            JobKind::Compact
        } else {
            JobKind::Collect
        };
        self.wipe(kind, targets, by).await
    }

    /// Write a row to the `jobs` table and wait, up to [`WIPE_WAIT`], for
    /// the loop to close it. Whichever process runs the loop carries it
    /// out.
    async fn wipe(
        &self,
        kind: JobKind,
        targets: &[String],
        by: &str,
    ) -> Result<WipeAnswer, String> {
        self.wipe_waiting(kind, targets, by, WIPE_WAIT).await
    }

    async fn wipe_waiting(
        &self,
        kind: JobKind,
        targets: &[String],
        by: &str,
        wait: Duration,
    ) -> Result<WipeAnswer, String> {
        let store = self
            .supervisor_store()
            .await
            .map_err(|e| format!("{e:#}"))?;
        // Made before the row, so the loop's close is not missed.
        let mut listener = Listener::new(store, "a wipe's answer");
        let id = store
            .open_job(kind, targets, by)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let answer = async {
            loop {
                let row = store.job(&id).await.map_err(|e| format!("{e:#}"))?;
                if let Some(closed) = row.and_then(|r| r.closed) {
                    return closed.map_or(Ok(()), Err);
                }
                listener.next().await;
            }
        };
        match tokio::time::timeout(wait, answer).await {
            Ok(Ok(())) => Ok(WipeAnswer::Done),
            Ok(Err(why)) => Err(why),
            Err(_) => Ok(WipeAnswer::Queued),
        }
    }

    /// Stop the runner (SIGTERM: it interrupts its steps, and their
    /// requests stay open for the next boot) and wait, up to `within`,
    /// for it to exit. Whether it did. One that has not is left to see
    /// this server go: its parent pipe closes, and it stops its steps and
    /// kills what is left after their grace.
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
    /// The `datalib-pipeline` binary ([`crate::binaries::resolve_pipeline_bin`]);
    /// with none, nothing syncs and every Sync button says why.
    pub pipeline: Option<PathBuf>,
    /// The step binaries' directory, ahead of the config's `binary_dir`.
    pub binary_dir: Option<PathBuf>,
    /// Every run's "now", in place of the clock: what
    /// `datalib-pipeline --now` is to one run. For fixtures, whose output must
    /// not depend on the day they were built.
    pub now: Option<String>,
    /// The channel to `/api/sync/stream`, where a change to the Manage
    /// rows is sent, so the page fetches them again.
    pub stream: Option<crate::watch::RootTx>,
    /// Told each time the runner writes to the run store.
    pub nudge: Option<crate::watch::Nudge>,
}

/// What the Sync buttons say when no runner was found.
const NO_PIPELINE: &str = "Syncing is unavailable: no datalib-pipeline was found next to \
     datalib-http (or at $DATALIB_PIPELINE_BIN, in $DATALIB_BINARY_DIR or on PATH).";

/// A runner that lived this long was working, and its next death starts
/// the count of deaths in a row again.
const HEALTHY_FOR: Duration = Duration::from_secs(60);

/// After this many deaths in a row, every Sync button says the runner
/// keeps stopping.
const TROUBLE_AFTER: u32 = 3;

/// The wait before restarting a runner after its `n`th death in a row:
/// one second, doubling, at most a minute.
fn backoff(deaths: u32) -> Duration {
    Duration::from_secs(1u64 << deaths.saturating_sub(1).min(6)).min(Duration::from_secs(60))
}

/// Keep `datalib-pipeline serve` running on this root until
/// [`SyncControl::shutdown`], restarting it when it dies, and keep what
/// it says for the handlers.
pub async fn run(cfg: HostConfig) {
    let control = cfg.control.clone();
    let mut stop = control.stop.subscribe();
    let Some(bin) = cfg.pipeline.clone() else {
        tracing::error!("{NO_PIPELINE}");
        control.set_trouble(Some(NO_PIPELINE.to_string()));
        let _ = stop.wait_for(|stopped| *stopped).await;
        let _ = control.exited.send(true);
        return;
    };
    let mut deaths = 0u32;
    while !*stop.borrow() {
        let started = Instant::now();
        let ended = run_child(&bin, &cfg, &mut stop).await;
        control.runs_the_loop.store(false, Ordering::SeqCst);
        control.busy.store(false, Ordering::SeqCst);
        let why = match ended {
            Ok(()) => break,
            Err(why) => why,
        };
        if started.elapsed() >= HEALTHY_FOR {
            deaths = 0;
        }
        deaths += 1;
        let wait = backoff(deaths);
        tracing::error!(
            deaths,
            "supervisor: datalib-pipeline serve stopped ({why}); starting it again in {wait:?}"
        );
        if deaths >= TROUBLE_AFTER
            && control.set_trouble(Some(format!(
                "Syncing is unavailable: the runner keeps stopping ({why}). \
                 The log has each time it stopped."
            )))
        {
            control.rows_changed(cfg.stream.as_ref());
        }
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|stopped| *stopped) => break,
        }
    }
    let _ = control.exited.send(true);
}

/// Run one `datalib-pipeline serve` until it exits. `Ok` when it exited
/// because this server asked it to stop; otherwise why it died.
async fn run_child(
    bin: &Path,
    cfg: &HostConfig,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let control = &cfg.control;
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("serve").arg(control.root.as_path());
    if let Some(dir) = &cfg.binary_dir {
        cmd.arg("--binary-dir").arg(dir);
    }
    if let Some(now) = &cfg.now {
        cmd.arg("--now").arg(now);
    }
    datalib_env::inherit_for_runner(cmd.as_std_mut());
    // A group of its own, so a terminal's Ctrl-C reaches only this
    // server, which stops the runner itself rather than see it die.
    std::os::unix::process::CommandExt::process_group(cmd.as_std_mut(), 0);
    // stdin is a liveness pipe, never written: however this server ends,
    // the kernel closes it, and the runner stops its steps and exits
    // (`datalib_parent_watch`).
    cmd.env(datalib_parent_watch::ENV_VAR, "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", bin.display()))?;
    let Some(stderr) = child.stderr.take() else {
        return Err("its stderr was not captured".into());
    };
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    // What it printed that was not an event: a panic, or why it would not
    // start. The last few say why it died.
    let mut said: VecDeque<String> = VecDeque::new();
    let mut stopping = false;
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => match serde_json::from_str::<ServeEvent>(&line) {
                    Ok(event) => control.hear(event, cfg.stream.as_ref(), cfg.nudge.as_ref()),
                    Err(_) => {
                        tracing::warn!("datalib-pipeline serve: {line}");
                        said.push_back(line);
                        if said.len() > 5 {
                            said.pop_front();
                        }
                    }
                },
                Ok(None) | Err(_) => break,
            },
            _ = stop.wait_for(|stopped| *stopped), if !stopping => {
                stopping = true;
                if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) {
                    // Safety: kill(2) on the child this function started
                    // and has not yet reaped, so the pid is still its.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
            }
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|e| format!("could not wait for it: {e}"))?;
    if stopping {
        return Ok(());
    }
    let last = said.back().map(|l| format!(": {l}")).unwrap_or_default();
    Err(format!("{status}{last}"))
}

impl SyncControl {
    fn rows_changed(&self, stream: Option<&crate::watch::RootTx>) {
        if let Some(tx) = stream {
            let _ = tx.send(
                crate::watch::RootEvent::TableChanged {
                    table: crate::watch::Table::ManageRows,
                }
                .into(),
            );
        }
    }

    /// Keep what the runner said, and tell the page what it changed.
    fn hear(
        &self,
        event: ServeEvent,
        stream: Option<&crate::watch::RootTx>,
        nudge: Option<&crate::watch::Nudge>,
    ) {
        let rows_changed = || self.rows_changed(stream);
        match event {
            ServeEvent::Holding => {
                self.runs_the_loop.store(true, Ordering::SeqCst);
                if self.set_trouble(None) {
                    rows_changed();
                }
            }
            ServeEvent::SyncStarted { .. } => self.busy.store(true, Ordering::SeqCst),
            ServeEvent::SyncOver { .. } => self.busy.store(false, Ordering::SeqCst),
            ServeEvent::RunStoreWritten => {
                if let Some(nudge) = nudge {
                    nudge.the_run_store_moved();
                }
            }
        }
    }
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
    /// held" would say a sync is running for ever. Once this server's
    /// runner holds the lock, `running` is what that runner reports;
    /// before that, whether the lock is held.
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

    /// A reset is not refused while another process runs the loop:
    /// whichever process holds the lock carries out the wipe row. With no
    /// loop to close the row, it is queued.
    #[tokio::test]
    async fn a_reset_is_queued_for_whichever_process_runs_the_loop() {
        let td = tempfile::tempdir().unwrap();
        let control = SyncControl::new(Arc::new(td.path().to_path_buf()));
        let held = datalib_dag::lock::acquire_runner(td.path()).expect("claim");
        let answer = control
            .wipe_waiting(
                JobKind::Reset,
                &["a/ingest".into()],
                "ui",
                Duration::from_millis(200),
            )
            .await;
        assert_eq!(answer, Ok(WipeAnswer::Queued));
        let store = control.supervisor_store().await.unwrap();
        let open = store.open_jobs().await.unwrap();
        assert_eq!(open.len(), 1, "{open:?}");
        drop(held);
    }

    /// One second, doubling, never more than a minute.
    #[test]
    fn a_dead_runner_is_restarted_after_a_growing_wait() {
        let waits: Vec<u64> = (1..=9).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    /// The runner's being back clears what its deaths made every Sync
    /// button say.
    #[test]
    fn holding_the_lock_clears_the_trouble() {
        let td = tempfile::tempdir().unwrap();
        let control = SyncControl::new(Arc::new(td.path().to_path_buf()));
        control.set_trouble(Some("keeps stopping".into()));
        control.hear(ServeEvent::Holding, None, None);
        assert_eq!(control.trouble(), None);
    }

    /// The prepare pass reaches `/api/config` from the store: its row in
    /// `jobs` and a row per step in `job_steps`, as the loop writes them.
    /// Before, the server kept it in memory from the loop's stderr, where
    /// no other client could read it.
    #[tokio::test]
    async fn the_prepare_pass_is_read_from_the_store() {
        use datalib_dag::supervisor::pass::PassState;
        use datalib_dag::supervisor::store::JobKind;
        let td = tempfile::tempdir().unwrap();
        let control = SyncControl::new(Arc::new(td.path().to_path_buf()));
        assert_eq!(control.upgrade().await.unwrap(), Upgrade::default());

        let store = Store::open(td.path()).await.unwrap();
        let job = store
            .open_job(JobKind::Prepare, &[], "the loop")
            .await
            .unwrap();
        store
            .begin_job_steps(&job, &["a/ingest", "a/render"])
            .await
            .unwrap();
        let u = control.upgrade().await.unwrap();
        assert!(u.migrating && !u.settled);
        assert!(u.steps.iter().all(|r| r.state == PassState::Waiting));
        store
            .set_job_step(&job, 1, PassState::Failed, Some("why"), None)
            .await
            .unwrap();
        store.close_job(&job, None).await.unwrap();
        let u = control.upgrade().await.unwrap();
        assert!(!u.migrating && u.settled);
        assert_eq!(u.steps[0].state, PassState::Waiting);
        assert_eq!(u.steps[1].state, PassState::Failed, "kept for the pill");
        assert_eq!(u.steps[1].error.as_deref(), Some("why"));
        store.close().await;
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
}
