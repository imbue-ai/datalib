//! The runner's settings and the pieces of a round that are not the
//! decision: retries, recording an outcome in `dag_state.json`, the
//! consumers' queue depth, reset. The round itself is
//! `supervisor/round.rs`; what starts when is `supervisor/tick.rs`.
//!
//! The scheduling rules — what a run selects, what makes a step stale, and
//! why a version is reported rather than measured — are in the crate README.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::events::{Event, EventSink, NoopSink, StepProgress};
use crate::graph::Graph;
use crate::run_state::RunState;
use crate::state::{DagState, LastRun};
use crate::step::{
    ArtifactState, FailureKind, StepCtx, StepError, StepId, StepOutcome, StepRun, StepSpec,
    StopSignal,
};
use crate::supervisor::tick::Budgets;
use crate::version::tree_version;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Max invocations (first try + retries) per failure kind.
    pub transient_attempts: u32,
    pub rate_limited_attempts: u32,
    /// Sleep before the first retry, doubled each further retry.
    pub backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            transient_attempts: 3,
            rate_limited_attempts: 3,
            backoff: Duration::from_secs(1),
        }
    }
}

impl RetryPolicy {
    fn max_attempts(&self, kind: FailureKind) -> u32 {
        match kind {
            FailureKind::Transient => self.transient_attempts,
            FailureKind::RateLimited => self.rate_limited_attempts,
            // Auth: a human has to act. Data: retrying won't help.
            // Cancelled: the user asked us to stop.
            FailureKind::Auth | FailureKind::Data | FailureKind::Cancelled => 1,
        }
    }
}

pub struct Runner {
    pub data_root: PathBuf,
    /// How many invocations of each class may run at once. Separate
    /// budgets are what keep four downloads from starving the index.
    pub budgets: Budgets,
    pub sink: Arc<dyn EventSink>,
    pub retry: RetryPolicy,
    /// Subset-sync mode: the source steps (those with no declared
    /// inputs) the user asked to sync. The run executes those steps
    /// plus their transitive dependents and nothing else; `None` (the
    /// default) selects every source step, so the subgraph is the whole
    /// graph. See [`Runner::runnable_subgraph`].
    pub only_fringe: Option<std::collections::HashSet<String>>,
    /// Extra environment applied to every subprocess step — run-wide
    /// settings like `PATH` (with the binary dir prepended) and the
    /// pinned `DATALIB_DAG_NOW`. A step's own `env:` entries win
    /// on key collision.
    pub child_env: Arc<BTreeMap<String, String>>,
    /// Flipped to `true` to stop the round: running steps get SIGINT on
    /// their process group and nothing new starts.
    pub stop: Option<tokio::sync::watch::Receiver<bool>>,
}

impl Runner {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            budgets: Budgets::from_parallelism(4),
            sink: Arc::new(NoopSink),
            retry: RetryPolicy::default(),
            only_fringe: None,
            child_env: Arc::new(BTreeMap::new()),
            stop: None,
        }
    }

    pub fn stop_on(mut self, rx: tokio::sync::watch::Receiver<bool>) -> Self {
        self.stop = Some(rx);
        self
    }

    pub fn sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sink = sink;
        self
    }

    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn only_fringe(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.only_fringe = Some(ids.into_iter().collect());
        self
    }

    pub fn child_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.child_env = Arc::new(env);
        self
    }
}

/// One `--reset` argument: a step id, optionally `+blobs` to take an
/// ingest step's blob CAS with its store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetTarget {
    pub step: StepId,
    pub part: String,
}

impl ResetTarget {
    pub fn parse(arg: &str) -> ResetTarget {
        let (step, part) = arg.split_once('+').unwrap_or((arg, "store"));
        ResetTarget {
            step: step.to_string(),
            part: part.to_string(),
        }
    }
}

impl Runner {
    /// Empty what the named steps wrote and forget that they ever ran, so
    /// the next run does their work from the start. Each target is one
    /// invocation of the step with `DATALIB_DAG_RESET` naming the part;
    /// what that empties is the step's to say (`step_protocol.md` § Reset).
    /// Nothing else runs: the caller holds the runner lock, which is what
    /// makes emptying a store safe.
    pub async fn reset(&self, graph: &Graph, targets: &[ResetTarget]) -> Result<()> {
        let mut state = DagState::load(&self.data_root).context("load dag state")?;
        for target in targets {
            state.steps.remove(&target.step);
        }
        state.save(&self.data_root).context("save dag state")?;
        for target in targets {
            let &i = graph
                .by_id
                .get(&target.step)
                .with_context(|| format!("--reset {}: no such step", target.step))?;
            let spec = &graph.steps[i];
            let StepRun::Subprocess { argv, env, .. } = &spec.run else {
                anyhow::bail!("--reset {}: not a subprocess step", target.step);
            };
            let ctx = StepCtx {
                step_id: spec.id.clone(),
                group: spec.group.clone(),
                group_type: spec.group_type.clone(),
                function: spec.function.clone(),
                data_root: self.data_root.clone(),
                inputs: vec![],
                changed_inputs: vec![],
                progress: StepProgress::new(spec.id.clone(), self.sink.clone()),
                checkpoint: crate::step::CheckpointSink::disconnected(),
                stop: StopSignal::never(),
            };
            let mut child_env = (*self.child_env).clone();
            child_env.insert(
                crate::subprocess::ENV_RESET.to_string(),
                target.part.clone(),
            );
            self.sink.emit(&Event::StepStart {
                step: spec.id.clone(),
                attempt: 1,
                builtin: argv
                    .first()
                    .is_some_and(|prog| crate::config::is_datalib_step(prog)),
            });
            let result =
                crate::subprocess::run_subprocess(argv, env, None, &child_env, 1, &ctx, &self.sink)
                    .await;
            let (status, error) = match &result {
                Ok(_) => (RunState::Succeeded, None),
                Err(e) => (RunState::Failed, Some(format!("{:#}", e.error))),
            };
            self.sink.emit(&Event::StepFinish {
                step: spec.id.clone(),
                status,
                error: error.clone(),
                exit_code: None,
                signal: None,
            });
            if let Some(error) = error {
                anyhow::bail!("reset {}:{}: {error}", target.step, target.part);
            }
        }
        Ok(())
    }
}

/// Terminal state of one step in one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    /// Ran to completion. `changed` = number of outputs whose version
    /// moved relative to the previous run.
    Succeeded {
        changed: usize,
    },
    /// In the runnable subgraph, but up to date: same inputs, same
    /// fingerprint as at its last success. Checked, and current.
    SkippedUpToDate,
    /// Outside the runnable subgraph — this run didn't ask for it, so
    /// it was never considered. Distinct from `SkippedUpToDate` on
    /// purpose: "not part of this run" and "checked, and current" are
    /// different facts, and a per-source sync makes the difference
    /// visible in the UI's task list.
    NotSelected,
    /// An upstream step failed (or was itself blocked); not invoked.
    Blocked {
        on: String,
    },
    Failed {
        kind: FailureKind,
    },
}

impl StepStatus {
    /// This status with its payload dropped — the part every reader
    /// outside the scheduler gets, and the only part that is written
    /// down. [`RunState`] owns the spelling.
    pub fn state(&self) -> RunState {
        match self {
            StepStatus::Succeeded { .. } => RunState::Succeeded,
            StepStatus::SkippedUpToDate => RunState::SkippedUpToDate,
            StepStatus::NotSelected => RunState::NotSelected,
            StepStatus::Blocked { .. } => RunState::Blocked,
            // A cancel is the one failure kind that is not a failure:
            // the person asked for it, and the step did as asked.
            StepStatus::Failed {
                kind: FailureKind::Cancelled,
            } => RunState::Stopped,
            StepStatus::Failed { .. } => RunState::Failed,
        }
    }
    pub fn is_ok(&self) -> bool {
        self.state().is_ok()
    }
}

#[derive(Debug, Clone)]
pub struct StepReport {
    pub id: String,
    pub status: StepStatus,
    /// Invocations this run (0 when skipped/blocked).
    pub attempts: u32,
    pub error: Option<String>,
    /// (artifact path, version now, changed this run)
    pub outputs: Vec<(String, String, bool)>,
}

#[derive(Debug, Clone)]
pub struct RunReport {
    /// One entry per step, in topological order.
    pub steps: Vec<StepReport>,
}

impl RunReport {
    pub fn step(&self, id: &str) -> &StepReport {
        self.steps
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no step {id:?} in report"))
    }
    pub fn all_ok(&self) -> bool {
        self.steps.iter().all(|s| s.status.is_ok())
    }
}

impl Runner {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish(
        &self,
        graph: &Graph,
        state: &mut DagState,
        status: &mut [Option<StepStatus>],
        i: usize,
        st: StepStatus,
        error: Option<String>,
        exit: Option<crate::step::Exit>,
        attempts: u32,
    ) {
        let id = &graph.steps[i].id;
        self.sink.emit(&Event::StepFinish {
            step: id.clone(),
            status: st.state(),
            error: error.clone(),
            exit_code: exit.and_then(|e| e.code),
            signal: exit.and_then(|e| e.signal),
        });
        let stamp = now_stamp();
        if let Some(run) = state.current_run.as_mut() {
            run.states
                .insert(id.clone(), st.state().as_str().to_string());
        }
        // `NotSelected` is a fact about this *run*, not about the step:
        // the run didn't ask for it, so nothing happened to it. Writing
        // that into `last_run` overwrote a real history — a step that
        // succeeded yesterday came back as "not selected", stamped with
        // the time of a run that never touched it, because every
        // per-source sync walks the whole graph to publish output
        // versions and reaches every step it isn't running.
        if st != StepStatus::NotSelected {
            let run_id = state
                .current_run
                .as_ref()
                .map(|r| r.run_id.clone())
                .unwrap_or_default();
            let entry = state.steps.entry(id.clone()).or_default();
            let last = entry.last_run.get_or_insert_with(|| LastRun {
                run_id: run_id.clone(),
                started_at: stamp.clone(),
                ..Default::default()
            });
            last.run_id = run_id;
            last.finished_at = Some(stamp.clone());
            last.status = st.state().as_str().to_string();
            last.attempts = attempts;
            last.error = error;
            if matches!(st.state(), RunState::Succeeded | RunState::SkippedUpToDate) {
                entry.last_success_at = Some(stamp);
            }
        }
        status[i] = Some(st);
    }
}

pub(crate) fn now_stamp() -> String {
    datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs()
}

/// Open a step's run record when the scheduler dispatches it, so a
/// reader can tell "running" from "not reached yet".
pub(crate) fn mark_running(state: &mut DagState, id: &StepId, stamp: &str) {
    if let Some(run) = state.current_run.as_mut() {
        run.states
            .insert(id.clone(), RunState::Running.as_str().to_string());
    }
    let run_id = state
        .current_run
        .as_ref()
        .map(|r| r.run_id.clone())
        .unwrap_or_default();
    state.steps.entry(id.clone()).or_default().last_run = Some(LastRun {
        run_id,
        started_at: stamp.to_string(),
        finished_at: None,
        status: String::new(),
        attempts: 0,
        error: None,
    });
}

pub(crate) fn step_summary(r: &StepReport) -> crate::events::StepSummary {
    let failure = match &r.status {
        StepStatus::Failed { kind } => Some(*kind),
        _ => None,
    };
    crate::events::StepSummary {
        step: r.id.clone(),
        status: r.status.state(),
        failure,
        attempts: r.attempts,
        error: r.error.clone(),
        outputs: r
            .outputs
            .iter()
            .map(|(path, version, changed)| crate::events::OutputSummary {
                path: path.clone(),
                version: version.clone(),
                changed: *changed,
            })
            .collect(),
    }
}

/// Resolve a step's reported (possibly empty) output states to
/// concrete `(path, version)` pairs for every declared output.
/// Reporting on an undeclared output is a contract violation.
///
/// The step's `fingerprint` is folded into every recorded version. A step
/// reports on its content and cannot know its own definition changed, so
/// without this a bumped `code_version` re-runs the step while leaving the
/// reported version identical — the tree is rebuilt and consumers skip it.
pub(crate) fn resolve_outputs(
    data_root: &std::path::Path,
    spec: &StepSpec,
    fingerprint: &str,
    reported: &[ArtifactState],
    sink: &dyn EventSink,
) -> Result<Vec<(String, String)>> {
    let output = spec.output();
    let mut by_path: BTreeMap<&str, &ArtifactState> = BTreeMap::new();
    for r in reported {
        if r.path.as_str() != output.as_str() {
            anyhow::bail!(
                "step {:?} reported on {:?}, but a step writes only the tree its id names ({:?})",
                spec.id,
                r.path.as_str(),
                output.as_str()
            );
        }
        by_path.insert(r.path.as_str(), r);
    }
    let path = output.as_str();
    let v = match by_path.get(path) {
        // The step vouched for a version: trust it. The mechanics
        // behind it (row-set hash, dolt commit, cursor hash) stay
        // the step's business.
        Some(a) => a.version.clone(),
        // Said nothing about its output: decide for ourselves.
        None => {
            sink.emit(&Event::Log {
                step: spec.id.clone(),
                level: crate::events::LogLevel::Info,
                msg: format!(
                    "reported no version for {path}; reading the whole tree to hash it. \
                     A version the step derives from what it wrote would be cheaper."
                ),
                ts: None,
                stream: None,
                target: None,
                thread: None,
                fields: None,
            });
            tree_version(&data_root.join(path))?
        }
    };
    Ok(vec![(path.to_string(), format!("{fingerprint}:{v}"))])
}

