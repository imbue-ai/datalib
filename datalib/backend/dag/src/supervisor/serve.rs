//! The loop held for the whole life of the process that runs it,
//! `datalib-pipeline serve`, which the app's server starts as a child. It
//! takes `runner-lock` (waiting while another process holds
//! it), closes what a dead loop left, runs the prepare pass, then serves
//! every request and wipe until told to stop. What a parent needs to
//! hear and the stores do not hold, it says as [`ServeEvent`]s.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::config::ConfigCheck;
use crate::events::{EventSink, NoopSink};
use crate::lock::RunnerLock;
use crate::scheduler::{PrepareError, Runner};
use crate::supervisor::announce::Listener;
use crate::supervisor::host;
use crate::supervisor::pass::PassState;
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
    /// The prepare pass will ask these steps, in this order. Not said
    /// when no step's program takes the verb.
    PreparePlan {
        steps: Vec<String>,
    },
    /// Where the prepare pass stands with one step.
    PrepareStep {
        step: String,
        state: PassState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The prepare pass is over, or there was none to run; the loop takes
    /// requests from now on.
    PrepareDone,
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
    prepare(opts, &emit).await;
    open_the_reruns(&store, root).await;
    emit(ServeEvent::PrepareDone);
    tracing::info!("supervisor: running the loop on {}", root.display());
    let mut periods = Periods {
        opts,
        emit: &emit,
        stop: &stop,
    };
    host::run_idle(&store, &mut listener, &mut periods, &mut stop.clone()).await;
    store.close().await;
}

