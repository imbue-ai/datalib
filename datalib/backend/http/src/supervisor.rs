//! The server's side of the supervisor loop (`docs/dev/plans/supervisor.md`
//! §2.8): it holds `runner-lock` for as long as it is up, runs the loop
//! whenever a request is open, runs reset jobs in between, and keeps each
//! UI job's row in step with the request behind it. A job and its request
//! share an id; the job's `parent_job_id` is the run that served it — one
//! busy period of the loop, from idle to busy and back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use app_schema::sync_jobs::{JobKind, JobState, SyncJobRow};
use datalib_core::repo::DynAppRepo;
use datalib_dag::config::ConfigCheck;
use datalib_dag::supervisor::host;
use datalib_dag::supervisor::store::{RequestOutcome, RequestRow, Store};
use datalib_dag::supervisor::RequestEvent;
use datalib_dag::{EventSink, Runner};
use serde::Serialize;
use tokio::sync::{broadcast, watch, Notify, OnceCell};

/// A push update for one job, fanned out to SSE subscribers
/// (`GET /api/sync/stream`) the instant its row changes — so the UI
/// reflects a job starting or ending without polling. What the run is
/// doing in between reaches the UI another way: the loop's writes to
/// `system/runs/runs.sqlite` are pushed as `table_changed` root frames
/// naming the datasets they feed (`watch.rs`).
#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub id: String,
    pub kind: String,
    /// Comma-separated source-step ids, mirroring
    /// [`SyncJobRow::source_ids`].
    pub source_ids: Option<String>,
    pub state: JobState,
    /// Whether the job still holds its sources after this event — the
    /// same answer [`SyncJobRow::is_active`] gives for the row, so a
    /// reader never has to work it out from `state`.
    pub active: bool,
    pub progress_msg: Option<String>,
}

impl ProgressEvent {
    pub fn new(job: &SyncJobRow, state: JobState, msg: Option<String>) -> Self {
        ProgressEvent {
            id: job.id.clone(),
            kind: job.kind.clone(),
            source_ids: job.source_ids.clone(),
            state,
            // A job told to stop is still winding down until it is
            // stamped finished.
            active: matches!(state, JobState::Pending | JobState::Running)
                || (state == JobState::Canceled && job.is_active()),
            progress_msg: msg,
        }
    }
}

/// Shared by the host and the HTTP enqueue/cancel handlers; the SSE
/// endpoint subscribes to it. A `send` with no subscribers is a no-op.
pub type ProgressTx = broadcast::Sender<ProgressEvent>;

/// How the rest of the server reaches the loop: whether a sync is
/// running, where to write a request, and how to stop it all.
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

    /// Something for the loop to look at: a request, a reset, a stop.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Stop the loop's steps (SIGINT each, and their requests stay open
    /// for the next boot) and wait, up to `within`, for it to let go.
    /// Whether it did.
    pub async fn shutdown(&self, within: Duration) -> bool {
        let _ = self.stop.send(true);
        let mut exited = self.exited.subscribe();
        let let_go = tokio::time::timeout(within, exited.wait_for(|done| *done))
            .await
            .is_ok_and(|seen| seen.is_ok());
        let_go
    }
}

/// What the host runs the loop with.
pub struct HostConfig {
    pub control: SyncControl,
    pub repo: DynAppRepo,
    /// The step binaries' directory, ahead of the config's `binary_dir`.
    pub binary_dir: Option<PathBuf>,
    pub progress_tx: ProgressTx,
}