/// What each consumer has not read yet, per producer: the seals (a
/// checkpoint, or the producer finishing) recorded after the version the
/// consumer last read, with the rows each added. Published as the
/// consumer's `queued{from=<producer>}` metric — the S of USE, kept by
/// the runner from what producers say, so no store is opened to measure
/// it.
pub(crate) struct QueueLedger {
    /// Per consumer, per producer index: seals not yet consumed.
    pending: Vec<BTreeMap<usize, Vec<Seal>>>,
    /// Per consumer, per producer: the version last put on the queue,
    /// consumed or not. A producer re-announces a seal until its
    /// consumer has run (a checkpoint arriving mid-pass is dropped by
    /// design), and the repeat must not count its rows twice.
    last_seen: Vec<BTreeMap<usize, String>>,
}

/// One seal as the ledger keeps it: the qualified version, and the rows
/// it added if the producer counted.
type Seal = (String, Option<u64>);

impl QueueLedger {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            pending: vec![BTreeMap::new(); n],
            last_seen: vec![BTreeMap::new(); n],
        }
    }

    pub(crate) fn sealed(
        &mut self,
        graph: &Graph,
        producer: usize,
        qualified: &str,
        rows: Option<u64>,
        sink: &dyn EventSink,
    ) {
        for &c in &graph.dependents[producer] {
            if self.last_seen[c].get(&producer).map(String::as_str) == Some(qualified) {
                continue;
            }
            self.last_seen[c].insert(producer, qualified.to_string());
            self.pending[c]
                .entry(producer)
                .or_default()
                .push((qualified.to_string(), rows));
            self.publish(graph, c, producer, sink);
        }
    }

    /// A pass of `consumer` finished having read its inputs as of
    /// `consumed`: every seal up to the version it read is off the queue.
    /// A version not among the seals is one recorded before this ledger
    /// began (a previous run's), or a producer that finished without a
    /// row count; either way, if the producer is done there is nothing
    /// more to come.
    pub(crate) fn consumed(
        &mut self,
        graph: &Graph,
        consumer: usize,
        consumed: &HashMap<String, String>,
        producer_done: impl Fn(usize) -> bool,
        sink: &dyn EventSink,
    ) {
        let producers: Vec<usize> = self.pending[consumer].keys().copied().collect();
        for p in producers {
            let out = graph.steps[p].output().as_str().to_string();
            let Some(read) = consumed.get(&out) else {
                continue;
            };
            let seals = self.pending[consumer].entry(p).or_default();
            match seals.iter().position(|(v, _)| v == read) {
                Some(idx) => {
                    seals.drain(..=idx);
                }
                None if producer_done(p) => seals.clear(),
                None => {}
            }
            self.publish(graph, consumer, p, sink);
        }
    }

    /// A consumer found up to date read everything there was.
    pub(crate) fn cleared(&mut self, graph: &Graph, consumer: usize, sink: &dyn EventSink) {
        let producers: Vec<usize> = self.pending[consumer].keys().copied().collect();
        for p in producers {
            self.pending[consumer].insert(p, Vec::new());
            self.publish(graph, consumer, p, sink);
        }
    }

    fn publish(&self, graph: &Graph, consumer: usize, producer: usize, sink: &dyn EventSink) {
        let value: u64 = self.pending[consumer]
            .get(&producer)
            .map(|seals| seals.iter().map(|(_, n)| n.unwrap_or(0)).sum())
            .unwrap_or(0);
        sink.emit(&Event::Metric {
            step: graph.steps[consumer].id.clone(),
            name: "queued".to_string(),
            labels: BTreeMap::from([("from".to_string(), graph.steps[producer].id.clone())]),
            value: value as i64,
        });
    }
}