/// The config as the loop would run it, or why it cannot.
pub fn load_config(root: &Path) -> Result<ConfigCheck, String> {
    let path = crate::config::root_config_path(root);
    if !path.is_file() {
        return Err(format!(
            "no config at {} — create one before syncing: in the app's Setup tab, or from \
             a shell with `datalib-pipeline init {}`, then add a source to it",
            path.display(),
            root.display()
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

/// Each time this process takes the loop, ask every step whose program
/// takes `--prepare` to make what it stored usable, one at a time, before
/// the loop takes any request: no render then reads a raw store in an old
/// shape. What a step cannot bring up to date in place it answers
/// `needs_rerun`, which the loop then runs ([`open_the_reruns`]); what a newer build wrote
/// it refuses, and the runner turns it off. A request opened meanwhile
/// waits for the loop. A step that fails is reported and the rest go on.
async fn prepare(opts: &ServeOptions, emit: &Emit) {
    let root = opts.root.as_path();
    let Ok(checked) = load_config(root) else {
        return;
    };
    let asked = crate::supervisor::upgrade::steps_to_ask(&checked.graph);
    if asked.is_empty() {
        return;
    }
    tracing::info!(
        steps = asked.len(),
        "supervisor: asking each step to prepare"
    );
    emit(ServeEvent::PreparePlan {
        steps: asked.clone(),
    });
    let run_id = crate::scheduler::new_run_id();
    let now = now(opts);
    let runner = steps_env(opts, &checked.cfg, &now, &run_id).map(|env| {
        let started = host::prepare_start();
        let sink: Arc<dyn EventSink> =
            match host::start_record(root, &checked.cfg, &run_id, &now, started) {
                Some(record) => Arc::new(record),
                None => Arc::new(NoopSink),
            };
        Runner::new(root).sink(sink).child_env(env.vars)
    });
    if let Ok(runner) = &runner {
        runner.plan(&checked.graph, &asked);
    }
    for step in &asked {
        emit(ServeEvent::PrepareStep {
            step: step.clone(),
            state: PassState::Running,
            error: None,
        });
        let answer = match &runner {
            Ok(runner) => match runner
                .prepare(&checked.graph, std::slice::from_ref(step))
                .await
            {
                Ok(done) => done
                    .into_iter()
                    .next()
                    .map(|m| m.answer.map_err(|e| prepare_error(&e)))
                    .unwrap_or(Ok(false)),
                Err(e) => Err(format!("{e:#}")),
            },
            Err(e) => Err(format!("{e:#}")),
        };
        match &answer {
            Ok(false) => {}
            Ok(true) => tracing::info!(step, "supervisor: needs to run again before it is current"),
            Err(why) => tracing::error!(step, "supervisor: could not prepare: {why}"),
        }
        let (state, error) = match answer {
            Ok(_) => (PassState::Done, None),
            Err(why) => (PassState::Failed, Some(why)),
        };
        emit(ServeEvent::PrepareStep {
            step: step.clone(),
            state,
            error,
        });
    }
}

/// Who opens the request that runs what the prepare pass could not bring
/// up to date in place.
pub const UPGRADE_REQUESTER: &str = "upgrade";

/// Run what the prepare pass marked `needs_rerun`, without waiting to be
/// asked: nothing reads a store in another shape, so what those steps
/// wrote is not shown until they run, and none of them downloads. One
/// request, unless one this requester opened is still open.
async fn open_the_reruns(store: &Store, root: &Path) {
    let Ok(checked) = load_config(root) else {
        return;
    };
    let opened = async {
        let record = store.load_record().await?;
        let roots = crate::supervisor::round::rerun_offer(&checked.graph, &record);
        if roots.is_empty() {
            return anyhow::Ok(None);
        }
        let open = store.open_requests().await?;
        if open.iter().any(|r| r.opened_by == UPGRADE_REQUESTER) {
            return anyhow::Ok(None);
        }
        store
            .open_request(&roots, UPGRADE_REQUESTER)
            .await
            .map(|id| Some((id, roots)))
    }
    .await;
    match opened {
        Ok(Some((id, roots))) => tracing::info!(
            request = %id,
            steps = %roots.join(", "),
            "supervisor: running the steps the prepare pass could not bring up to date"
        ),
        Ok(None) => {}
        Err(e) => tracing::error!("supervisor: could not open the upgrade's re-runs: {e:#}"),
    }
}

fn prepare_error(e: &PrepareError) -> String {
    match e {
        PrepareError::Refused(why) => {
            format!("refused, and is turned off until it no longer refuses: {why}")
        }
        PrepareError::Failed(why) => why.clone(),
    }
}

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

struct Periods<'a> {
    opts: &'a ServeOptions,
    emit: &'a Emit,
    stop: &'a watch::Receiver<bool>,
}

impl host::Periods for Periods<'_> {
    async fn busy_period(&mut self, store: &Store) {
        serve_period(self.opts, self.emit, self.stop, store).await;
    }

    async fn settle(&mut self, store: &Store) -> Option<BTreeMap<String, String>> {
        settle(&self.opts.root, store).await
    }
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

/// One busy period: a run opened, and the loop served until no request
/// it can place is open. The loop re-reads the config as it goes; the
/// step environment is the one built here.
async fn serve_period(
    opts: &ServeOptions,
    emit: &Emit,
    stop: &watch::Receiver<bool>,
    store: &Store,
) {
    let root = opts.root.as_path();
    let checked = match load_config(root) {
        Ok(checked) => checked,
        Err(why) => return fail_open_requests(store, &why).await,
    };
    let run_id = crate::scheduler::new_run_id();
    let now = now(opts);
    let env = match steps_env(opts, &checked.cfg, &now, &run_id) {
        Ok(env) => env,
        Err(e) => return fail_open_requests(store, &format!("{e:#}")).await,
    };
    let open = store.open_requests().await.unwrap_or_default();
    let wiping = store.open_wipes().await.unwrap_or_default();
    let started = host::period_start(&open, &wiping);
    let sink: Arc<dyn EventSink> = match host::start_record(
        root,
        &checked.cfg,
        &run_id,
        &now,
        started,
    ) {
        Some(record) => Arc::new(record),
        None => {
            tracing::warn!(run = %run_id, "supervisor: run store unavailable; nothing recorded this run");
            Arc::new(NoopSink)
        }
    };
    let mut runner = Runner::new(root)
        .sink(sink)
        .child_env(env.vars)
        .stop_on(stop.clone())
        .reload_from(Arc::new(ConfigFile::new(crate::config::root_config_path(
            root,
        ))));
    if let Some(n) = opts.parallelism {
        runner = runner.parallelism(n);
    }

    emit(ServeEvent::SyncStarted {
        run_id: run_id.clone(),
    });
    tracing::info!(run = %run_id, "supervisor: a sync started");
    let served = runner.serve(&checked.graph, store).await;
    // The run store's record closes when its sink goes, which is the
    // runner's; before the loop says it is over, so no one reads "over"
    // before it is.
    drop(runner);
    if let Err(e) = served {
        tracing::error!(run = %run_id, "supervisor: the loop failed: {e:#}");
        // Its requests would otherwise start the next busy period at once,
        // and fail the same way.
        fail_open_requests(store, &format!("the sync failed: {e:#}")).await;
        if let Err(e) = host::take_over(store, root).await {
            tracing::error!("supervisor: could not close the failed run: {e:#}");
        }
    }
    emit(ServeEvent::SyncOver {
        run_id: run_id.clone(),
    });
    tracing::info!(run = %run_id, "supervisor: the sync is over");
    say_runner_statements(&run_id);
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
            ServeEvent::PreparePlan {
                steps: vec!["a/ingest".into()],
            },
            ServeEvent::PrepareStep {
                step: "a/ingest".into(),
                state: PassState::Failed,
                error: Some("why".into()),
            },
            ServeEvent::PrepareDone,
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
        assert_eq!(
            serde_json::to_string(&ServeEvent::PrepareStep {
                step: "a/ingest".into(),
                state: PassState::Running,
                error: None,
            })
            .unwrap(),
            r#"{"event":"prepare_step","step":"a/ingest","state":"running"}"#
        );
    }
}