/// How often an idle host looks for a request nobody woke it for: one a
/// `datalib-dag` client wrote.
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
    match host::close_dead_loop(&root).await {
        Ok(Some(run)) => tracing::warn!(run, "supervisor: closed a run a dead loop left open"),
        Ok(None) => {}
        Err(e) => tracing::error!("supervisor: could not close a dead loop's run: {e:#}"),
    }
    recover(cfg).await;
    tracing::info!("supervisor: running the loop on {}", root.display());

    while !*stop.borrow() {
        let open = match store.open_requests().await {
            Ok(open) => open,
            Err(e) => {
                tracing::error!("supervisor: could not read the open requests: {e:#}");
                Vec::new()
            }
        };
        let jobs = cfg.repo.list_jobs(true, 1_000).await.unwrap_or_else(|e| {
            tracing::error!("supervisor: could not list the jobs: {e}");
            Vec::new()
        });
        match next_move(&open, &jobs) {
            Next::Reset(id) => {
                let job = jobs
                    .into_iter()
                    .find(|j| j.id == id)
                    .expect("next_move picked it");
                run_reset(cfg, job).await
            }
            Next::Serve => serve_period(cfg, &store).await,
            Next::CloseStopped(stopped) => {
                for (id, by) in stopped {
                    if let Err(e) = store
                        .close_request(&id, RequestOutcome::Stopped, None)
                        .await
                    {
                        tracing::error!(request = %id, "supervisor: could not close it: {e:#}");
                    }
                    job_closed(cfg, &id, Some(RequestOutcome::Stopped), None, Some(&by)).await;
                }
            }
            Next::Idle => {
                tokio::select! {
                    _ = cfg.control.wake.notified() => {}
                    _ = tokio::time::sleep(IDLE_POLL) => {}
                    _ = stop.changed() => {}
                }
            }
        }
    }
    store.close().await;
}

/// The lock, once whoever holds it lets go. Until then a `datalib-dag`
/// is running the loop and serving the UI's requests too, so a job whose
/// request it closes is finished here all the same.
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
                finish_closed_jobs(cfg).await;
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

/// What the host does when no busy period is running.
#[derive(Debug, PartialEq)]
enum Next {
    /// The oldest reset job, by id: it needs the root to itself, and
    /// between busy periods it has it.
    Reset(String),
    Serve,
    /// Requests asked to stop before any loop took them on: (id, by whom).
    CloseStopped(Vec<(String, String)>),
    Idle,
}

fn next_move(open: &[RequestRow], active_jobs: &[SyncJobRow]) -> Next {
    let oldest_reset = active_jobs
        .iter()
        .filter(|j| j.kind == JobKind::Reset.as_str() && j.job_state() == Some(JobState::Pending))
        .min_by(|a, b| (&a.created_at_utc, &a.id).cmp(&(&b.created_at_utc, &b.id)));
    if let Some(job) = oldest_reset {
        return Next::Reset(job.id.clone());
    }
    if open.iter().any(|r| r.stop_requested_by.is_none()) {
        return Next::Serve;
    }
    if open.is_empty() {
        return Next::Idle;
    }
    Next::CloseStopped(
        open.iter()
            .filter_map(|r| Some((r.id.clone(), r.stop_requested_by.clone()?)))
            .collect(),
    )
}