/// A fresh run id. UUID v7, so ids sort in the order the runs started
/// while still being unique by construction — two runs pinned to the
/// same `--now` (the tests do this) get different ids.
pub fn new_run_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub(crate) async fn invoke_with_retry(
    run: &StepRun,
    ctx: StepCtx,
    retry: &RetryPolicy,
    sink: &Arc<dyn EventSink>,
    child_env: &BTreeMap<String, String>,
) -> (u32, Result<StepOutcome, StepError>) {
    let builtin = match run {
        StepRun::Subprocess { argv, .. } => argv
            .first()
            .is_some_and(|prog| crate::config::is_datalib_step(prog)),
        StepRun::InProcess(_) => false,
    };
    let mut attempt = 1u32;
    loop {
        sink.emit(&Event::StepStart {
            step: ctx.step_id.clone(),
            attempt,
            builtin,
        });
        let res = match run {
            StepRun::InProcess(f) => f(ctx.clone()).await,
            StepRun::Subprocess { argv, env, params } => {
                crate::subprocess::run_subprocess(
                    argv,
                    env,
                    params.as_deref(),
                    child_env,
                    attempt,
                    &ctx,
                    sink,
                )
                .await
            }
        };
        match res {
            Ok(outcome) => return (attempt, Ok(outcome)),
            Err(e) => {
                if attempt >= retry.max_attempts(e.kind) {
                    return (attempt, Err(e));
                }
                let backoff = retry.backoff * 2u32.saturating_pow(attempt - 1);
                if !backoff.is_zero() {
                    tokio::time::sleep(backoff).await;
                }
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::step::StepOutcome;
    use crate::version::UNKNOWN;

    /// Records every event for assertions.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Event>>);
    impl EventSink for Recorder {
        fn emit(&self, event: &Event) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn runner(root: &std::path::Path) -> Runner {
        Runner::new(root).retry(RetryPolicy {
            backoff: Duration::ZERO,
            ..RetryPolicy::default()
        })
    }

    fn download(name: &str, content: Arc<Mutex<String>>, runs: Arc<AtomicU32>) -> StepSpec {
        StepSpec::new(
            format!("{name}/raw"),
            StepRun::in_process(move |ctx: StepCtx| {
                let content = content.clone();
                let runs = runs.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    let new = content.lock().unwrap().clone();
                    std::fs::write(dir.join("data.txt"), &new).unwrap();
                    ctx.progress.set_length(Some(1));
                    ctx.progress.inc(1);
                    let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                    // Stands in for a real download reporting its raw
                    // store's dolt commit: derived from what was
                    // written, so an unchanged poll reports the same
                    // string without the step having to remember it.
                    let version = blake3::hash(new.as_bytes()).to_hex().to_string();
                    Ok(StepOutcome {
                        outputs: vec![ArtifactState::versioned(&pat, version)],
                        exit: None,
                    })
                }
            }),
        )
    }

    fn render(name: &str, runs: Arc<AtomicU32>) -> StepSpec {
        let inp = format!("{name}/raw");
        StepSpec::new(
            format!("{name}/rendered_md"),
            StepRun::in_process(move |ctx: StepCtx| {
                let runs = runs.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let src = ctx.path(&ctx.inputs[0]).join("data.txt");
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    let text = std::fs::read_to_string(&src)
                        .map_err(|e| StepError::new(FailureKind::Data, e))?;
                    std::fs::write(dir.join("data.md"), text.to_uppercase()).unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        )
        .input(&inp)
    }

    /// The fan-in index step: concatenates every input tree's files
    /// into `unified_index/grid/index.txt`. Its inputs are named, not
    /// globbed. Counts invocations and remembers `changed_inputs`.
    fn index(runs: Arc<AtomicU32>, seen_changed: Arc<Mutex<Vec<String>>>) -> StepSpec {
        StepSpec::new(
            "unified_index/grid",
            StepRun::in_process(move |ctx: StepCtx| {
                let runs = runs.clone();
                let seen_changed = seen_changed.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    *seen_changed.lock().unwrap() = ctx
                        .changed_inputs
                        .iter()
                        .map(|a| a.as_str().to_string())
                        .collect();
                    let mut combined = String::new();
                    let mut inputs = ctx.inputs.clone();
                    inputs.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                    for a in &inputs {
                        let mut files: Vec<_> = walkdir::WalkDir::new(ctx.path(a))
                            .into_iter()
                            .filter_map(|e| e.ok())
                            .filter(|e| e.file_type().is_file())
                            .map(|e| e.path().to_path_buf())
                            .collect();
                        files.sort();
                        for f in files {
                            combined.push_str(&std::fs::read_to_string(f).unwrap());
                            combined.push('\n');
                        }
                    }
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join("index.txt"), combined).unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        )
        .input("email/rendered_md")
        .input("slack/rendered_md")
    }

    struct Fixture {
        root: tempfile::TempDir,
        slack_content: Arc<Mutex<String>>,
        email_content: Arc<Mutex<String>>,
        runs: BTreeMap<&'static str, Arc<AtomicU32>>,
        index_changed_inputs: Arc<Mutex<Vec<String>>>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
                slack_content: Arc::new(Mutex::new("slack v1".to_string())),
                email_content: Arc::new(Mutex::new("email v1".to_string())),
                runs: [
                    "slack/raw",
                    "email/raw",
                    "slack/rendered_md",
                    "email/rendered_md",
                    "unified_index/grid",
                ]
                .into_iter()
                .map(|k| (k, Arc::new(AtomicU32::new(0))))
                .collect(),
                index_changed_inputs: Arc::default(),
            }
        }

        fn graph(&self) -> Graph {
            Graph::build(vec![
                download(
                    "slack",
                    self.slack_content.clone(),
                    self.runs["slack/raw"].clone(),
                ),
                render("slack", self.runs["slack/rendered_md"].clone()),
                download(
                    "email",
                    self.email_content.clone(),
                    self.runs["email/raw"].clone(),
                ),
                render("email", self.runs["email/rendered_md"].clone()),
                index(
                    self.runs["unified_index/grid"].clone(),
                    self.index_changed_inputs.clone(),
                ),
            ])
            .unwrap()
        }

        fn run_count(&self, id: &str) -> u32 {
            self.runs[id].load(Ordering::SeqCst)
        }
    }

    // ── streaming dispatch ────────────────────────────────────────────
    //
    // Synthetic steps only: a "producer" that checkpoints on demand and a
    // "consumer" that counts its passes. No store, no provider, no
    // subprocess — the scheduler's rules are what is under test, and
    // anything real would make a failure ambiguous between the two.

    /// A producer that seals `checkpoints` times, re-announcing each seal
    /// until a consumer pass has actually observed it.
    ///
    /// The re-announcing is not test scaffolding — it is what a real
    /// producer does, and it is required by the design: a checkpoint that
    /// arrives while the consumer is already running is **dropped**, so a
    /// producer that announced once and waited would wait forever. Modeling
    /// that is what makes these assertions deterministic instead of timing
    /// dependent.
    fn streaming_producer(name: &str, batches: u32, passes: Arc<AtomicU32>) -> StepSpec {
        StepSpec::new(
            format!("{name}/raw"),
            StepRun::in_process(move |ctx: StepCtx| {
                let passes = passes.clone();
                async move {
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    for k in 0..batches {
                        std::fs::write(dir.join("data.txt"), format!("batch{k}")).unwrap();
                        while passes.load(Ordering::SeqCst) < k + 1 {
                            ctx.checkpoint(&format!("v{k}"));
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                    }
                    std::fs::write(dir.join("data.txt"), "final").unwrap();
                    let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                    Ok(StepOutcome {
                        outputs: vec![ArtifactState::versioned(&pat, "final")],
                        exit: None,
                    })
                }
            }),
        )
        .streams_output()
    }

    /// Counts its passes; that count is the only thing the tests assert on.
    fn counting_consumer(id: &str, input: &str, passes: Arc<AtomicU32>) -> StepSpec {
        StepSpec::new(
            id.to_string(),
            StepRun::in_process(move |ctx: StepCtx| {
                let passes = passes.clone();
                async move {
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join("out.txt"), "x").unwrap();
                    passes.fetch_add(1, Ordering::SeqCst);
                    Ok(StepOutcome::default())
                }
            }),
        )
        .input(input)
    }

    /// The S of USE, from what the producer says: each seal's rows go on
    /// the consumer's queue, and each pass the consumer completes takes
    /// off everything up to the version it read. The sequence is the
    /// assertion — it has to rise on a seal and fall to zero once the
    /// consumer has caught up, and end at zero when the run does.
    #[tokio::test]
    async fn a_consumers_queue_rises_on_seals_and_drains_as_it_reads() {
        let root = tempfile::tempdir().unwrap();
        let passes = Arc::new(AtomicU32::new(0));
        // Like `streaming_producer`, but each seal says it added 10 rows,
        // and the final outcome says its last segment added 5.
        let producer = {
            let passes = passes.clone();
            StepSpec::new(
                "slack/raw",
                StepRun::in_process(move |ctx: StepCtx| {
                    let passes = passes.clone();
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        for k in 0..2u32 {
                            std::fs::write(dir.join("data.txt"), format!("batch{k}")).unwrap();
                            while passes.load(Ordering::SeqCst) < k + 1 {
                                ctx.checkpoint_rows(&format!("v{k}"), 10);
                                tokio::time::sleep(Duration::from_millis(2)).await;
                            }
                        }
                        std::fs::write(dir.join("data.txt"), "final").unwrap();
                        let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                        Ok(StepOutcome {
                            outputs: vec![ArtifactState::versioned(&pat, "final").with_rows(5)],
                            exit: None,
                        })
                    }
                }),
            )
            .streams_output()
        };
        let graph = Graph::build(vec![
            producer,
            counting_consumer("unified_index/grid", "slack/raw", passes.clone()),
        ])
        .unwrap();

        let rec = Arc::new(Recorder::default());
        let mut r = runner(root.path());
        r.sink = rec.clone();
        let report = r.run(&graph).await.unwrap();
        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");

        let queued: Vec<i64> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::Metric {
                    step,
                    name,
                    labels,
                    value,
                } if step == "unified_index/grid"
                    && name == "queued"
                    && labels.get("from").map(String::as_str) == Some("slack/raw") =>
                {
                    Some(*value)
                }
                _ => None,
            })
            .collect();
        // A seal re-announced while the consumer is mid-pass adds the
        // same version again only as a metric refresh, so the exact
        // sequence has repeats; what must hold is the shape.
        assert!(!queued.is_empty(), "no queued metric was published");
        assert!(
            queued.contains(&10),
            "a seal of 10 rows never showed: {queued:?}"
        );
        assert!(
            queued.contains(&5),
            "the outcome's last segment of 5 rows never showed: {queued:?}"
        );
        assert_eq!(
            *queued.last().unwrap(),
            0,
            "the run ended with rows unread: {queued:?}"
        );
        assert!(
            queued.windows(2).any(|w| w[0] > 0 && w[1] == 0),
            "the queue never drained after a pass: {queued:?}"
        );
    }

    /// The point of the whole feature: a consumer runs *before* its
    /// producer has finished, once per checkpoint, plus a final pass.
    #[tokio::test]
    async fn a_checkpoint_dispatches_the_consumer_before_the_producer_finishes() {
        let root = tempfile::tempdir().unwrap();
        let passes = Arc::new(AtomicU32::new(0));
        let graph = Graph::build(vec![
            streaming_producer("slack", 3, passes.clone()),
            counting_consumer("unified_index/grid", "slack/raw", passes.clone()),
        ])
        .unwrap();

        let rec = Arc::new(Recorder::default());
        let mut r = runner(root.path());
        r.sink = rec.clone();
        let report = r.run(&graph).await.unwrap();

        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        // Three checkpoints each dispatched a pass. The fourth is the
        // final one, which runs because the producer's real output
        // version ("final") differs from the last checkpoint's.
        assert_eq!(
            passes.load(Ordering::SeqCst),
            4,
            "expected one pass per checkpoint plus the final pass"
        );
        let checkpoints = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Event::Checkpoint { .. }))
            .count();
        assert_eq!(checkpoints, 3, "every checkpoint reaches the event stream");
    }

    /// A chain of three. The middle step's early pass writes new output,
    /// and that output has to reach the last step while the first is
    /// still running: an early pass that completes is a seal for the next
    /// hop, exactly as a checkpoint is. Without that a chain streams one
    /// hop only -- the first consumer runs early, but nothing downstream of
    /// it moves until it goes terminal, which is after the producer has
    /// finished. For `ingest -> render -> grid_index` that meant no row
    /// reached the grid until the download was over.
    #[tokio::test]
    async fn an_early_pass_that_produced_output_dispatches_its_own_consumers() {
        let root = tempfile::tempdir().unwrap();
        let sink_passes = Arc::new(AtomicU32::new(0));
        // How many times the sink had run by the time the producer
        // finished: the number this test is about.
        let sink_passes_at_producer_end = Arc::new(AtomicU32::new(0));

        // Two batches. After sealing each, wait (bounded, so a regression
        // fails rather than hangs) for the sink to have caught up with it.
        let producer = {
            let sink_passes = sink_passes.clone();
            let at_end = sink_passes_at_producer_end.clone();
            StepSpec::new(
                "slack/raw",
                StepRun::in_process(move |ctx: StepCtx| {
                    let sink_passes = sink_passes.clone();
                    let at_end = at_end.clone();
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        for k in 0..2u32 {
                            std::fs::write(dir.join("data.txt"), format!("batch{k}")).unwrap();
                            let deadline = std::time::Instant::now() + Duration::from_secs(2);
                            while sink_passes.load(Ordering::SeqCst) < k + 1
                                && std::time::Instant::now() < deadline
                            {
                                ctx.checkpoint(&format!("v{k}"));
                                tokio::time::sleep(Duration::from_millis(2)).await;
                            }
                        }
                        at_end.store(sink_passes.load(Ordering::SeqCst), Ordering::SeqCst);
                        std::fs::write(dir.join("data.txt"), "final").unwrap();
                        let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                        Ok(StepOutcome {
                            outputs: vec![ArtifactState::versioned(&pat, "final")],
                            exit: None,
                        })
                    }
                }),
            )
            .streams_output()
        };
        // Reads whatever the producer has written and reports a version
        // derived from it, so an early pass over a new batch is new output.
        let middle = StepSpec::new(
            "slack/rendered",
            StepRun::in_process(move |ctx: StepCtx| async move {
                let dir = ctx.path_str(&ctx.step_id);
                std::fs::create_dir_all(&dir).unwrap();
                let read = std::fs::read_to_string(ctx.path_str("slack/raw").join("data.txt"))
                    .unwrap_or_default();
                std::fs::write(dir.join("out.txt"), &read).unwrap();
                let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                let version = blake3::hash(read.as_bytes()).to_hex().to_string();
                Ok(StepOutcome {
                    outputs: vec![ArtifactState::versioned(&pat, version)],
                    exit: None,
                })
            }),
        )
        .input("slack/raw")
        .streams_output();
        let graph = Graph::build(vec![
            producer,
            middle,
            counting_consumer("unified_index/grid", "slack/rendered", sink_passes.clone()),
        ])
        .unwrap();

        let report = runner(root.path()).run(&graph).await.unwrap();
        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        assert!(
            sink_passes_at_producer_end.load(Ordering::SeqCst) >= 2,
            "the last step ran {} time(s) before the first finished; \
             a middle step's early pass must wake its own consumers",
            sink_passes_at_producer_end.load(Ordering::SeqCst)
        );
    }

    /// Without the capability nothing streams, and the consumer runs once.
    /// This is the guard that keeps a sink which cannot be read mid-write
    /// from being read mid-write.
    #[tokio::test]
    async fn a_producer_that_does_not_declare_streams_output_dispatches_nobody_early() {
        let root = tempfile::tempdir().unwrap();
        let passes = Arc::new(AtomicU32::new(0));
        // Same producer, capability removed. One batch only: with no early
        // dispatch its re-announce loop would never see a pass, so the
        // producer must not depend on one.
        let mut producer = StepSpec::new(
            "slack/raw",
            StepRun::in_process(move |ctx: StepCtx| async move {
                let dir = ctx.path_str(&ctx.step_id);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("data.txt"), "batch0").unwrap();
                for k in 0..3 {
                    ctx.checkpoint(&format!("v{k}"));
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                Ok(StepOutcome {
                    outputs: vec![ArtifactState::versioned(&pat, "final")],
                    exit: None,
                })
            }),
        );
        producer.streams_output = false;
        let graph = Graph::build(vec![
            producer,
            counting_consumer("unified_index/grid", "slack/raw", passes.clone()),
        ])
        .unwrap();

        let rec = Arc::new(Recorder::default());
        let mut r = runner(root.path());
        r.sink = rec.clone();
        let report = r.run(&graph).await.unwrap();

        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "no capability means no early dispatch: one final pass only"
        );
        // And it says why, rather than ignoring the checkpoints silently.
        let warned = rec.0.lock().unwrap().iter().any(|e| {
            matches!(e, Event::Log { msg, level: crate::events::LogLevel::Warn, .. }
                if msg.contains("streams_output"))
        });
        assert!(
            warned,
            "a checkpoint on a non-streaming sink must be reported"
        );
    }

    /// At most one instance of a step in flight. `grid_index` is a fan-in
    /// that every source pokes, so two producers checkpointing at once is
    /// the ordinary case — and a second dispatch would put two writers on
    /// one tree, which single-writer-per-file forbids everywhere here.
    #[tokio::test]
    async fn two_producers_checkpointing_never_run_the_consumer_twice_at_once() {
        let root = tempfile::tempdir().unwrap();
        let concurrent = Arc::new(AtomicU32::new(0));
        let max_seen = Arc::new(AtomicU32::new(0));
        let passes = Arc::new(AtomicU32::new(0));

        let consumer = {
            let concurrent = concurrent.clone();
            let max_seen = max_seen.clone();
            let passes = passes.clone();
            StepSpec::new(
                "unified_index/grid",
                StepRun::in_process(move |ctx: StepCtx| {
                    let (concurrent, max_seen, passes) =
                        (concurrent.clone(), max_seen.clone(), passes.clone());
                    async move {
                        let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        passes.fetch_add(1, Ordering::SeqCst);
                        // Long enough that a second dispatch would overlap.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("out.txt"), "x").unwrap();
                        concurrent.fetch_sub(1, Ordering::SeqCst);
                        Ok(StepOutcome::default())
                    }
                }),
            )
            .input("slack/raw")
            .input("email/raw")
        };

        // Both producers checkpoint repeatedly with no handshake, so the
        // consumer is poked far more often than it can run.
        let chatty = |name: &str| {
            StepSpec::new(
                format!("{name}/raw"),
                StepRun::in_process(move |ctx: StepCtx| async move {
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    for k in 0..25 {
                        std::fs::write(dir.join("data.txt"), format!("b{k}")).unwrap();
                        ctx.checkpoint(&format!("v{k}"));
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                    Ok(StepOutcome {
                        outputs: vec![ArtifactState::versioned(&pat, "final")],
                        exit: None,
                    })
                }),
            )
            .streams_output()
        };

        let graph = Graph::build(vec![chatty("slack"), chatty("email"), consumer]).unwrap();
        let report = runner(root.path()).run(&graph).await.unwrap();

        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "two instances of one step ran at once; single-writer-per-file forbids it"
        );
        // Checkpoints arriving while it runs are dropped, not queued, so
        // the pass count stays far below the 50 checkpoints emitted.
        let n = passes.load(Ordering::SeqCst);
        assert!(
            n < 50,
            "checkpoints must be dropped while the consumer runs, not queued (ran {n} times)"
        );
    }

    /// A seal that lands while the fan-in is mid-pass is not queued -- a
    /// second instance would be a second writer on one tree -- but it is
    /// not forgotten either: one more pass follows as soon as the current
    /// one lands, and reads what the seal announced. Without that, rows
    /// sealed by one source while the index was busy with another sat
    /// there until the next seal from anyone, or the end of the run.
    ///
    /// Two producers, forced to interleave: `email` seals only once the
    /// fan-in is inside its pass over `slack`, and the pass holds until
    /// that seal has been sent. Both producers then stay alive until the
    /// fan-in has run again -- bounded, so a regression fails rather than
    /// hangs -- and record how many passes it had made when they finished.
    #[tokio::test]
    async fn a_seal_arriving_mid_pass_gets_one_more_pass_after_it() {
        use std::sync::atomic::AtomicBool;
        let root = tempfile::tempdir().unwrap();
        let passes = Arc::new(AtomicU32::new(0));
        let concurrent = Arc::new(AtomicU32::new(0));
        let max_seen = Arc::new(AtomicU32::new(0));
        let in_pass = Arc::new(AtomicBool::new(false));
        let email_sealed = Arc::new(AtomicBool::new(false));
        // The fewest passes any producer had seen when it finished. The
        // minimum, because a producer giving up its wait and finishing is
        // itself a seal that starts a pass -- the *other* producer would
        // then see two, and the maximum would pass for the wrong reason.
        let passes_at_producers_end = Arc::new(AtomicU32::new(u32::MAX));

        async fn until(flag: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !flag() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }

        let producer = |name: &str, seal_when: Arc<AtomicBool>, sealed: Arc<AtomicBool>| {
            let passes = passes.clone();
            let at_end = passes_at_producers_end.clone();
            StepSpec::new(
                format!("{name}/rendered_md"),
                StepRun::in_process(move |ctx: StepCtx| {
                    let (seal_when, sealed, passes, at_end) = (
                        seal_when.clone(),
                        sealed.clone(),
                        passes.clone(),
                        at_end.clone(),
                    );
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("data.md"), "rows").unwrap();
                        until(|| seal_when.load(Ordering::SeqCst)).await;
                        ctx.checkpoint("v1");
                        sealed.store(true, Ordering::SeqCst);
                        until(|| passes.load(Ordering::SeqCst) >= 2).await;
                        at_end.fetch_min(passes.load(Ordering::SeqCst), Ordering::SeqCst);
                        let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                        Ok(StepOutcome {
                            outputs: vec![ArtifactState::versioned(&pat, "final")],
                            exit: None,
                        })
                    }
                }),
            )
            .streams_output()
        };
        let always = Arc::new(AtomicBool::new(true));
        let slack = producer("slack", always, Arc::new(AtomicBool::new(false)));
        let email = producer("email", in_pass.clone(), email_sealed.clone());

        let grid_index = {
            let (passes, concurrent, max_seen, in_pass, email_sealed) = (
                passes.clone(),
                concurrent.clone(),
                max_seen.clone(),
                in_pass.clone(),
                email_sealed.clone(),
            );
            StepSpec::new(
                "unified_index/grid",
                StepRun::in_process(move |ctx: StepCtx| {
                    let (passes, concurrent, max_seen, in_pass, email_sealed) = (
                        passes.clone(),
                        concurrent.clone(),
                        max_seen.clone(),
                        in_pass.clone(),
                        email_sealed.clone(),
                    );
                    async move {
                        let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        // The first pass holds until email has sealed into
                        // it, so that seal is guaranteed to land mid-pass.
                        if passes.load(Ordering::SeqCst) == 0 {
                            in_pass.store(true, Ordering::SeqCst);
                            until(|| email_sealed.load(Ordering::SeqCst)).await;
                            // The scheduler reads the seal off a channel;
                            // give it a moment to have done so before this
                            // pass lands.
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("index.txt"), "x").unwrap();
                        passes.fetch_add(1, Ordering::SeqCst);
                        concurrent.fetch_sub(1, Ordering::SeqCst);
                        Ok(StepOutcome::default())
                    }
                }),
            )
            .input("slack/rendered_md")
            .input("email/rendered_md")
        };

        let graph = Graph::build(vec![slack, email, grid_index]).unwrap();
        let rec = Arc::new(Recorder::default());
        let mut r = runner(root.path());
        r.sink = rec.clone();
        let report = tokio::time::timeout(Duration::from_secs(15), r.run(&graph))
            .await
            .expect("the run must terminate")
            .unwrap();
        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        let trace: Vec<String> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::StepStart { step, .. } => Some(format!("start {step}")),
                Event::StepFinish { step, .. } => Some(format!("finish {step}")),
                Event::Checkpoint { step, version, .. } => Some(format!("seal {step} {version}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "two instances of the fan-in ran at once"
        );
        assert!(
            passes_at_producers_end.load(Ordering::SeqCst) >= 2,
            "email's seal landed while the fan-in was busy with slack and was then \
             forgotten: only {} pass(es) had run when the producers finished; \
             events: {trace:#?}",
            passes_at_producers_end.load(Ordering::SeqCst)
        );
    }

    /// Many seals landing while the fan-in is mid-pass collapse into
    /// *one* follow-up pass, and that pass reads all of them. Each
    /// producer seals once to get the fan-in started, then five more
    /// times while it holds that first pass; when it lands, exactly one
    /// more pass runs before the producers finish -- not one per seal,
    /// and not zero -- and the queue the runner keeps per producer goes
    /// from everything sealed mid-pass straight to empty.
    #[tokio::test]
    async fn seals_landing_mid_pass_collapse_into_one_follow_up_pass() {
        use std::sync::atomic::AtomicBool;
        const SEALS_EACH: u32 = 5;
        const ROWS_EACH: u64 = 10;
        let root = tempfile::tempdir().unwrap();
        let passes = Arc::new(AtomicU32::new(0));
        let concurrent = Arc::new(AtomicU32::new(0));
        let max_seen = Arc::new(AtomicU32::new(0));
        let in_pass = Arc::new(AtomicBool::new(false));
        let sealed = Arc::new(AtomicU32::new(0));
        // Each producer's reading of the pass count as it finished, after
        // giving a wrongly-owed third pass time to show up.
        let slack_saw = Arc::new(AtomicU32::new(0));
        let email_saw = Arc::new(AtomicU32::new(0));

        async fn until(flag: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !flag() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }

        let producer = |name: &str, saw: Arc<AtomicU32>| {
            let (passes, in_pass, sealed) = (passes.clone(), in_pass.clone(), sealed.clone());
            StepSpec::new(
                format!("{name}/rendered_md"),
                StepRun::in_process(move |ctx: StepCtx| {
                    let (passes, in_pass, sealed, saw) =
                        (passes.clone(), in_pass.clone(), sealed.clone(), saw.clone());
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("data.md"), "rows").unwrap();
                        // One seal to start the fan-in; every other lands
                        // while it is busy.
                        ctx.checkpoint_rows("v0", ROWS_EACH);
                        until(|| in_pass.load(Ordering::SeqCst)).await;
                        for k in 1..=SEALS_EACH {
                            ctx.checkpoint_rows(&format!("v{k}"), ROWS_EACH);
                            sealed.fetch_add(1, Ordering::SeqCst);
                        }
                        until(|| passes.load(Ordering::SeqCst) >= 2).await;
                        // A pass wrongly owed per seal would be dispatched
                        // the moment the follow-up lands; give it room.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        saw.store(passes.load(Ordering::SeqCst), Ordering::SeqCst);
                        let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                        Ok(StepOutcome {
                            outputs: vec![ArtifactState::versioned(&pat, "final")],
                            exit: None,
                        })
                    }
                }),
            )
            .streams_output()
        };

        let grid_index = {
            let (passes, concurrent, max_seen, in_pass, sealed) = (
                passes.clone(),
                concurrent.clone(),
                max_seen.clone(),
                in_pass.clone(),
                sealed.clone(),
            );
            StepSpec::new(
                "unified_index/grid",
                StepRun::in_process(move |ctx: StepCtx| {
                    let (passes, concurrent, max_seen, in_pass, sealed) = (
                        passes.clone(),
                        concurrent.clone(),
                        max_seen.clone(),
                        in_pass.clone(),
                        sealed.clone(),
                    );
                    async move {
                        let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        if passes.load(Ordering::SeqCst) == 0 {
                            in_pass.store(true, Ordering::SeqCst);
                            until(|| sealed.load(Ordering::SeqCst) >= 2 * SEALS_EACH).await;
                            // The seals cross a channel; let the scheduler
                            // read every one before this pass lands.
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("index.txt"), "x").unwrap();
                        passes.fetch_add(1, Ordering::SeqCst);
                        concurrent.fetch_sub(1, Ordering::SeqCst);
                        Ok(StepOutcome::default())
                    }
                }),
            )
            .input("slack/rendered_md")
            .input("email/rendered_md")
        };

        let graph = Graph::build(vec![
            producer("slack", slack_saw.clone()),
            producer("email", email_saw.clone()),
            grid_index,
        ])
        .unwrap();
        let rec = Arc::new(Recorder::default());
        let mut r = runner(root.path());
        r.sink = rec.clone();
        let report = tokio::time::timeout(Duration::from_secs(15), r.run(&graph))
            .await
            .expect("the run must terminate")
            .unwrap();
        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "two fan-in passes overlapped"
        );
        for (name, saw) in [("slack", &slack_saw), ("email", &email_saw)] {
            assert_eq!(
                saw.load(Ordering::SeqCst),
                2,
                "{name}: {SEALS_EACH} seals each from two producers, landing mid-pass, \
                 must wake the fan-in exactly once more",
            );
        }

        // The queue the runner keeps per producer: every seal piles on
        // while the fan-in is busy, and the one follow-up pass takes the
        // whole pile off in a single step. (The first pass landing may
        // take its own starting seal off first, for the producer whose
        // seal dispatched it -- so the pile just before the drain is
        // at least the mid-pass seals, not exactly all of them.)
        let events = rec.0.lock().unwrap();
        for producer in ["slack/rendered_md", "email/rendered_md"] {
            let mut queued: Vec<i64> = events
                .iter()
                .filter_map(|e| match e {
                    Event::Metric {
                        step,
                        name,
                        labels,
                        value,
                    } if step == "unified_index/grid"
                        && name == "queued"
                        && labels.get("from").map(String::as_str) == Some(producer) =>
                    {
                        Some(*value)
                    }
                    _ => None,
                })
                .collect();
            queued.dedup();
            let mid_pass = (SEALS_EACH as u64 * ROWS_EACH) as i64;
            assert!(
                queued.contains(&(mid_pass + ROWS_EACH as i64)),
                "{producer}: the queue never held the starting seal plus every mid-pass one: {queued:?}"
            );
            let n = queued.len();
            assert!(
                n >= 2 && queued[n - 1] == 0 && queued[n - 2] >= mid_pass,
                "{producer}: one pass must drain the whole queue in one step, not one seal of it: {queued:?}"
            );
        }
    }

    /// A streaming pass must not wait behind `parallelism`. With every
    /// ordinary slot occupied by producers, a consumer that competed for
    /// those slots would never run — losing the whole point of the
    /// feature in exactly the case it exists for.
    #[tokio::test]
    async fn a_streaming_pass_runs_even_when_every_ordinary_slot_is_busy() {
        let root = tempfile::tempdir().unwrap();
        let passes = Arc::new(AtomicU32::new(0));

        // One producer streams; the others just occupy slots until the
        // streaming consumer has run.
        let mut specs = vec![streaming_producer("slack", 1, passes.clone())];
        for name in ["email", "github", "notion"] {
            // Their own signal, not the producer's semaphore: the producer
            // *consumes* a permit, so sharing one made this a race rather
            // than a wait.
            let ran = passes.clone();
            specs.push(StepSpec::new(
                format!("{name}/raw"),
                StepRun::in_process(move |ctx: StepCtx| {
                    let ran = ran.clone();
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("data.txt"), "x").unwrap();
                        // Hold the ordinary slot until the streaming pass
                        // has actually run. If streaming competed for these
                        // slots, this would never return.
                        while ran.load(Ordering::SeqCst) == 0 {
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                        Ok(StepOutcome::default())
                    }
                }),
            ));
        }
        specs.push(counting_consumer(
            "unified_index/grid",
            "slack/raw",
            passes.clone(),
        ));
        let graph = Graph::build(specs).unwrap();

        let mut r = runner(root.path());
        r.budgets = Budgets::from_parallelism(4); // exactly the number of producers
        let report = tokio::time::timeout(Duration::from_secs(10), r.run(&graph))
            .await
            .expect("a streaming pass competing for ordinary slots would deadlock here")
            .unwrap();

        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        assert!(
            passes.load(Ordering::SeqCst) >= 2,
            "the consumer must have run early, while all four slots were held"
        );
    }

    /// The `render -> grid_index` shape specifically: several producers
    /// that both checkpoint *and* finish at about the same moment.
    ///
    /// Two different code paths can make the fan-in ready — a checkpoint
    /// (streaming dispatch) and `release_dependents` when the last producer
    /// goes terminal — and they can fire in the same scheduler iteration.
    /// The consumer writes one doltlite store, so two instances of it is a
    /// second writer on one file, which this repo forbids everywhere.
    ///
    /// The barrier makes the collision reliable rather than hoping for it:
    /// neither producer returns until both have reached the end.
    #[tokio::test]
    async fn producers_finishing_together_still_run_the_fan_in_one_at_a_time() {
        let root = tempfile::tempdir().unwrap();
        let concurrent = Arc::new(AtomicU32::new(0));
        let max_seen = Arc::new(AtomicU32::new(0));
        let passes = Arc::new(AtomicU32::new(0));
        // 2 producers; `wait()` releases only when both have arrived.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let render = |name: &str, barrier: Arc<tokio::sync::Barrier>| {
            StepSpec::new(
                format!("{name}/rendered_md"),
                StepRun::in_process(move |ctx: StepCtx| {
                    let barrier = barrier.clone();
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("data.md"), "rows").unwrap();
                        // Seal once, then finish in lockstep with the other
                        // producer so the checkpoint-driven dispatch and the
                        // deps-satisfied dispatch race each other.
                        ctx.checkpoint("v1");
                        barrier.wait().await;
                        let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                        Ok(StepOutcome {
                            outputs: vec![ArtifactState::versioned(&pat, "final")],
                            exit: None,
                        })
                    }
                }),
            )
            .streams_output()
        };

        let grid_index = {
            let (concurrent, max_seen, passes) =
                (concurrent.clone(), max_seen.clone(), passes.clone());
            StepSpec::new(
                "unified_index/grid",
                StepRun::in_process(move |ctx: StepCtx| {
                    let (concurrent, max_seen, passes) =
                        (concurrent.clone(), max_seen.clone(), passes.clone());
                    async move {
                        let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        passes.fetch_add(1, Ordering::SeqCst);
                        // Long enough that an overlapping dispatch overlaps.
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("index.txt"), "x").unwrap();
                        concurrent.fetch_sub(1, Ordering::SeqCst);
                        Ok(StepOutcome::default())
                    }
                }),
            )
            .input("slack/rendered_md")
            .input("email/rendered_md")
        };

        let graph = Graph::build(vec![
            render("slack", barrier.clone()),
            render("email", barrier),
            grid_index,
        ])
        .unwrap();
        let report = tokio::time::timeout(Duration::from_secs(10), runner(root.path()).run(&graph))
            .await
            .expect("the fan-in must not deadlock when both producers finish together")
            .unwrap();

        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "two instances of the fan-in ran at once; it writes one store, \
             and single-writer-per-file forbids that"
        );
        // It has to actually run: the final pass sees "final", which no
        // checkpoint reported, so the index is never left stale.
        assert!(
            passes.load(Ordering::SeqCst) >= 1,
            "the fan-in never ran at all"
        );
        let idx = report
            .steps
            .iter()
            .find(|s| s.id == "unified_index/grid")
            .unwrap();
        assert!(idx.status.is_ok(), "{:?}", idx.status);
        // And the pass that followed the first one read *both* finals:
        // the second producer's finish arrived while the fan-in was busy
        // with the first's, and held-back is not the same as dropped.
        let st = DagState::load(root.path()).unwrap();
        let read = &st.steps["unified_index/grid"].input_versions;
        for producer in ["slack/rendered_md", "email/rendered_md"] {
            assert!(
                read.get(producer).is_some_and(|v| v.ends_with(":final")),
                "the fan-in's last pass never read {producer}'s final output: {read:?}"
            );
        }
    }

    /// A producer *finishing* should start the fan-in, not just a
    /// checkpoint from one.
    ///
    /// With several renders feeding `grid_index`, the fast ones are done
    /// long before the slow one. Waiting for `remaining_deps` to reach zero
    /// means none of their documents reach the grid until the slowest
    /// source finishes — which for a mirror with one big source is nearly
    /// the whole run.
    ///
    /// Neither producer checkpoints here, deliberately: this is about the
    /// completion path on its own. A render that finishes in under the
    /// cadence never checkpoints at all, so this is the common case rather
    /// than an exotic one.
    #[tokio::test]
    async fn a_finished_producer_starts_the_fan_in_before_its_slow_sibling() {
        let root = tempfile::tempdir().unwrap();
        let slow_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Was the slow producer still running the first time the fan-in ran?
        let ran_early = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let passes = Arc::new(AtomicU32::new(0));

        let fast = StepSpec::new(
            "fast/rendered_md",
            StepRun::in_process(move |ctx: StepCtx| async move {
                let dir = ctx.path_str(&ctx.step_id);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("data.md"), "rows").unwrap();
                let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                Ok(StepOutcome {
                    outputs: vec![ArtifactState::versioned(&pat, "fast-v1")],
                    exit: None,
                })
            }),
        )
        .streams_output();

        let slow = {
            let slow_done = slow_done.clone();
            let passes = passes.clone();
            StepSpec::new(
                "slow/rendered_md",
                StepRun::in_process(move |ctx: StepCtx| {
                    let (slow_done, passes) = (slow_done.clone(), passes.clone());
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("data.md"), "rows").unwrap();
                        // Stay running until the fan-in has had a pass, so
                        // the assertion does not depend on sleep lengths.
                        for _ in 0..400 {
                            if passes.load(Ordering::SeqCst) > 0 {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        slow_done.store(true, std::sync::atomic::Ordering::SeqCst);
                        let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                        Ok(StepOutcome {
                            outputs: vec![ArtifactState::versioned(&pat, "slow-v1")],
                            exit: None,
                        })
                    }
                }),
            )
            .streams_output()
        };

        let grid_index = {
            let (slow_done, ran_early, passes) =
                (slow_done.clone(), ran_early.clone(), passes.clone());
            StepSpec::new(
                "unified_index/grid",
                StepRun::in_process(move |ctx: StepCtx| {
                    let (slow_done, ran_early, passes) =
                        (slow_done.clone(), ran_early.clone(), passes.clone());
                    async move {
                        if passes.fetch_add(1, Ordering::SeqCst) == 0
                            && !slow_done.load(std::sync::atomic::Ordering::SeqCst)
                        {
                            ran_early.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("index.txt"), "x").unwrap();
                        Ok(StepOutcome::default())
                    }
                }),
            )
            .input("fast/rendered_md")
            .input("slow/rendered_md")
        };

        let graph = Graph::build(vec![fast, slow, grid_index]).unwrap();
        let report = tokio::time::timeout(Duration::from_secs(15), runner(root.path()).run(&graph))
            .await
            .expect("the fan-in never ran, so the slow producer never returned")
            .unwrap();

        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        assert!(
            ran_early.load(std::sync::atomic::Ordering::SeqCst),
            "the fan-in waited for every producer: a source that finished early \
             contributed nothing until the slowest one was done"
        );
    }

    /// A second run where nothing moved must not run the fan-in at all.
    ///
    /// This is the steady-state case, and it is the one streaming can
    /// quietly ruin: if an early pass leaves the consumer's recorded
    /// `input_versions` incomplete, the next run sees the missing entries as
    /// movement and re-runs the consumer once per producer — every run,
    /// forever, each pass reading nothing.
    #[tokio::test]
    async fn a_second_run_with_nothing_moved_does_not_run_the_fan_in() {
        let root = tempfile::tempdir().unwrap();
        let passes = Arc::new(AtomicU32::new(0));

        let producer = |name: &str| {
            StepSpec::new(
                format!("{name}/rendered_md"),
                StepRun::in_process(move |ctx: StepCtx| async move {
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join("data.md"), "rows").unwrap();
                    let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                    // Same version both runs: nothing moved.
                    Ok(StepOutcome {
                        outputs: vec![ArtifactState::versioned(&pat, "stable")],
                        exit: None,
                    })
                }),
            )
            .streams_output()
        };
        let build = || {
            Graph::build(vec![
                producer("a"),
                producer("b"),
                producer("c"),
                counting_consumer("unified_index/grid", "a/rendered_md", passes.clone())
                    .input("b/rendered_md")
                    .input("c/rendered_md"),
            ])
            .unwrap()
        };

        let r1 = runner(root.path()).run(&build()).await.unwrap();
        assert!(r1.steps.iter().all(|s| s.status.is_ok()), "{r1:#?}");
        let after_first = passes.load(Ordering::SeqCst);
        assert!(after_first >= 1, "the fan-in never ran on the first run");

        let r2 = runner(root.path()).run(&build()).await.unwrap();
        assert!(r2.steps.iter().all(|s| s.status.is_ok()), "{r2:#?}");
        assert_eq!(
            passes.load(Ordering::SeqCst),
            after_first,
            "nothing moved, so the second run must not run the fan-in at all \
             -- not an early pass, not a final one"
        );
        let idx = r2
            .steps
            .iter()
            .find(|s| s.id == "unified_index/grid")
            .unwrap();
        assert!(
            matches!(idx.status, StepStatus::SkippedUpToDate),
            "expected the fan-in to be skipped up-to-date, got {:?}",
            idx.status
        );
    }

    /// A step may only claim to have consumed what it could actually see.
    ///
    /// This is the shape that lost a whole source. An early pass of the
    /// consumer starts, and a producer it has not read yet *finishes while
    /// that pass is running*. If the pass records its `input_versions` from
    /// whatever is current when it lands, it claims that producer's final
    /// version — a version it never read. The final pass then finds nothing
    /// changed, is skipped up-to-date, and that producer's documents are
    /// simply absent from the index.
    ///
    /// The fixture pipeline caught it two runs in six. This one is
    /// deterministic: the interleaving is forced, not raced.
    #[tokio::test]
    async fn a_pass_may_not_claim_a_producer_that_finished_after_it_started() {
        let root = tempfile::tempdir().unwrap();
        // Set when the consumer's first pass begins; `late` waits for it, so
        // the consumer is guaranteed to be mid-pass when `late` finishes.
        let consumer_running = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let late_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Did `late`'s output exist on each pass? The last entry is what
        // matters: by the end, the consumer must have seen it.
        let saw_late = Arc::new(Mutex::new(Vec::<bool>::new()));

        // Checkpoints, which is what makes an early dispatch happen at all.
        let early = {
            let consumer_running = consumer_running.clone();
            StepSpec::new(
                "early/rendered_md",
                StepRun::in_process(move |ctx: StepCtx| {
                    let consumer_running = consumer_running.clone();
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("data.md"), "early").unwrap();
                        while !consumer_running.load(std::sync::atomic::Ordering::SeqCst) {
                            ctx.checkpoint("early-v1");
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                        let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                        Ok(StepOutcome {
                            outputs: vec![ArtifactState::versioned(&pat, "early-final")],
                            exit: None,
                        })
                    }
                }),
            )
            .streams_output()
        };

        // Writes nothing until the consumer is already mid-pass, then
        // finishes. Its output therefore cannot have been read by that pass.
        let late = {
            let (consumer_running, late_done) = (consumer_running.clone(), late_done.clone());
            StepSpec::new(
                "late/rendered_md",
                StepRun::in_process(move |ctx: StepCtx| {
                    let (consumer_running, late_done) =
                        (consumer_running.clone(), late_done.clone());
                    async move {
                        while !consumer_running.load(std::sync::atomic::Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("data.md"), "late").unwrap();
                        late_done.store(true, std::sync::atomic::Ordering::SeqCst);
                        let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                        Ok(StepOutcome {
                            outputs: vec![ArtifactState::versioned(&pat, "late-final")],
                            exit: None,
                        })
                    }
                }),
            )
            .streams_output()
        };

        let consumer = {
            let (consumer_running, late_done, saw_late) = (
                consumer_running.clone(),
                late_done.clone(),
                saw_late.clone(),
            );
            StepSpec::new(
                "unified_index/grid",
                StepRun::in_process(move |ctx: StepCtx| {
                    let (consumer_running, late_done, saw_late) = (
                        consumer_running.clone(),
                        late_done.clone(),
                        saw_late.clone(),
                    );
                    async move {
                        // Read *first*, before `late` is unblocked: that is
                        // the whole point. On the first pass this sees
                        // nothing from `late`, which is the truth the pass
                        // must not then contradict.
                        let seen = ctx.path_str("late/rendered_md").join("data.md").is_file();
                        saw_late.lock().unwrap().push(seen);
                        // Now let `late` run, and stay alive until it has
                        // finished, so this pass spans its completion. That
                        // is what creates the false claim.
                        consumer_running.store(true, std::sync::atomic::Ordering::SeqCst);
                        for _ in 0..500 {
                            if late_done.load(std::sync::atomic::Ordering::SeqCst) {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("index.txt"), "x").unwrap();
                        Ok(StepOutcome::default())
                    }
                }),
            )
            .input("early/rendered_md")
            .input("late/rendered_md")
        };

        let graph = Graph::build(vec![early, late, consumer]).unwrap();
        let report = tokio::time::timeout(Duration::from_secs(20), runner(root.path()).run(&graph))
            .await
            .expect("deadlock")
            .unwrap();
        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");

        let passes = saw_late.lock().unwrap().clone();
        assert!(
            passes.last().copied().unwrap_or(false),
            "the consumer's last pass never saw `late`'s output, so a source \
             is missing from the index: an early pass claimed a producer that \
             finished after it started, and the final pass was skipped as \
             up-to-date (passes saw: {passes:?})"
        );
    }

    /// The pleasing half of the design: an early pass records its
    /// `input_versions` the ordinary way, so the final pass meets the
    /// existing staleness predicate and is *skipped* when nothing moved
    /// after the last checkpoint. Streaming is the same rules, earlier.
    #[tokio::test]
    async fn the_final_pass_is_skipped_when_the_last_checkpoint_saw_everything() {
        let root = tempfile::tempdir().unwrap();
        let passes = Arc::new(AtomicU32::new(0));

        // Reports the *same* version it last checkpointed, so by the time
        // the producer exits the consumer has already seen everything.
        let producer = StepSpec::new(
            "slack/raw",
            StepRun::in_process({
                let passes = passes.clone();
                move |ctx: StepCtx| {
                    let passes = passes.clone();
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("data.txt"), "all-of-it").unwrap();
                        while passes.load(Ordering::SeqCst) == 0 {
                            ctx.checkpoint("v-final");
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                        let pat = crate::ArtifactPath::parse(&ctx.step_id).unwrap();
                        Ok(StepOutcome {
                            outputs: vec![ArtifactState::versioned(&pat, "v-final")],
                            exit: None,
                        })
                    }
                }
            }),
        )
        .streams_output();

        let graph = Graph::build(vec![
            producer,
            counting_consumer("unified_index/grid", "slack/raw", passes.clone()),
        ])
        .unwrap();
        let report = runner(root.path()).run(&graph).await.unwrap();

        assert!(report.steps.iter().all(|s| s.status.is_ok()), "{report:#?}");
        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "the early pass consumed the final version, so the final pass had nothing to do"
        );
        let consumer = report
            .steps
            .iter()
            .find(|s| s.id == "unified_index/grid")
            .unwrap();
        assert!(
            matches!(consumer.status, StepStatus::Succeeded { .. }),
            "its one pass is its outcome, got {:?}",
            consumer.status
        );
    }

    #[tokio::test]
    async fn first_run_runs_everything_second_run_skips_downstream() {
        let fx = Fixture::new();
        let g = fx.graph();
        let r = runner(fx.root.path());

        let rep1 = r.run(&g).await.unwrap();
        assert!(rep1.all_ok(), "{rep1:#?}");
        for id in [
            "slack/raw",
            "slack/rendered_md",
            "email/raw",
            "email/rendered_md",
            "unified_index/grid",
        ] {
            assert_eq!(fx.run_count(id), 1, "{id} should have run once");
            assert!(
                matches!(rep1.step(id).status, StepStatus::Succeeded { .. }),
                "{id}: {:?}",
                rep1.step(id).status
            );
        }
        let idx = fx.root.path().join("unified_index/grid/index.txt");
        assert_eq!(
            std::fs::read_to_string(&idx).unwrap(),
            "EMAIL V1\nSLACK V1\n"
        );

        // Nothing changed upstream: downloads are re-invoked (they must
        // poll the remote) but report unchanged; everything downstream
        // skips.
        let rep2 = r.run(&g).await.unwrap();
        assert!(rep2.all_ok(), "{rep2:#?}");
        assert_eq!(fx.run_count("slack/raw"), 2);
        assert_eq!(fx.run_count("email/raw"), 2);
        assert_eq!(fx.run_count("slack/rendered_md"), 1, "render must skip");
        assert_eq!(fx.run_count("email/rendered_md"), 1, "render must skip");
        assert_eq!(fx.run_count("unified_index/grid"), 1, "index must skip");
        assert_eq!(
            rep2.step("slack/rendered_md").status,
            StepStatus::SkippedUpToDate
        );
        assert_eq!(
            rep2.step("unified_index/grid").status,
            StepStatus::SkippedUpToDate
        );
    }

    /// The run record is the only thing that makes a run visible to
    /// anyone who did not spawn it — a terminal `datalib-dag` and the
    /// UI's worker write the same file, so the UI can show either. Its
    /// id is the one the caller chose, so the run store (keyed by the
    /// same id) and the record describe the same run.
    #[tokio::test]
    async fn the_run_id_is_the_one_handed_to_the_steps() {
        let fx = Fixture::new();
        let g = fx.graph();
        let pinned = "2026-08-31T12:34:56+02:00";
        let r = runner(fx.root.path()).child_env(BTreeMap::from([
            (crate::subprocess::ENV_NOW.to_string(), pinned.to_string()),
            (
                crate::subprocess::ENV_RUN_ID.to_string(),
                "run-abc".to_string(),
            ),
        ]));
        assert!(r.run(&g).await.unwrap().all_ok());

        let st = DagState::load(fx.root.path()).unwrap();
        let run = st.current_run.expect("a run leaves a record");
        assert_eq!(run.run_id, "run-abc");
        assert_eq!(run.started_at, pinned);
    }

    /// Without a caller-chosen id the runner mints one, and two runs
    /// pinned to the same clock still get different ids.
    #[tokio::test]
    async fn a_run_without_a_given_id_mints_a_unique_one() {
        let fx = Fixture::new();
        let g = fx.graph();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let r = runner(fx.root.path()).child_env(BTreeMap::from([(
                crate::subprocess::ENV_NOW.to_string(),
                "2026-08-31T12:34:56+02:00".to_string(),
            )]));
            assert!(r.run(&g).await.unwrap().all_ok());
            ids.push(
                DagState::load(fx.root.path())
                    .unwrap()
                    .current_run
                    .unwrap()
                    .run_id,
            );
        }
        assert_ne!(ids[0], ids[1]);
        assert!(uuid::Uuid::parse_str(&ids[0]).is_ok(), "{}", ids[0]);
    }

    #[tokio::test]
    async fn a_run_records_its_plan_and_every_step_outcome() {
        let fx = Fixture::new();
        let g = fx.graph();
        let r = runner(fx.root.path());
        assert!(r.run(&g).await.unwrap().all_ok());

        let st = DagState::load(fx.root.path()).unwrap();
        let run = st.current_run.expect("a run leaves a record");
        assert!(run.finished_at.is_some(), "a completed run is closed");
        assert_eq!(run.plan.len(), g.steps.len(), "the plan is the whole graph");
        for id in &run.plan {
            assert_eq!(
                run.states.get(id).map(String::as_str),
                Some("succeeded"),
                "{id} has no state in the run record"
            );
        }

        for id in &run.plan {
            let last = st.steps[id].last_run.as_ref().expect("{id}: no last_run");
            assert_eq!(last.status, "succeeded");
            assert!(last.finished_at.is_some());
            assert!(!last.started_at.is_empty());
        }

        // Second run: everything downstream is up to date, and a skip is
        // a terminal state like any other — the record says so rather
        // than leaving last run's answer in place.
        assert!(r.run(&g).await.unwrap().all_ok());
        let st = DagState::load(fx.root.path()).unwrap();
        let run = st.current_run.expect("second run recorded");
        assert_eq!(
            run.states["slack/rendered_md"], "skipped_up_to_date",
            "a skipped step is recorded as skipped, not left as succeeded"
        );
        assert_eq!(
            st.steps["slack/rendered_md"]
                .last_run
                .as_ref()
                .unwrap()
                .status,
            "skipped_up_to_date"
        );
    }

    /// A failure has to be legible without reading the log: the record
    /// carries the status, the attempt count and the message.
    #[tokio::test]
    async fn a_failed_step_records_its_error_and_blocks_its_dependent() {
        let fx = Fixture::new();
        let failing = StepSpec::new(
            "email/rendered_md",
            StepRun::in_process(|_ctx| async {
                Err(StepError::new(
                    FailureKind::Data,
                    anyhow::anyhow!("bad json"),
                ))
            }),
        )
        .input("email/raw");
        let g = Graph::build(vec![
            download(
                "email",
                fx.email_content.clone(),
                fx.runs["email/raw"].clone(),
            ),
            failing,
            StepSpec::new(
                "unified_index/grid",
                StepRun::in_process(|_ctx| async { Ok(StepOutcome::default()) }),
            )
            .input("email/rendered_md"),
        ])
        .unwrap();
        let _ = runner(fx.root.path()).run(&g).await.unwrap();

        let st = DagState::load(fx.root.path()).unwrap();
        let run = st.current_run.unwrap();
        assert_eq!(run.states["email/rendered_md"], "failed");
        assert_eq!(run.states["unified_index/grid"], "blocked");

        let failed = st.steps["email/rendered_md"].last_run.as_ref().unwrap();
        assert_eq!(failed.status, "failed");
        assert!(failed.error.as_deref().unwrap().contains("bad json"));
        assert_eq!(failed.attempts, 1, "a Data failure is not retried");

        // A step that never ran still gets a record — "blocked, and on
        // what" is the answer the table needs.
        let blocked = st.steps["unified_index/grid"].last_run.as_ref().unwrap();
        assert_eq!(blocked.status, "blocked");
        assert!(blocked.finished_at.is_some());
    }

    /// A step that answers a cancel is recorded as stopped, not failed:
    /// the person asked for it. Its dependents still wait, and the next
    /// run takes the step again — a stop is not "done".
    #[tokio::test]
    async fn a_cancelled_step_is_recorded_as_stopped_and_runs_again_next_time() {
        let fx = Fixture::new();
        let stops = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let stop_once = stops.clone();
        let stopping = StepSpec::new(
            "email/rendered_md",
            StepRun::in_process(move |_ctx| {
                let first = stop_once.swap(false, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if first {
                        Err(StepError::new(
                            FailureKind::Cancelled,
                            anyhow::anyhow!("interrupted"),
                        ))
                    } else {
                        Ok(StepOutcome::default())
                    }
                }
            }),
        )
        .input("email/raw");
        let g = Graph::build(vec![
            download(
                "email",
                fx.email_content.clone(),
                fx.runs["email/raw"].clone(),
            ),
            stopping,
            StepSpec::new(
                "unified_index/grid",
                StepRun::in_process(|_ctx| async { Ok(StepOutcome::default()) }),
            )
            .input("email/rendered_md"),
        ])
        .unwrap();
        let r = runner(fx.root.path());
        let rep = r.run(&g).await.unwrap();
        assert!(!rep.all_ok(), "a stopped run is not a clean one");

        let st = DagState::load(fx.root.path()).unwrap();
        let run = st.current_run.unwrap();
        assert_eq!(run.states["email/rendered_md"], "stopped");
        assert_eq!(run.states["unified_index/grid"], "blocked");
        let stopped = st.steps["email/rendered_md"].last_run.as_ref().unwrap();
        assert_eq!(stopped.status, "stopped");
        assert_eq!(stopped.attempts, 1, "a cancel is not retried");

        // Next run: the stopped step is taken again, and the chain
        // completes.
        let rep = r.run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        let st = DagState::load(fx.root.path()).unwrap();
        assert_eq!(
            st.steps["email/rendered_md"]
                .last_run
                .as_ref()
                .unwrap()
                .status,
            "succeeded"
        );
    }

    /// A failure moves `last_run` and leaves `last_success_at` where the
    /// last good run put it — the Manage screen's "last success" (#646).
    #[tokio::test]
    async fn a_failure_keeps_the_last_success_it_follows() {
        let fx = Fixture::new();
        let fails = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fail_now = fails.clone();
        let flaky = StepSpec::new(
            "email/rendered_md",
            StepRun::in_process(move |_ctx| {
                let fail = fail_now.load(std::sync::atomic::Ordering::SeqCst);
                async move {
                    if fail {
                        Err(StepError::new(
                            FailureKind::Data,
                            anyhow::anyhow!("bad json"),
                        ))
                    } else {
                        Ok(StepOutcome::default())
                    }
                }
            }),
        )
        .input("email/raw");
        let g = Graph::build(vec![
            download(
                "email",
                fx.email_content.clone(),
                fx.runs["email/raw"].clone(),
            ),
            flaky,
        ])
        .unwrap();
        let r = runner(fx.root.path());
        r.run(&g).await.unwrap();
        let mut st = DagState::load(fx.root.path()).unwrap();
        let good = st.steps.get_mut("email/rendered_md").unwrap();
        let succeeded_at = good.last_run.as_ref().unwrap().finished_at.clone();
        assert!(succeeded_at.is_some());
        assert_eq!(good.last_success_at, succeeded_at);
        // Both runs finish within the same second, so a stamp the
        // failure wrongly wrote would equal the real one. Back-date it.
        let succeeded_at = Some("2026-01-01T00:00:00+00:00".to_string());
        good.last_success_at = succeeded_at.clone();
        st.save(fx.root.path()).unwrap();

        fails.store(true, std::sync::atomic::Ordering::SeqCst);
        // Moving the input is what makes the step run again rather than
        // skip as up to date.
        *fx.email_content.lock().unwrap() = "email v2".to_string();
        r.run(&g).await.unwrap();
        let st = DagState::load(fx.root.path()).unwrap();
        let after = &st.steps["email/rendered_md"];
        assert_eq!(after.last_run.as_ref().unwrap().status, "failed");
        assert_eq!(after.last_success_at, succeeded_at);
    }

    #[tokio::test]
    async fn upstream_change_reruns_only_the_affected_chain() {
        let fx = Fixture::new();
        let g = fx.graph();
        let r = runner(fx.root.path());
        r.run(&g).await.unwrap();

        *fx.slack_content.lock().unwrap() = "slack v2".to_string();
        let rep = r.run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");

        assert_eq!(fx.run_count("slack/rendered_md"), 2, "slack chain reruns");
        assert_eq!(fx.run_count("email/rendered_md"), 1, "email chain skips");
        assert_eq!(fx.run_count("unified_index/grid"), 2, "fan-in reruns");
        // The fan-in saw exactly which input moved.
        assert_eq!(
            *fx.index_changed_inputs.lock().unwrap(),
            vec!["slack/rendered_md".to_string()]
        );
        let idx = fx.root.path().join("unified_index/grid/index.txt");
        assert_eq!(
            std::fs::read_to_string(&idx).unwrap(),
            "EMAIL V1\nSLACK V2\n"
        );
    }

    #[tokio::test]
    async fn subset_sync_runs_only_selected_downloads() {
        let fx = Fixture::new();
        let g = fx.graph();
        let r = runner(fx.root.path());
        r.run(&g).await.unwrap();

        // Both upstreams change, but only slack is selected for sync:
        // email's download must not be invoked, its stale chain must
        // count as up to date, and the fan-in must rerun on slack's
        // change alone.
        *fx.slack_content.lock().unwrap() = "slack v2".to_string();
        *fx.email_content.lock().unwrap() = "email v2".to_string();
        let r2 = runner(fx.root.path()).only_fringe(["slack/raw".to_string()]);
        let rep = r2.run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");

        assert_eq!(fx.run_count("slack/raw"), 2);
        assert_eq!(fx.run_count("email/raw"), 1, "email must not sync");
        assert_eq!(rep.step("email/raw").status, StepStatus::NotSelected);
        assert_eq!(
            rep.step("email/rendered_md").status,
            StepStatus::NotSelected
        );
        assert_eq!(fx.run_count("slack/rendered_md"), 2);
        assert_eq!(fx.run_count("unified_index/grid"), 2);
        // The index saw only the synced chain as changed, and the
        // output still carries email's OLD content.
        assert_eq!(
            *fx.index_changed_inputs.lock().unwrap(),
            vec!["slack/rendered_md".to_string()]
        );
        let idx = fx.root.path().join("unified_index/grid/index.txt");
        assert_eq!(
            std::fs::read_to_string(&idx).unwrap(),
            "EMAIL V1\nSLACK V2\n"
        );

        // A full run afterwards picks up email's pending change.
        let rep = r.run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(fx.run_count("email/raw"), 2);
        assert_eq!(fx.run_count("email/rendered_md"), 2);
        assert_eq!(fx.run_count("slack/rendered_md"), 2, "slack unchanged now");
        assert_eq!(
            std::fs::read_to_string(&idx).unwrap(),
            "EMAIL V2\nSLACK V2\n"
        );
    }

    /// Regression: "running" has to reach the *file*, while the step is
    /// still running.
    #[tokio::test]
    async fn a_running_step_is_visible_on_disk_while_it_runs() {
        let root = tempfile::tempdir().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());

        let g = Graph::build(vec![StepSpec::new("gate/raw", {
            let (started, release) = (started.clone(), release.clone());
            StepRun::in_process(move |ctx: StepCtx| {
                let (started, release) = (started.clone(), release.clone());
                async move {
                    std::fs::create_dir_all(ctx.path_str(&ctx.step_id)).unwrap();
                    started.notify_one();
                    release.notified().await;
                    Ok(StepOutcome::default())
                }
            })
        })])
        .unwrap();

        let r = runner(root.path());
        let run = tokio::spawn(async move { r.run(&g).await });

        started.notified().await;
        // The step is parked inside its body. Whatever the file says
        // now is what a poller would see for as long as the step runs.
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let st = DagState::load(root.path()).unwrap();
                if let Some(cur) = st
                    .current_run
                    .as_ref()
                    .and_then(|c| c.states.get("gate/raw"))
                {
                    return (
                        cur.clone(),
                        st.steps.get("gate/raw").and_then(|s| s.last_run.clone()),
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a step in flight must be readable as running from dag_state.json");

        assert_eq!(seen.0, "running");
        let last = seen.1.expect("a dispatched step has an open last_run");
        assert!(
            last.finished_at.is_none(),
            "an in-flight step has no finish time yet: {last:?}"
        );
        assert!(
            !last.started_at.is_empty(),
            "…but it does have a start time, which is what the grid shows"
        );

        release.notify_one();
        assert!(run.await.unwrap().unwrap().all_ok());
    }

    /// Regression: a subset sync must not rewrite the history of the
    /// steps it did not select.
    #[tokio::test]
    async fn a_subset_sync_leaves_unselected_steps_history_alone() {
        let fx = Fixture::new();
        let g = fx.graph();
        let r = runner(fx.root.path());
        assert!(r.run(&g).await.unwrap().all_ok());

        let before = DagState::load(fx.root.path()).unwrap().steps["email/raw"]
            .last_run
            .clone()
            .expect("the first run recorded email/raw");
        assert_eq!(before.status, "succeeded");

        // A sync of a different source. It walks email/raw and reports
        // it not-selected, but nothing happened to email/raw.
        let r2 = runner(fx.root.path()).only_fringe(["slack/raw".to_string()]);
        let rep = r2.run(&g).await.unwrap();
        assert_eq!(rep.step("email/raw").status, StepStatus::NotSelected);

        let st = DagState::load(fx.root.path()).unwrap();
        let after = st.steps["email/raw"]
            .last_run
            .as_ref()
            .expect("email/raw keeps its record");
        assert_eq!(
            after.status, "succeeded",
            "a run that did not select this step must not restate what it did"
        );
        assert_eq!(
            after.finished_at, before.finished_at,
            "nor when it did it — this is the timestamp the grid shows as \
             'last synced', and it moved on every unrelated sync"
        );

        // The run record still carries the fact, because that map is
        // about the run rather than about the step.
        assert_eq!(
            st.current_run.expect("a run leaves a record").states["email/raw"],
            "not_selected"
        );
    }

    /// A step no run has ever selected has no history to keep, and must
    /// not acquire a fake one: `not_selected` is not something that
    /// happened to it, so it stays "never run" rather than becoming a
    /// row stamped with a run that skipped it.
    #[tokio::test]
    async fn a_never_selected_step_has_no_last_run_at_all() {
        let fx = Fixture::new();
        let g = fx.graph();
        let r = runner(fx.root.path()).only_fringe(["slack/raw".to_string()]);
        assert!(r.run(&g).await.unwrap().all_ok());

        let st = DagState::load(fx.root.path()).unwrap();
        assert!(
            st.steps
                .get("email/raw")
                .and_then(|s| s.last_run.as_ref())
                .is_none(),
            "a step this run never selected has not run, and must not \
             claim to have: {:#?}",
            st.steps.get("email/raw"),
        );
    }

    /// Regression: subset-sync on a *fresh* data root. Out-of-scope
    /// steps must stay untouched even when nothing has ever run here.
    /// A step with no recorded successful run counts as dirty, and that
    /// check used to run ahead of the subset-sync skip, so an unselected
    /// chain's render was invoked with no raw store underneath it — it
    /// failed with `Data`, and the failure poisoned the fan-in, so the
    /// chain the user *did* select never reached the index.
    #[tokio::test]
    async fn subset_sync_on_a_first_run_skips_unselected_chains() {
        let fx = Fixture::new();
        let g = fx.graph();
        // No prior run: nothing in this root has ever succeeded.
        let r = runner(fx.root.path()).only_fringe(["slack/raw".to_string()]);
        let rep = r.run(&g).await.unwrap();

        // The selected chain does its work.
        assert_eq!(fx.run_count("slack/raw"), 1);
        assert_eq!(fx.run_count("slack/rendered_md"), 1);

        // The unselected chain must not be touched at all — not the
        // download (that part already worked), and not the render.
        assert_eq!(rep.step("email/raw").status, StepStatus::NotSelected);
        assert_eq!(
            fx.run_count("email/rendered_md"),
            0,
            "render of an unselected chain must not be invoked: its raw \
             store does not exist yet"
        );
        assert_eq!(
            rep.step("email/rendered_md").status,
            StepStatus::NotSelected
        );

        // ...so nothing is poisoned, and the fan-in still indexes the
        // chain the user asked to sync.
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(fx.run_count("unified_index/grid"), 1);
        assert_eq!(
            std::fs::read_to_string(fx.root.path().join("unified_index/grid/index.txt")).unwrap(),
            "SLACK V1\n"
        );
    }

    /// "Sync yolink" means yolink, not "yolink plus whatever else is
    /// pending". Yesterday's email download landed but its render
    /// failed; today's subset sync of a different source leaves that
    /// alone rather than quietly dragging it along. The next full run
    /// picks it up.
    #[tokio::test]
    async fn subset_sync_leaves_pending_work_in_other_chains_alone() {
        let fx = Fixture::new();
        let failing_email_render = StepSpec::new(
            "email/rendered_md",
            StepRun::in_process(|_ctx| async {
                Err(StepError::new(
                    FailureKind::Data,
                    anyhow::anyhow!("boom: unparseable row"),
                ))
            }),
        )
        .input("email/raw");
        let broken = Graph::build(vec![
            download(
                "slack",
                fx.slack_content.clone(),
                fx.runs["slack/raw"].clone(),
            ),
            render("slack", fx.runs["slack/rendered_md"].clone()),
            download(
                "email",
                fx.email_content.clone(),
                fx.runs["email/raw"].clone(),
            ),
            failing_email_render,
            index(
                fx.runs["unified_index/grid"].clone(),
                fx.index_changed_inputs.clone(),
            ),
        ])
        .unwrap();

        // Run 1 (full): email downloads fine, its render fails. Now
        // `email/raw` is real but has never been rendered.
        let rep1 = runner(fx.root.path()).run(&broken).await.unwrap();
        assert!(!rep1.all_ok());
        assert!(matches!(
            rep1.step("email/rendered_md").status,
            StepStatus::Failed { .. }
        ));

        // Run 2: sync slack only. The email chain is untouched — no
        // poll, no retry — and slack still reaches the index.
        let g = fx.graph();
        let r2 = runner(fx.root.path()).only_fringe(["slack/raw".to_string()]);
        let rep2 = r2.run(&g).await.unwrap();
        assert!(rep2.all_ok(), "{rep2:#?}");
        assert_eq!(fx.run_count("email/raw"), 1, "no poll");
        assert_eq!(fx.run_count("email/rendered_md"), 0, "no retry");
        assert_eq!(
            rep2.step("email/rendered_md").status,
            StepStatus::NotSelected
        );
        // The index was blocked in run 1 (email.render failed), so this
        // is its first run: slack reaches it, email contributes nothing.
        assert_eq!(fx.run_count("unified_index/grid"), 1);

        // Run 3 (full): the pending render is picked back up.
        let rep3 = runner(fx.root.path()).run(&g).await.unwrap();
        assert!(rep3.all_ok(), "{rep3:#?}");
        assert_eq!(fx.run_count("email/rendered_md"), 1, "full run recovers it");
        assert_eq!(
            std::fs::read_to_string(fx.root.path().join("unified_index/grid/index.txt")).unwrap(),
            "EMAIL V1\nSLACK V1\n"
        );
    }

    /// A step this run isn't touching is never content-hashed, however
    /// much data its output tree holds.
    #[tokio::test]
    async fn an_unselected_step_with_data_on_disk_is_not_hashed() {
        let fx = Fixture::new();
        // Writes its store, then fails without reporting a version.
        let aborted_email_download = StepSpec::new(
            "email/raw",
            StepRun::in_process(|ctx: StepCtx| async move {
                let dir = ctx.path_str(&ctx.step_id);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("data.txt"), "email v1").unwrap();
                Err(StepError::new(
                    FailureKind::Data,
                    anyhow::anyhow!("boom: unparseable page 3"),
                ))
            }),
        );
        let broken = Graph::build(vec![
            download(
                "slack",
                fx.slack_content.clone(),
                fx.runs["slack/raw"].clone(),
            ),
            render("slack", fx.runs["slack/rendered_md"].clone()),
            aborted_email_download,
            render("email", fx.runs["email/rendered_md"].clone()),
            index(
                fx.runs["unified_index/grid"].clone(),
                fx.index_changed_inputs.clone(),
            ),
        ])
        .unwrap();

        // Run 1 (full): email's download writes and then fails.
        let rep1 = runner(fx.root.path()).run(&broken).await.unwrap();
        assert!(matches!(
            rep1.step("email/raw").status,
            StepStatus::Failed { .. }
        ));
        let store = fx.root.path().join("email/raw/data.txt");
        assert!(
            store.exists(),
            "the test needs data on disk, or hashing would give ABSENT and prove nothing"
        );

        // Run 2: sync slack only. `email/raw` is out of scope and has
        // no recorded version, and the runner says so rather than
        // reading the store to invent one.
        let g = fx.graph();
        let r = runner(fx.root.path()).only_fringe(["slack/raw".to_string()]);
        let rep2 = r.run(&g).await.unwrap();
        assert!(rep2.all_ok(), "{rep2:#?}");
        assert_eq!(rep2.step("email/raw").status, StepStatus::NotSelected);
        assert_eq!(rep2.step("email/raw").outputs[0].1, UNKNOWN);
        assert_eq!(
            rep2.step("email/rendered_md").outputs[0].1,
            UNKNOWN,
            "never ran, nothing recorded"
        );
        // The chain that was asked for still reaches the index.
        assert_eq!(fx.run_count("unified_index/grid"), 1);

        // Run 3, identical: "we don't know" compares equal to itself,
        // so the fan-in is not dirtied every single run. This is the
        // property that makes dropping the hash safe — the hash was
        // stable across runs too, just three billion bytes slower.
        let rep3 = runner(fx.root.path())
            .only_fringe(["slack/raw".to_string()])
            .run(&g)
            .await
            .unwrap();
        assert!(rep3.all_ok(), "{rep3:#?}");
        assert_eq!(
            fx.run_count("unified_index/grid"),
            1,
            "an unversioned unselected input must not re-dirty the fan-in"
        );
    }

    /// A step's config changed but its inputs did not: it re-runs.
    #[tokio::test]
    async fn config_change_reruns_the_step_with_unchanged_inputs() {
        let fx = Fixture::new();
        let runs = Arc::new(AtomicU32::new(0));
        // Same id, same inputs, same outputs — only the step's own
        // definition differs, which is what a params edit amounts to.
        let render_v = |tag: &'static str, runs: Arc<AtomicU32>| {
            StepSpec::new(
                "slack/rendered_md",
                StepRun::in_process(move |ctx: StepCtx| {
                    let runs = runs.clone();
                    async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        let src = ctx.path_str("slack/raw/data.txt");
                        let dir = ctx.path_str("slack/rendered_md");
                        std::fs::create_dir_all(&dir).unwrap();
                        let text = std::fs::read_to_string(&src).unwrap_or_default();
                        std::fs::write(
                            std::path::Path::new(&dir).join("data.md"),
                            format!("{tag}:{text}"),
                        )
                        .unwrap();
                        Ok(StepOutcome::default())
                    }
                }),
            )
            .input("slack/raw")
            .code_version(tag)
        };
        let graph_with = |tag: &'static str, runs: Arc<AtomicU32>| {
            Graph::build(vec![
                download(
                    "slack",
                    fx.slack_content.clone(),
                    fx.runs["slack/raw"].clone(),
                ),
                render_v(tag, runs),
            ])
            .unwrap()
        };

        let rep1 = runner(fx.root.path())
            .run(&graph_with("v1", runs.clone()))
            .await
            .unwrap();
        assert!(rep1.all_ok(), "{rep1:#?}");
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Same config again: nothing moved, nothing re-runs.
        let rep2 = runner(fx.root.path())
            .run(&graph_with("v1", runs.clone()))
            .await
            .unwrap();
        assert_eq!(
            rep2.step("slack/rendered_md").status,
            StepStatus::SkippedUpToDate
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1, "idempotent re-run");

        // Config edited. The raw store is untouched, so only the
        // fingerprint can catch this.
        let rep3 = runner(fx.root.path())
            .run(&graph_with("v2", runs.clone()))
            .await
            .unwrap();
        assert!(rep3.all_ok(), "{rep3:#?}");
        assert!(
            matches!(
                rep3.step("slack/rendered_md").status,
                StepStatus::Succeeded { .. }
            ),
            "a config change must re-run the step: {:?}",
            rep3.step("slack/rendered_md").status
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read_to_string(fx.root.path().join("slack/rendered_md/data.md")).unwrap(),
            "v2:slack v1",
            "the tree must be rebuilt under the new config"
        );
    }

    /// A step added to the config after a successful run, consuming an
    /// output that has *not* changed. It has no recorded success, so it
    /// is stale and runs — same clause that retries an aborted step.
    #[tokio::test]
    async fn step_added_to_the_config_runs_against_unchanged_inputs() {
        let fx = Fixture::new();
        let g1 = Graph::build(vec![
            download(
                "slack",
                fx.slack_content.clone(),
                fx.runs["slack/raw"].clone(),
            ),
            render("slack", fx.runs["slack/rendered_md"].clone()),
        ])
        .unwrap();
        assert!(runner(fx.root.path()).run(&g1).await.unwrap().all_ok());

        let audit_runs = Arc::new(AtomicU32::new(0));
        let ar = audit_runs.clone();
        let audit = StepSpec::new(
            "slack/audit",
            StepRun::in_process(move |ctx: StepCtx| {
                let ar = ar.clone();
                async move {
                    ar.fetch_add(1, Ordering::SeqCst);
                    let dir = ctx.path_str("slack/audit");
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(std::path::Path::new(&dir).join("a.txt"), "audited").unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        )
        .input("slack/raw");

        let g2 = Graph::build(vec![
            download(
                "slack",
                fx.slack_content.clone(),
                fx.runs["slack/raw"].clone(),
            ),
            render("slack", fx.runs["slack/rendered_md"].clone()),
            audit,
        ])
        .unwrap();
        let rep = runner(fx.root.path()).run(&g2).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        // The download re-polled and found nothing new...
        assert!(
            matches!(
                rep.step("slack/raw").status,
                StepStatus::Succeeded { changed: 0 }
            ),
            "{:?}",
            rep.step("slack/raw").status
        );
        // ...the existing render is up to date...
        assert_eq!(
            rep.step("slack/rendered_md").status,
            StepStatus::SkippedUpToDate
        );
        // ...and the new step still runs.
        assert_eq!(audit_runs.load(Ordering::SeqCst), 1);
        assert!(fx.root.path().join("slack/audit/a.txt").is_file());
    }
    /// A consumer must notice its input *disappearing*, not just
    /// changing. A deleted tree versions as `absent`, which differs from
    /// a content hash like any other change, so the consumer re-runs and
    /// gets a chance to drop the output it built from data that is gone.
    #[tokio::test]
    async fn deleted_input_reruns_its_consumer() {
        let root = tempfile::tempdir().unwrap();

        // Writes its tree on the first run only. After the user deletes
        // it, the step still runs (no inputs, so always) but recreates
        // nothing — which is how the tree stays absent.
        let wrote = Arc::new(AtomicU32::new(0));
        let w = wrote.clone();
        let producer = StepSpec::new(
            "takeout/raw",
            StepRun::in_process(move |ctx: StepCtx| {
                let w = w.clone();
                async move {
                    if w.fetch_add(1, Ordering::SeqCst) == 0 {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("chat.json"), "v1").unwrap();
                    }
                    Ok(StepOutcome::default())
                }
            }),
        );

        let runs = Arc::new(AtomicU32::new(0));
        let rn = runs.clone();
        let consumer = StepSpec::new(
            "takeout/rendered_md",
            StepRun::in_process(move |ctx: StepCtx| {
                let rn = rn.clone();
                async move {
                    rn.fetch_add(1, Ordering::SeqCst);
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    let out = std::path::Path::new(&dir).join("chat.md");
                    match std::fs::read_to_string(ctx.path(&ctx.inputs[0]).join("chat.json")) {
                        Ok(text) => std::fs::write(out, text).unwrap(),
                        // Source gone: drop what we rendered from it.
                        Err(_) => {
                            let _ = std::fs::remove_file(out);
                        }
                    }
                    Ok(StepOutcome::default())
                }
            }),
        )
        .input("takeout/raw");

        let g = Graph::build(vec![producer, consumer]).unwrap();
        let r = runner(root.path());
        assert!(r.run(&g).await.unwrap().all_ok());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(root.path().join("takeout/rendered_md/chat.md").is_file());

        // The user deletes the raw store off disk.
        std::fs::remove_dir_all(root.path().join("takeout/raw")).unwrap();

        let rep = r.run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "a deleted input must re-run its consumer, not read as unchanged"
        );
        assert!(
            !root.path().join("takeout/rendered_md/chat.md").exists(),
            "the consumer got its chance to drop output built from data that is gone"
        );

        // And it settles: still absent next run, so nothing re-runs.
        let rep = r.run(&g).await.unwrap();
        assert_eq!(
            rep.step("takeout/rendered_md").status,
            StepStatus::SkippedUpToDate
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    /// One source's broken render must not keep the others out of the
    /// grid: the index reads what every render has committed.
    #[tokio::test]
    async fn a_failed_render_leaves_the_index_to_the_sources_that_worked() {
        let fx = Fixture::new();
        // Break slack.render by removing its input mid-way: simplest is
        // a fresh graph where slack.render always fails.
        let failing_render = StepSpec::new(
            "slack/rendered_md",
            StepRun::in_process(|_ctx| async {
                Err(StepError::new(
                    FailureKind::Data,
                    anyhow::anyhow!("boom: unparseable row"),
                ))
            }),
        )
        .input("slack/raw");
        let g = Graph::build(vec![
            download(
                "slack",
                fx.slack_content.clone(),
                fx.runs["slack/raw"].clone(),
            ),
            failing_render,
            download(
                "email",
                fx.email_content.clone(),
                fx.runs["email/raw"].clone(),
            ),
            render("email", fx.runs["email/rendered_md"].clone()),
            index(
                fx.runs["unified_index/grid"].clone(),
                fx.index_changed_inputs.clone(),
            ),
        ])
        .unwrap();

        let rep = runner(fx.root.path()).run(&g).await.unwrap();
        assert!(!rep.all_ok());
        assert_eq!(
            rep.step("slack/rendered_md").status,
            StepStatus::Failed {
                kind: FailureKind::Data
            }
        );
        // Data errors don't retry.
        assert_eq!(rep.step("slack/rendered_md").attempts, 1);
        // The sibling chain still ran to completion...
        assert!(matches!(
            rep.step("email/rendered_md").status,
            StepStatus::Succeeded { .. }
        ));
        // ...and so did the fan-in, over what email committed.
        assert!(
            matches!(
                rep.step("unified_index/grid").status,
                StepStatus::Succeeded { .. }
            ),
            "{rep:#?}"
        );
        assert_eq!(fx.run_count("unified_index/grid"), 1);
    }

    #[tokio::test]
    async fn transient_failures_retry_then_succeed() {
        let root = tempfile::tempdir().unwrap();
        let attempts_seen = Arc::new(AtomicU32::new(0));
        let a = attempts_seen.clone();
        let flaky = StepSpec::new(
            "flaky/raw",
            StepRun::in_process(move |ctx: StepCtx| {
                let a = a.clone();
                async move {
                    let n = a.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 {
                        return Err(StepError::new(
                            FailureKind::Transient,
                            anyhow::anyhow!("connection reset"),
                        ));
                    }
                    let dir = ctx.path_str("flaky/raw");
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join("x"), "ok").unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        );
        let g = Graph::build(vec![flaky]).unwrap();
        let rep = runner(root.path()).run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(rep.step("flaky/raw").attempts, 3);
    }

    #[tokio::test]
    async fn failed_step_stays_dirty_and_recovers_next_run() {
        let root = tempfile::tempdir().unwrap();
        let fail_now = Arc::new(Mutex::new(true));
        let runs = Arc::new(AtomicU32::new(0));
        let (f, rn) = (fail_now.clone(), runs.clone());
        let dl = StepSpec::new(
            "src/raw",
            StepRun::in_process(move |ctx: StepCtx| {
                let (f, rn) = (f.clone(), rn.clone());
                async move {
                    rn.fetch_add(1, Ordering::SeqCst);
                    let dir = ctx.path_str("src/raw");
                    std::fs::create_dir_all(&dir).unwrap();
                    // Partial progress lands even on the failing run —
                    // the step is incremental and commits before dying.
                    std::fs::write(dir.join("data.txt"), "partial").unwrap();
                    if *f.lock().unwrap() {
                        let pat = crate::ArtifactPath::parse("src/raw").unwrap();
                        return Err(
                            StepError::new(FailureKind::Auth, anyhow::anyhow!("HTTP 401"))
                                .with_outputs(vec![ArtifactState::versioned(
                                    &pat,
                                    blake3::hash(b"partial").to_hex().to_string(),
                                )]),
                        );
                    }
                    std::fs::write(dir.join("data.txt"), "complete").unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        );
        let render_runs = Arc::new(AtomicU32::new(0));
        let g = Graph::build(vec![dl, render("src", render_runs.clone())]).unwrap();

        let r = runner(root.path());
        let rep1 = r.run(&g).await.unwrap();
        assert_eq!(
            rep1.step("src/raw").status,
            StepStatus::Failed {
                kind: FailureKind::Auth
            }
        );
        // Auth doesn't retry.
        assert_eq!(rep1.step("src/raw").attempts, 1);
        // What the failed download committed, it vouched for, so the
        // render reads it (plans/supervisor.md §2.5).
        assert!(
            matches!(
                rep1.step("src/rendered_md").status,
                StepStatus::Succeeded { .. }
            ),
            "{rep1:#?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("src/rendered_md/data.md")).unwrap(),
            "PARTIAL"
        );

        // "Fix the credentials" and rerun: everything completes.
        *fail_now.lock().unwrap() = false;
        let rep2 = r.run(&g).await.unwrap();
        assert!(rep2.all_ok(), "{rep2:#?}");
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(render_runs.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read_to_string(root.path().join("src/rendered_md/data.md")).unwrap(),
            "COMPLETE"
        );
    }

    #[tokio::test]
    async fn events_stream_start_progress_finish() {
        let fx = Fixture::new();
        let g = fx.graph();
        let rec = Arc::new(Recorder::default());
        let r = runner(fx.root.path()).sink(rec.clone());
        r.run(&g).await.unwrap();

        let events = rec.0.lock().unwrap();
        let starts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::StepStart { step, .. } => Some(step.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 5, "every step started once: {starts:?}");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ProgressInc { step, .. } if step == "slack/raw"
        )));
        let finishes = events
            .iter()
            .filter(|e| matches!(e, Event::StepFinish { .. }))
            .count();
        assert_eq!(finishes, 5);
    }
}