/// The config as the loop would run it, or why it cannot.
fn load_config(root: &Path) -> Result<ConfigCheck, String> {
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

/// One busy period: the config loaded once, a run opened, and the loop
/// served until no request it can place is open.
async fn serve_period(cfg: &HostConfig, store: &Store) {
    let root = cfg.control.root.clone();
    let checked = match load_config(&root) {
        Ok(checked) => checked,
        Err(why) => return fail_open_requests(cfg, store, &why).await,
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
        Err(e) => return fail_open_requests(cfg, store, &format!("{e:#}")).await,
    };
    let sink: Arc<dyn EventSink> = match host::start_record(&root, &checked.cfg, &run_id, &now) {
        Some(record) => Arc::new(record),
        None => {
            tracing::warn!(run = %run_id, "supervisor: run store unavailable; nothing recorded this run");
            Arc::new(datalib_dag::events::NoopSink)
        }
    };
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let mut runner = Runner::new(root.as_path())
        .sink(sink)
        .child_env(env.vars)
        .stop_on(cfg.control.stop.subscribe());
    runner.requests = Some(tx);

    cfg.control.busy.store(true, Ordering::SeqCst);
    tracing::info!(run = %run_id, "supervisor: a sync started");
    let mut admitted: Vec<String> = Vec::new();
    let served = {
        let serving = runner.serve(&checked.graph, store);
        tokio::pin!(serving);
        loop {
            tokio::select! {
                biased;
                Some(event) = events.recv() => {
                    on_event(cfg, &run_id, &checked, event, &mut admitted).await;
                }
                served = &mut serving => break served,
            }
        }
    };
    // The run store's record closes when its sink goes, which is the
    // runner's; before the flag drops, so no one reads "over" before it is.
    drop(runner);
    while let Ok(event) = events.try_recv() {
        on_event(cfg, &run_id, &checked, event, &mut admitted).await;
    }
    if let Err(e) = served {
        tracing::error!(run = %run_id, "supervisor: the loop failed: {e:#}");
        // Its requests would otherwise start the next busy period at once,
        // and fail the same way.
        let why = format!("the sync failed: {e:#}");
        for id in admitted {
            let _ = store.close_request(&id, RequestOutcome::Failed, None).await;
            finish(cfg, &id, JobState::Failed, Some(&why)).await;
        }
        if let Err(e) = host::close_dead_loop(&root).await {
            tracing::error!("supervisor: could not close the failed run: {e:#}");
        }
    }
    cfg.control.busy.store(false, Ordering::SeqCst);
    tracing::info!(run = %run_id, "supervisor: the sync is over");
}

async fn on_event(
    cfg: &HostConfig,
    run_id: &str,
    checked: &ConfigCheck,
    event: RequestEvent,
    admitted: &mut Vec<String>,
) {
    match event {
        RequestEvent::Admitted { id } => {
            admitted.push(id.clone());
            let Some(job) = cfg.repo.get_job(&id).await.ok().flatten() else {
                return;
            };
            let msg = format!(
                "syncing {}…",
                job.source_ids.as_deref().unwrap_or("all sources")
            );
            match cfg.repo.start_job(&id, run_id, Some(&msg)).await {
                Ok(Some(started)) => {
                    let state = started.job_state().unwrap_or(JobState::Running);
                    emit(&cfg.progress_tx, &started, state, Some(&msg));
                }
                Ok(None) => {}
                Err(e) => tracing::error!(job = %id, "supervisor: could not start the job: {e}"),
            }
        }
        RequestEvent::Closed {
            id,
            outcome,
            failed_step,
            stopped_by,
        } => {
            admitted.retain(|a| a != &id);
            let failed = failed_step.map(|step| {
                let known = checked.graph.by_id.contains_key(&step);
                (step, known)
            });
            job_closed(
                cfg,
                &id,
                Some(outcome),
                failed.as_ref().map(|(s, k)| (s.as_str(), *k)),
                stopped_by.as_deref(),
            )
            .await;
        }
    }
}

/// How a job ends, from how its request did. `failed` is the step it
/// failed at, and whether the config has that step at all.
fn job_end(
    outcome: Option<RequestOutcome>,
    failed: Option<(&str, bool)>,
    stopped_by: Option<&str>,
) -> (JobState, Option<String>) {
    match outcome {
        Some(RequestOutcome::Done) => (JobState::Done, None),
        Some(RequestOutcome::Failed) => (
            JobState::Failed,
            Some(match failed {
                Some((step, true)) => format!("{step} failed; its row has the log"),
                Some((step, false)) => format!("the config has no step {step}"),
                None => "the sync failed".to_string(),
            }),
        ),
        Some(RequestOutcome::Stopped) => (
            JobState::Canceled,
            Some(match stopped_by {
                None | Some("ui") => "canceled by user".to_string(),
                Some(who) => format!("stopped by {who}"),
            }),
        ),
        None => (
            JobState::Failed,
            Some(
                "its request was closed by a newer build, with an outcome this one cannot name"
                    .into(),
            ),
        ),
    }
}

async fn job_closed(
    cfg: &HostConfig,
    id: &str,
    outcome: Option<RequestOutcome>,
    failed: Option<(&str, bool)>,
    stopped_by: Option<&str>,
) {
    let (state, msg) = job_end(outcome, failed, stopped_by);
    if state == JobState::Done {
        let _ = cfg.repo.update_job_progress(id, Some(1.0), None).await;
    }
    finish(cfg, id, state, msg.as_deref()).await;
}

/// Close every open request with `why` — the config cannot be run — and
/// finish its job the same way, so the next look does not try again.
async fn fail_open_requests(cfg: &HostConfig, store: &Store, why: &str) {
    tracing::warn!("supervisor: cannot sync: {why}");
    for request in store.open_requests().await.unwrap_or_default() {
        let outcome = match request.stop_requested_by {
            Some(_) => RequestOutcome::Stopped,
            None => RequestOutcome::Failed,
        };
        let _ = store.close_request(&request.id, outcome, None).await;
        match request.stop_requested_by {
            Some(by) => job_closed(cfg, &request.id, Some(outcome), None, Some(&by)).await,
            None => finish(cfg, &request.id, JobState::Failed, Some(why)).await,
        }
    }
}

/// A reset job, between busy periods: each target's step invoked with
/// `DATALIB_DAG_RESET`, in a run of its own.
async fn run_reset(cfg: &HostConfig, job: SyncJobRow) {
    let root = cfg.control.root.clone();
    let run_id = datalib_dag::scheduler::new_run_id();
    let targets = job.source_ids.clone().unwrap_or_default();
    let msg = format!("resetting {targets}…");
    let Some(started) = cfg
        .repo
        .start_job(&job.id, &run_id, Some(&msg))
        .await
        .ok()
        .flatten()
    else {
        return finish(cfg, &job.id, JobState::Failed, Some("could not start it")).await;
    };
    if started.job_state() != Some(JobState::Running) {
        return finish(cfg, &job.id, JobState::Canceled, Some("canceled by user")).await;
    }
    emit(&cfg.progress_tx, &started, JobState::Running, Some(&msg));
    let checked = match load_config(&root) {
        Ok(checked) => checked,
        Err(why) => return finish(cfg, &job.id, JobState::Failed, Some(&why)).await,
    };
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs();
    let env = match host::step_env(
        &checked.cfg,
        cfg.binary_dir.as_deref(),
        &extra_path(),
        &now,
        &run_id,
    ) {
        Ok(env) => env,
        Err(e) => return finish(cfg, &job.id, JobState::Failed, Some(&format!("{e:#}"))).await,
    };
    let sink: Arc<dyn EventSink> = match host::start_record(&root, &checked.cfg, &run_id, &now) {
        Some(record) => Arc::new(record),
        None => Arc::new(datalib_dag::events::NoopSink),
    };
    let targets: Vec<_> = targets
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(datalib_dag::scheduler::ResetTarget::parse)
        .collect();
    cfg.control.busy.store(true, Ordering::SeqCst);
    let result = {
        let runner = Runner::new(root.as_path()).sink(sink).child_env(env.vars);
        runner.reset(&checked.graph, &targets).await
    };
    cfg.control.busy.store(false, Ordering::SeqCst);
    match result {
        Ok(()) => {
            let _ = cfg.repo.update_job_progress(&job.id, Some(1.0), None).await;
            finish(cfg, &job.id, JobState::Done, None).await;
        }
        Err(e) => finish(cfg, &job.id, JobState::Failed, Some(&format!("{e:#}"))).await,
    }
}

/// What becomes of a job the last server left active.
#[derive(Debug, PartialEq)]
enum Recovery {
    Leave,
    /// Running when the server died, and its request is still open: it
    /// is queued again, and the next busy period takes it on.
    Requeue,
    /// Its request never got written.
    OpenRequest,
    /// Told to stop, and the stop never reached its request.
    AskStop,
    Finish(JobState, String),
}

fn recovery(job: &SyncJobRow, request: Option<&RequestRow>) -> Recovery {
    let state = job.job_state();
    if job.kind == JobKind::Reset.as_str() {
        return match state {
            Some(JobState::Pending) => Recovery::Leave,
            Some(JobState::Canceled) => Recovery::Finish(
                JobState::Canceled,
                "canceled by user; the server stopped before the reset finished".into(),
            ),
            _ => Recovery::Finish(
                JobState::Failed,
                "interrupted: the server stopped while this reset ran".into(),
            ),
        };
    }
    let Some(request) = request else {
        return match state {
            Some(JobState::Pending) => Recovery::OpenRequest,
            Some(JobState::Canceled) => Recovery::Finish(
                JobState::Canceled,
                "canceled by user; the server stopped before the sync had".into(),
            ),
            _ => Recovery::Finish(
                JobState::Failed,
                "interrupted: the server stopped while this job ran".into(),
            ),
        };
    };
    if let Some(outcome) = request.closed {
        let failed = request.failed_step.as_deref().map(|s| (s, true));
        let (state, msg) = job_end(outcome, failed, request.stop_requested_by.as_deref());
        return Recovery::Finish(state, msg.unwrap_or_default());
    }
    match state {
        Some(JobState::Running) => Recovery::Requeue,
        Some(JobState::Canceled) if request.stop_requested_by.is_none() => Recovery::AskStop,
        _ => Recovery::Leave,
    }
}

async fn recover(cfg: &HostConfig) {
    let Ok(mailbox) = cfg.control.mailbox().await else {
        return;
    };
    let jobs = match cfg.repo.list_jobs(true, 1_000).await {
        Ok(jobs) => jobs,
        Err(e) => return tracing::error!("supervisor: startup recovery could not list jobs: {e}"),
    };
    for job in jobs {
        let request = mailbox.request(&job.id).await.ok().flatten();
        let fix = recovery(&job, request.as_ref());
        if fix != Recovery::Leave {
            tracing::warn!(job = %job.id, "supervisor: recovering a job the last server left: {fix:?}");
        }
        match fix {
            Recovery::Leave => {}
            Recovery::Requeue => {
                if let Err(e) = cfg.repo.requeue_job(&job.id).await {
                    tracing::error!(job = %job.id, "supervisor: could not requeue it: {e}");
                }
            }
            Recovery::OpenRequest => {
                if let Err(why) = open_request_for(&cfg.control, &job).await {
                    finish(cfg, &job.id, JobState::Failed, Some(&why)).await;
                }
            }
            Recovery::AskStop => {
                let _ = mailbox.request_stop(&job.id, "ui").await;
            }
            Recovery::Finish(state, why) => {
                // A run this job's last server had open; closed already
                // unless it was from before the server ran the loop itself.
                let run = job.parent_job_id.as_deref().unwrap_or(&job.id);
                let _ = datalib_runs::close_abandoned_run(
                    &cfg.control.root,
                    run,
                    datalib_dag::run_state::RunState::Stopped.as_str(),
                    &why,
                )
                .await;
                finish(cfg, &job.id, state, Some(&why)).await;
            }
        }
    }
}

/// While another process runs the loop, the jobs whose requests it has
/// closed are finished here.
async fn finish_closed_jobs(cfg: &HostConfig) {
    let Ok(mailbox) = cfg.control.mailbox().await else {
        return;
    };
    let Ok(jobs) = cfg.repo.list_jobs(true, 1_000).await else {
        return;
    };
    for job in jobs.iter().filter(|j| j.kind != JobKind::Reset.as_str()) {
        let Some(request) = mailbox.request(&job.id).await.ok().flatten() else {
            continue;
        };
        if let Some(outcome) = request.closed {
            let failed = request.failed_step.as_deref().map(|s| (s, true));
            job_closed(
                cfg,
                &job.id,
                outcome,
                failed,
                request.stop_requested_by.as_deref(),
            )
            .await;
        }
    }
}

/// The request a sync job stands for, under the job's id: rooted at the
/// sources it names, or at every source the config has.
pub async fn open_request_for(control: &SyncControl, job: &SyncJobRow) -> Result<(), String> {
    let named: Vec<String> = job
        .source_ids
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let roots = if named.is_empty() {
        let checked = load_config(&control.root)?;
        checked
            .graph
            .fringe_ids()
            .into_iter()
            .map(str::to_string)
            .collect()
    } else {
        named
    };
    let mailbox = control.mailbox().await.map_err(|e| format!("{e:#}"))?;
    mailbox
        .open_request_as(&job.id, &roots, "ui")
        .await
        .map_err(|e| format!("could not write the request: {e:#}"))?;
    control.wake();
    Ok(())
}

fn emit(tx: &ProgressTx, job: &SyncJobRow, state: JobState, msg: Option<&str>) {
    let _ = tx.send(ProgressEvent::new(job, state, msg.map(str::to_string)));
}

/// Stamp a job finished and say so. A write that did not land leaves the
/// UI showing the job as running until the next boot; the store is a
/// doltlite file this process owns, where a failed statement is a passing
/// condition, so it is tried once more.
async fn finish(cfg: &HostConfig, id: &str, state: JobState, msg: Option<&str>) {
    let write = || cfg.repo.finish_job(id, state, msg);
    if let Err(first) = write().await {
        tracing::error!(job = %id, "supervisor: finishing the job failed ({first}); retrying once");
        if let Err(e) = write().await {
            tracing::error!(job = %id, "supervisor: finishing the job failed again ({e}); the row is wrong until the next boot");
        }
    }
    match cfg.repo.get_job(id).await {
        Ok(Some(job)) => emit(&cfg.progress_tx, &job, state, msg),
        // A request some other client opened: no job to tell anyone of.
        Ok(None) => {}
        Err(e) => tracing::error!(job = %id, "supervisor: could not read the job back: {e}"),
    }
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

    fn job(kind: JobKind, state: JobState, created: &str) -> SyncJobRow {
        SyncJobRow {
            id: format!("job-{created}"),
            kind: kind.as_str().into(),
            source_ids: Some("a/ingest".into()),
            parent_job_id: None,
            state: state.as_str().into(),
            created_at_utc: created.into(),
            started_at_utc: None,
            finished_at_utc: None,
            tz_offset: None,
            error: None,
            pid: None,
            progress_pct: None,
            progress_msg: None,
        }
    }

    fn request(id: &str) -> RequestRow {
        RequestRow {
            id: id.into(),
            roots: vec!["a/ingest".into()],
            opened_by: "ui".into(),
            stop_requested_by: None,
            closed: None,
            failed_step: None,
        }
    }

    /// A reset needs the root to itself, so it goes before a busy period
    /// rather than waiting behind one that may never empty.
    #[test]
    fn the_oldest_pending_reset_goes_first() {
        let resets = [
            job(JobKind::Reset, JobState::Pending, "2026-09-23T10:00:02Z"),
            job(JobKind::Reset, JobState::Pending, "2026-09-23T10:00:01Z"),
            job(JobKind::Reset, JobState::Canceled, "2026-09-23T10:00:00Z"),
        ];
        assert_eq!(
            next_move(&[request("r")], &resets),
            Next::Reset("job-2026-09-23T10:00:01Z".into())
        );
    }

    /// A request asked to stop before any loop took it on is closed where
    /// it stands: a busy period for it would open a run to do nothing.
    #[test]
    fn requests_that_were_only_ever_stopped_are_closed_without_a_run() {
        let mut stopped = request("s");
        stopped.stop_requested_by = Some("ui".into());
        assert_eq!(
            next_move(std::slice::from_ref(&stopped), &[]),
            Next::CloseStopped(vec![("s".into(), "ui".into())])
        );
        assert_eq!(next_move(&[stopped, request("r")], &[]), Next::Serve);
        assert_eq!(next_move(&[], &[]), Next::Idle);
    }

    #[test]
    fn a_job_ends_the_way_its_request_did() {
        use RequestOutcome::*;
        assert_eq!(job_end(Some(Done), None, None), (JobState::Done, None));
        let (state, msg) = job_end(Some(Failed), Some(("a/ingest", true)), None);
        assert_eq!(state, JobState::Failed);
        assert!(msg.unwrap().starts_with("a/ingest failed"));
        let (_, msg) = job_end(Some(Failed), Some(("gone/ingest", false)), None);
        assert_eq!(msg.as_deref(), Some("the config has no step gone/ingest"));
        assert_eq!(
            job_end(Some(Stopped), None, Some("ui")),
            (JobState::Canceled, Some("canceled by user".into()))
        );
        assert_eq!(
            job_end(Some(Stopped), None, Some("claude")).1.as_deref(),
            Some("stopped by claude")
        );
        assert_eq!(job_end(None, None, None).0, JobState::Failed);
    }

    /// Boot recovery reads each job against its request: a request is a
    /// row that outlives the server, so a job it still stands for is run
    /// again rather than written off.
    #[test]
    fn a_job_the_last_server_left_is_set_to_match_its_request() {
        let running = job(JobKind::All, JobState::Running, "t");
        let open = request(&running.id);
        assert_eq!(recovery(&running, Some(&open)), Recovery::Requeue);

        let pending = job(JobKind::All, JobState::Pending, "t");
        assert_eq!(recovery(&pending, Some(&open)), Recovery::Leave);
        assert_eq!(recovery(&pending, None), Recovery::OpenRequest);

        let canceled = job(JobKind::All, JobState::Canceled, "t");
        assert_eq!(recovery(&canceled, Some(&open)), Recovery::AskStop);
        let mut stopping = open.clone();
        stopping.stop_requested_by = Some("ui".into());
        assert_eq!(recovery(&canceled, Some(&stopping)), Recovery::Leave);

        let mut done = open.clone();
        done.closed = Some(Some(RequestOutcome::Done));
        assert_eq!(
            recovery(&running, Some(&done)),
            Recovery::Finish(JobState::Done, String::new())
        );

        assert!(matches!(
            recovery(&running, None),
            Recovery::Finish(JobState::Failed, why) if why.starts_with("interrupted")
        ));
        assert_eq!(
            recovery(&job(JobKind::Reset, JobState::Pending, "t"), None),
            Recovery::Leave
        );
        assert!(matches!(
            recovery(&job(JobKind::Reset, JobState::Running, "t"), None),
            Recovery::Finish(JobState::Failed, _)
        ));
    }
}
