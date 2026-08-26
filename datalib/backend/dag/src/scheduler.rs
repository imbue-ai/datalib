//! The runner: executes a [`Graph`] with bounded parallelism,
//! skipping steps whose inputs are unchanged, retrying failures by
//! kind, and poisoning the subtree below a failure.
//!
//! Scheduling semantics:
//!
//! * The run executes a *runnable subgraph*: the source steps this run
//!   selected plus everything downstream of them. With no `--sync` that
//!   is the whole graph. Steps outside it are reported `NotSelected`
//!   and cannot run, whatever their state.
//! * Inside the subgraph a step runs iff it is **stale**, which is one
//!   predicate with four clauses: it declares no inputs (its real input
//!   is outside the graph, so it always runs); or it has never
//!   succeeded; or some input's version differs from the one it
//!   consumed at its last success; or its own fingerprint — argv,
//!   env, declared patterns — differs from the one recorded then.
//! * A step reports a content-derived version per output. Two runs over
//!   the same data report the same string, so "unchanged" is derived
//!   rather than asserted, and consumers skip. An output the step says
//!   nothing about is content hashed instead.
//! * A failed step blocks its dependents *this run*, but any partial
//!   output versions it reported are recorded — steps are
//!   incremental, so the next run resumes from the committed partial
//!   state.
//! * Failure kinds map to a retry policy here; the step only
//!   classifies. Retries simply re-invoke the step — safe because
//!   steps promise idempotency.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::task::JoinSet;

use crate::events::{Event, EventSink, NoopSink, StepProgress};
use crate::graph::Graph;
use crate::state::{DagState, StepState};
use crate::step::{ArtifactState, FailureKind, StepCtx, StepError, StepOutcome, StepRun, StepSpec};
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
    pub parallelism: usize,
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
}

impl Runner {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            parallelism: 4,
            sink: Arc::new(NoopSink),
            retry: RetryPolicy::default(),
            only_fringe: None,
            child_env: Arc::new(BTreeMap::new()),
        }
    }

    pub fn sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sink = sink;
        self
    }

    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Enable subset-sync mode with the given fringe step ids.
    pub fn only_fringe(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.only_fringe = Some(ids.into_iter().collect());
        self
    }

    /// Set the run-wide subprocess environment (see [`Runner::child_env`]).
    pub fn child_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.child_env = Arc::new(env);
        self
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
    fn as_str(&self) -> &'static str {
        match self {
            StepStatus::Succeeded { .. } => "succeeded",
            StepStatus::SkippedUpToDate => "skipped_up_to_date",
            StepStatus::NotSelected => "not_selected",
            StepStatus::Blocked { .. } => "blocked",
            StepStatus::Failed { .. } => "failed",
        }
    }
    pub fn is_ok(&self) -> bool {
        matches!(
            self,
            StepStatus::Succeeded { .. } | StepStatus::SkippedUpToDate | StepStatus::NotSelected
        )
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

/// What the dispatcher decided for a ready step.
enum Decision {
    Run {
        ctx: StepCtx,
    },
    /// Up to date, or outside the runnable subgraph. Either way the
    /// step's outputs keep the versions recorded for them, so consumers
    /// compare against the right thing.
    Skip {
        status: StepStatus,
    },
    Block {
        on: String,
    },
}

impl Runner {
    pub async fn run(&self, graph: &Graph) -> Result<RunReport> {
        // Announce the full plan first, so consumers can draw every
        // task (pending included) before anything runs.
        self.sink.emit(&Event::RunPlan {
            steps: graph
                .topo
                .iter()
                .map(|&i| graph.steps[i].id.clone())
                .collect(),
        });
        let mut state = DagState::load(&self.data_root).context("load dag state")?;

        let n = graph.steps.len();
        // Current version of every concrete artifact, filled in as
        // producers reach a terminal state. Every artifact has a
        // producer: `Graph::build` synthesizes a source step for any
        // path the config leaves unwritten, so there is nothing for the
        // scheduler to hash on its own behalf.
        let mut versions: HashMap<String, String> = HashMap::new();
        // Whether each artifact's version moved this run (drives the
        // per-output `changed` flag in the report).
        let mut changed_now: HashMap<String, bool> = HashMap::new();

        let mut status: Vec<Option<StepStatus>> = vec![None; n];
        let runnable = self.runnable_subgraph(graph);
        let mut attempts_taken: Vec<u32> = vec![0; n];
        let mut errors: Vec<Option<String>> = vec![None; n];
        let mut remaining_deps: Vec<usize> = graph.deps.iter().map(|d| d.len()).collect();
        let mut ready: VecDeque<usize> = (0..n).filter(|&i| remaining_deps[i] == 0).collect();
        let mut running = 0usize;
        let mut set: JoinSet<(usize, u32, Result<StepOutcome, StepError>)> = JoinSet::new();

        loop {
            // Dispatch as many ready steps as parallelism allows.
            // Skip/block decisions are made inline (no slot consumed);
            // real work is spawned.
            while running < self.parallelism {
                let Some(i) = ready.pop_front() else { break };
                match self.decide(graph, &state, &status, &runnable, &versions, i) {
                    Decision::Skip { status: st } => {
                        // Outputs keep their last-recorded versions.
                        let prev = state.steps.get(&graph.steps[i].id);
                        for out in &graph.steps[i].outputs {
                            let v = match prev.and_then(|s| s.output_versions.get(out.as_str())) {
                                Some(v) => v.clone(),
                                // Succeeded before but no recorded
                                // version — hash what's on disk.
                                None => tree_version(&self.data_root.join(out.as_str()))?,
                            };
                            versions.insert(out.as_str().to_string(), v);
                            changed_now.insert(out.as_str().to_string(), false);
                        }
                        self.finish(graph, &mut status, i, st, None);
                        release_dependents(graph, &mut remaining_deps, &mut ready, i);
                    }
                    Decision::Block { on } => {
                        self.finish(graph, &mut status, i, StepStatus::Blocked { on }, None);
                        release_dependents(graph, &mut remaining_deps, &mut ready, i);
                    }
                    Decision::Run { ctx } => {
                        running += 1;
                        let run = graph.steps[i].run.clone();
                        let retry = self.retry.clone();
                        let sink = self.sink.clone();
                        let child_env = self.child_env.clone();
                        set.spawn(async move {
                            let (attempts, res) =
                                invoke_with_retry(&run, ctx, &retry, &sink, &child_env).await;
                            (i, attempts, res)
                        });
                    }
                }
            }

            if running == 0 {
                break;
            }
            let (i, attempts, res) = set
                .join_next()
                .await
                .expect("running > 0 implies a joinable task")
                .context("step task panicked")?;
            running -= 1;
            attempts_taken[i] = attempts;

            let spec = &graph.steps[i];
            let prior_outs = state
                .steps
                .get(&spec.id)
                .map(|s| s.output_versions.clone())
                .unwrap_or_default();
            let st = match res {
                Ok(outcome) => {
                    match resolve_outputs(
                        &self.data_root,
                        spec,
                        &graph.fingerprints[i],
                        &outcome.outputs,
                    ) {
                        Ok(resolved) => {
                            let mut changed = 0usize;
                            for (path, v) in &resolved {
                                let moved = prior_outs.get(path) != Some(v);
                                changed += moved as usize;
                                versions.insert(path.clone(), v.clone());
                                changed_now.insert(path.clone(), moved);
                            }
                            let input_versions = graph.resolved_inputs[i]
                                .iter()
                                .filter_map(|a| {
                                    versions
                                        .get(a.as_str())
                                        .map(|v| (a.as_str().to_string(), v.clone()))
                                })
                                .collect();
                            state.steps.insert(
                                spec.id.clone(),
                                StepState {
                                    input_versions,
                                    output_versions: resolved.into_iter().collect(),
                                    succeeded: true,
                                    fingerprint: graph.fingerprints[i].clone(),
                                },
                            );
                            StepStatus::Succeeded { changed }
                        }
                        Err(e) => {
                            // Contract violation (reported on an
                            // undeclared output, or hashing failed).
                            errors[i] = Some(format!("{e:#}"));
                            StepStatus::Failed {
                                kind: FailureKind::Data,
                            }
                        }
                    }
                }
                Err(step_err) => {
                    // A failed incremental step may still have
                    // committed partial output; record what it vouched
                    // for so the next run sees the movement. (Only the
                    // explicitly reported artifacts — unreported ones
                    // may be mid-write and get re-hashed next run.)
                    if !step_err.outputs.is_empty() {
                        if let Ok(resolved) = resolve_outputs(
                            &self.data_root,
                            spec,
                            &graph.fingerprints[i],
                            &step_err.outputs,
                        ) {
                            let entry = state.steps.entry(spec.id.clone()).or_default();
                            for (path, v) in resolved {
                                entry.output_versions.insert(path, v);
                            }
                        }
                    }
                    errors[i] = Some(format!("{:#}", step_err.error));
                    StepStatus::Failed {
                        kind: step_err.kind,
                    }
                }
            };
            self.finish(graph, &mut status, i, st, errors[i].clone());
            release_dependents(graph, &mut remaining_deps, &mut ready, i);
            // Persist after every terminal step so a crash mid-run
            // keeps the completed steps' bookkeeping.
            state.save(&self.data_root).context("save dag state")?;
        }

        state.save(&self.data_root).context("save dag state")?;

        let steps = graph
            .topo
            .iter()
            .map(|&i| {
                let spec = &graph.steps[i];
                StepReport {
                    id: spec.id.clone(),
                    status: status[i]
                        .clone()
                        .expect("all steps reached a terminal state"),
                    attempts: attempts_taken[i],
                    error: errors[i].clone(),
                    outputs: spec
                        .outputs
                        .iter()
                        .map(|o| {
                            let path = o.as_str().to_string();
                            let now = versions
                                .get(&path)
                                .cloned()
                                .or_else(|| {
                                    state
                                        .steps
                                        .get(&spec.id)
                                        .and_then(|s| s.output_versions.get(&path).cloned())
                                })
                                .unwrap_or_else(|| "unknown".into());
                            let changed = changed_now.get(&path).copied().unwrap_or(false);
                            (path, now, changed)
                        })
                        .collect(),
                }
            })
            .collect();
        let report = RunReport { steps };
        // Terminal machine-readable record of the whole run — the
        // stream-side replacement for the old summary JSON file.
        self.sink.emit(&Event::RunSummary {
            steps: report.steps.iter().map(step_summary).collect(),
        });
        Ok(report)
    }

    /// The runnable subgraph: the source steps this run selected plus
    /// their transitive dependents. With no subset-sync selection that
    /// is every step.
    ///
    /// This is reachability in the graph, computed once before anything
    /// runs, and deliberately independent of run-time state — not what
    /// succeeded before, not whether an input exists, not what happened
    /// to run earlier this pass. It is what makes "sync yolink" mean
    /// the same thing every time: the set of steps that can move is a
    /// property of the config, readable off the DAG, rather than
    /// something you reconstruct from the state file to predict.
    ///
    /// The cost is that pending work elsewhere stays pending — a source
    /// whose render failed yesterday isn't dragged along by an
    /// unrelated sync. That's the intended trade: it comes back on the
    /// next full run, and in exchange a per-source sync never does
    /// surprising work on someone else's chain.
    ///
    /// Steps outside it are still walked, because an in-subgraph fan-in
    /// can depend on them: walking publishes their recorded output
    /// versions (so consumers compare against the right thing) and
    /// gives every step a terminal status for the report. They are
    /// never invoked.
    fn runnable_subgraph(&self, graph: &Graph) -> Vec<bool> {
        let n = graph.steps.len();
        let Some(only) = &self.only_fringe else {
            return vec![true; n];
        };
        let mut scope = vec![false; n];
        let mut queue: VecDeque<usize> = (0..n)
            .filter(|&i| only.contains(&graph.steps[i].id))
            .collect();
        for &i in &queue {
            scope[i] = true;
        }
        while let Some(i) = queue.pop_front() {
            for &d in &graph.dependents[i] {
                if !scope[d] {
                    scope[d] = true;
                    queue.push_back(d);
                }
            }
        }
        scope
    }

    fn decide(
        &self,
        graph: &Graph,
        state: &DagState,
        status: &[Option<StepStatus>],
        runnable: &[bool],
        versions: &HashMap<String, String>,
        i: usize,
    ) -> Decision {
        // Subtree poisoning: any non-ok dependency blocks this step.
        for &d in &graph.deps[i] {
            let dep_status = status[d]
                .as_ref()
                .expect("ready step implies all deps terminal");
            if !dep_status.is_ok() {
                return Decision::Block {
                    on: graph.steps[d].id.clone(),
                };
            }
        }

        // Outside the runnable subgraph: this run didn't ask for it, so
        // it is not considered at all — not its state, not its inputs.
        // "Sync yolink" means run yolink and leave the rest of the graph
        // alone, including work that is genuinely pending elsewhere (a
        // source downloaded yesterday whose render failed). Its outputs
        // keep their recorded versions, so nothing downstream is
        // spuriously dirtied, and the next full run picks it back up.
        if !runnable[i] {
            return Decision::Skip {
                status: StepStatus::NotSelected,
            };
        }

        let spec = &graph.steps[i];
        let prev = state.steps.get(&spec.id);

        // Clause 1: no declared inputs. Its real input is outside the
        // graph — a remote service for a download, a staged directory
        // for a synthesized `staged:` step — so the scheduler cannot
        // version it and always runs the step. Internal incrementality
        // is what makes that cheap.
        let no_inputs = spec.inputs.is_empty();
        // Clause 2: never completed. Aborted last run, failed last run,
        // or added to the config since — all the same fact, and all
        // reasons to run. This overlaps clause 4 today (a step with no
        // recorded success has no recorded fingerprint either, and the
        // empty string differs from every real hash), but it is the
        // honest statement of the rule and shouldn't lean on that
        // coincidence.
        let never_succeeded = !prev.map(|s| s.succeeded).unwrap_or(false);
        // Clause 4: the step itself changed. Editing `params` in the
        // config changes the argv the runner would execute, so the
        // fingerprint moves and the step is stale even though nothing
        // it reads did. State written before fingerprints existed has
        // an empty string here, which differs from any real hash and
        // costs one re-run.
        let fingerprint_changed = prev
            .map(|s| s.fingerprint != graph.fingerprints[i])
            .unwrap_or(true);

        // Clause 3: an input moved. Only meaningful against a recorded
        // success — with no baseline there is nothing to compare, which
        // is what clause 2 is for.
        let mut changed_inputs = Vec::new();
        if let Some(prev) = prev.filter(|p| p.succeeded) {
            for a in &graph.resolved_inputs[i] {
                let now = versions.get(a.as_str());
                let before = prev.input_versions.get(a.as_str());
                match (now, before) {
                    (Some(nv), Some(bv)) if nv == bv => {}
                    // Newly declared input, version moved, or (defensively)
                    // no current version — treat as changed.
                    _ => changed_inputs.push(a.clone()),
                }
            }
        }

        let stale =
            no_inputs || never_succeeded || fingerprint_changed || !changed_inputs.is_empty();
        if !stale {
            return Decision::Skip {
                status: StepStatus::SkippedUpToDate,
            };
        }
        Decision::Run {
            ctx: StepCtx {
                step_id: spec.id.clone(),
                data_root: self.data_root.clone(),
                inputs: graph.resolved_inputs[i].clone(),
                // "What moved" only means something when the step is
                // running *because* something moved. If it never
                // succeeded, or its own definition changed, it should
                // redo all of its work.
                changed_inputs: if never_succeeded || fingerprint_changed {
                    vec![]
                } else {
                    changed_inputs
                },
                progress: StepProgress::new(spec.id.clone(), self.sink.clone()),
            },
        }
    }

    fn finish(
        &self,
        graph: &Graph,
        status: &mut [Option<StepStatus>],
        i: usize,
        st: StepStatus,
        error: Option<String>,
    ) {
        self.sink.emit(&Event::StepFinish {
            step: graph.steps[i].id.clone(),
            status: st.as_str().to_string(),
            error,
        });
        status[i] = Some(st);
    }
}

fn step_summary(r: &StepReport) -> crate::events::StepSummary {
    let failure = match &r.status {
        StepStatus::Failed { kind } => serde_json::to_value(kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string)),
        _ => None,
    };
    crate::events::StepSummary {
        step: r.id.clone(),
        status: r.status.as_str().to_string(),
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
/// Two cases, and that is the whole protocol: the step supplied a
/// version, or it didn't and we hash the tree. Hashing is always
/// correct and always slower — it reads every file under the output —
/// so first-party steps report a version for everything they declare.
///
/// The step's `fingerprint` is folded into every recorded version. A
/// step reports on its *content*, and it has no way to know that its
/// own definition changed — the runner never tells it. Without this, a
/// bumped `code_version` re-runs the step (its fingerprint moved) but
/// leaves the reported version identical, so consumers skip: the tree
/// gets rebuilt while the index keeps serving what the old definition
/// produced. Folding it in makes "produced by a different step" count
/// as a change downstream, which is the conservative direction.
fn resolve_outputs(
    data_root: &std::path::Path,
    spec: &StepSpec,
    fingerprint: &str,
    reported: &[ArtifactState],
) -> Result<Vec<(String, String)>> {
    let mut by_path: BTreeMap<&str, &ArtifactState> = BTreeMap::new();
    for r in reported {
        if !spec.outputs.iter().any(|o| o.as_str() == r.path.as_str()) {
            anyhow::bail!(
                "step {:?} reported on {:?}, which is not a declared output",
                spec.id,
                r.path.as_str()
            );
        }
        by_path.insert(r.path.as_str(), r);
    }
    let mut out = Vec::new();
    for o in &spec.outputs {
        let path = o.as_str();
        let v = match by_path.get(path) {
            // The step vouched for a version: trust it. The mechanics
            // behind it (row-set hash, dolt commit, cursor hash) stay
            // the step's business.
            Some(a) => a.version.clone(),
            // Said nothing about this output: decide for ourselves.
            None => tree_version(&data_root.join(path))?,
        };
        out.push((path.to_string(), format!("{fingerprint}:{v}")));
    }
    Ok(out)
}

/// Mark dependents of `i` ready once all their deps are terminal.
/// Poisoned dependents still flow through `decide` (as `Block`) so the
/// report stays exhaustive: every step gets a terminal status.
fn release_dependents(
    graph: &Graph,
    remaining_deps: &mut [usize],
    ready: &mut VecDeque<usize>,
    i: usize,
) {
    for &j in &graph.dependents[i] {
        remaining_deps[j] -= 1;
        if remaining_deps[j] == 0 {
            ready.push_back(j);
        }
    }
}

async fn invoke_with_retry(
    run: &StepRun,
    ctx: StepCtx,
    retry: &RetryPolicy,
    sink: &Arc<dyn EventSink>,
    child_env: &BTreeMap<String, String>,
) -> (u32, Result<StepOutcome, StepError>) {
    let mut attempt = 1u32;
    loop {
        sink.emit(&Event::StepStart {
            step: ctx.step_id.clone(),
            attempt,
        });
        let res = match run {
            StepRun::InProcess(f) => f(ctx.clone()).await,
            StepRun::Subprocess { argv, env } => {
                crate::subprocess::run_subprocess(argv, env, child_env, &ctx, sink).await
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

    /// A download step writing `content` to `<name>/raw/data.txt`
    /// every invocation, honestly reporting whether it changed. Counts
    /// invocations.
    fn download(name: &str, content: Arc<Mutex<String>>, runs: Arc<AtomicU32>) -> StepSpec {
        let out = format!("{name}/raw");
        StepSpec::new(
            format!("{name}.download"),
            StepRun::in_process(move |ctx: StepCtx| {
                let content = content.clone();
                let runs = runs.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let dir = ctx.path_str(&format!(
                        "{}/raw",
                        ctx.step_id.strip_suffix(".download").unwrap()
                    ));
                    std::fs::create_dir_all(&dir).unwrap();
                    let file = dir.join("data.txt");
                    let new = content.lock().unwrap().clone();
                    std::fs::write(&file, &new).unwrap();
                    ctx.progress.set_length(Some(1));
                    ctx.progress.inc(1);
                    let pat = crate::ArtifactPat::parse(&format!(
                        "{}/raw",
                        ctx.step_id.strip_suffix(".download").unwrap()
                    ))
                    .unwrap();
                    // Stands in for a real download reporting its raw
                    // store's dolt commit: derived from what was
                    // written, so an unchanged poll reports the same
                    // string without the step having to remember it.
                    let version = blake3::hash(new.as_bytes()).to_hex().to_string();
                    Ok(StepOutcome {
                        outputs: vec![ArtifactState::versioned(&pat, version)],
                    })
                }
            }),
        )
        .output(&out)
    }

    /// A render step copying `<name>/raw/data.txt` →
    /// `<name>/rendered_md/data.md`, uppercased. Reports nothing (the
    /// scheduler content-hashes). Counts invocations.
    fn render(name: &str, runs: Arc<AtomicU32>) -> StepSpec {
        let (inp, out) = (format!("{name}/raw"), format!("{name}/rendered_md"));
        let name = name.to_string();
        StepSpec::new(
            format!("{name}.render"),
            StepRun::in_process(move |ctx: StepCtx| {
                let runs = runs.clone();
                let name = name.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let src = ctx.path_str(&format!("{name}/raw/data.txt"));
                    let dir = ctx.path_str(&format!("{name}/rendered_md"));
                    std::fs::create_dir_all(&dir).unwrap();
                    let text = std::fs::read_to_string(&src)
                        .map_err(|e| StepError::new(FailureKind::Data, e))?;
                    std::fs::write(dir.join("data.md"), text.to_uppercase()).unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        )
        .input(&inp)
        .output(&out)
    }

    /// The fan-in index step: concatenates every `*/rendered_md`
    /// tree's files into `system/backend_index/index.txt`. Wildcard
    /// input. Counts invocations and remembers `changed_inputs`.
    fn index(runs: Arc<AtomicU32>, seen_changed: Arc<Mutex<Vec<String>>>) -> StepSpec {
        StepSpec::new(
            "index",
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
                    let dir = ctx.path_str("system/backend_index");
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join("index.txt"), combined).unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        )
        .input("**/rendered_md")
        .output("system/backend_index")
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
                    "slack.download",
                    "email.download",
                    "slack.render",
                    "email.render",
                    "index",
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
                    self.runs["slack.download"].clone(),
                ),
                render("slack", self.runs["slack.render"].clone()),
                download(
                    "email",
                    self.email_content.clone(),
                    self.runs["email.download"].clone(),
                ),
                render("email", self.runs["email.render"].clone()),
                index(
                    self.runs["index"].clone(),
                    self.index_changed_inputs.clone(),
                ),
            ])
            .unwrap()
        }

        fn run_count(&self, id: &str) -> u32 {
            self.runs[id].load(Ordering::SeqCst)
        }
    }

    #[tokio::test]
    async fn first_run_runs_everything_second_run_skips_downstream() {
        let fx = Fixture::new();
        let g = fx.graph();
        let r = runner(fx.root.path());

        let rep1 = r.run(&g).await.unwrap();
        assert!(rep1.all_ok(), "{rep1:#?}");
        for id in [
            "slack.download",
            "slack.render",
            "email.download",
            "email.render",
            "index",
        ] {
            assert_eq!(fx.run_count(id), 1, "{id} should have run once");
            assert!(
                matches!(rep1.step(id).status, StepStatus::Succeeded { .. }),
                "{id}: {:?}",
                rep1.step(id).status
            );
        }
        let idx = fx.root.path().join("system/backend_index/index.txt");
        assert_eq!(
            std::fs::read_to_string(&idx).unwrap(),
            "EMAIL V1\nSLACK V1\n"
        );

        // Nothing changed upstream: downloads are re-invoked (they must
        // poll the remote) but report unchanged; everything downstream
        // skips.
        let rep2 = r.run(&g).await.unwrap();
        assert!(rep2.all_ok(), "{rep2:#?}");
        assert_eq!(fx.run_count("slack.download"), 2);
        assert_eq!(fx.run_count("email.download"), 2);
        assert_eq!(fx.run_count("slack.render"), 1, "render must skip");
        assert_eq!(fx.run_count("email.render"), 1, "render must skip");
        assert_eq!(fx.run_count("index"), 1, "index must skip");
        assert_eq!(
            rep2.step("slack.render").status,
            StepStatus::SkippedUpToDate
        );
        assert_eq!(rep2.step("index").status, StepStatus::SkippedUpToDate);
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

        assert_eq!(fx.run_count("slack.render"), 2, "slack chain reruns");
        assert_eq!(fx.run_count("email.render"), 1, "email chain skips");
        assert_eq!(fx.run_count("index"), 2, "fan-in reruns");
        // The fan-in saw exactly which input moved.
        assert_eq!(
            *fx.index_changed_inputs.lock().unwrap(),
            vec!["slack/rendered_md".to_string()]
        );
        let idx = fx.root.path().join("system/backend_index/index.txt");
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
        let r2 = runner(fx.root.path()).only_fringe(["slack.download".to_string()]);
        let rep = r2.run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");

        assert_eq!(fx.run_count("slack.download"), 2);
        assert_eq!(fx.run_count("email.download"), 1, "email must not sync");
        assert_eq!(rep.step("email.download").status, StepStatus::NotSelected);
        assert_eq!(rep.step("email.render").status, StepStatus::NotSelected);
        assert_eq!(fx.run_count("slack.render"), 2);
        assert_eq!(fx.run_count("index"), 2);
        // The index saw only the synced chain as changed, and the
        // output still carries email's OLD content.
        assert_eq!(
            *fx.index_changed_inputs.lock().unwrap(),
            vec!["slack/rendered_md".to_string()]
        );
        let idx = fx.root.path().join("system/backend_index/index.txt");
        assert_eq!(
            std::fs::read_to_string(&idx).unwrap(),
            "EMAIL V1\nSLACK V2\n"
        );

        // A full run afterwards picks up email's pending change.
        let rep = r.run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(fx.run_count("email.download"), 2);
        assert_eq!(fx.run_count("email.render"), 2);
        assert_eq!(fx.run_count("slack.render"), 2, "slack unchanged now");
        assert_eq!(
            std::fs::read_to_string(&idx).unwrap(),
            "EMAIL V2\nSLACK V2\n"
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
        let r = runner(fx.root.path()).only_fringe(["slack.download".to_string()]);
        let rep = r.run(&g).await.unwrap();

        // The selected chain does its work.
        assert_eq!(fx.run_count("slack.download"), 1);
        assert_eq!(fx.run_count("slack.render"), 1);

        // The unselected chain must not be touched at all — not the
        // download (that part already worked), and not the render.
        assert_eq!(rep.step("email.download").status, StepStatus::NotSelected);
        assert_eq!(
            fx.run_count("email.render"),
            0,
            "render of an unselected chain must not be invoked: its raw \
             store does not exist yet"
        );
        assert_eq!(rep.step("email.render").status, StepStatus::NotSelected);

        // ...so nothing is poisoned, and the fan-in still indexes the
        // chain the user asked to sync.
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(fx.run_count("index"), 1);
        assert_eq!(
            std::fs::read_to_string(fx.root.path().join("system/backend_index/index.txt")).unwrap(),
            "SLACK V1\n"
        );
    }

    /// Out of scope means out of scope, even for a step whose input is
    /// external — staged by the user, no producer in the graph. "Sync
    /// slack" doesn't reach `takeout.render`, so it doesn't run, and
    /// the fan-in still indexes what slack produced.
    #[tokio::test]
    async fn subset_sync_skips_an_out_of_scope_step_with_an_external_input() {
        let fx = Fixture::new();
        let runs = Arc::new(AtomicU32::new(0));
        let staged = fx.root.path().join("takeout/staged_zip");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("data.txt"), "takeout v1").unwrap();

        let takeout = {
            let runs = runs.clone();
            StepSpec::new(
                "takeout.render",
                StepRun::in_process(move |ctx: StepCtx| {
                    let runs = runs.clone();
                    async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        let text =
                            std::fs::read_to_string(ctx.path_str("takeout/staged_zip/data.txt"))
                                .map_err(|e| StepError::new(FailureKind::Data, e))?;
                        let dir = ctx.path_str("takeout/rendered_md");
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("data.md"), text.to_uppercase()).unwrap();
                        Ok(StepOutcome::default())
                    }
                }),
            )
            .input("takeout/staged_zip")
            .output("takeout/rendered_md")
        };

        let g = Graph::build(vec![
            download(
                "slack",
                fx.slack_content.clone(),
                fx.runs["slack.download"].clone(),
            ),
            render("slack", fx.runs["slack.render"].clone()),
            download(
                "email",
                fx.email_content.clone(),
                fx.runs["email.download"].clone(),
            ),
            render("email", fx.runs["email.render"].clone()),
            takeout,
            index(fx.runs["index"].clone(), fx.index_changed_inputs.clone()),
        ])
        .unwrap();
        let ti = g
            .steps
            .iter()
            .position(|s| s.id.as_str() == "takeout.render")
            .unwrap();
        assert!(
            g.deps[ti]
                .iter()
                .any(|&d| g.steps[d].id.starts_with(crate::graph::STAGED_STEP_PREFIX)),
            "fixture must exercise the staged-input path"
        );

        let r = runner(fx.root.path()).only_fringe(["slack.download".to_string()]);
        let rep = r.run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "a step no selected download feeds is out of scope"
        );
        assert_eq!(rep.step("takeout.render").status, StepStatus::NotSelected);
        assert_eq!(fx.run_count("email.render"), 0);
        assert_eq!(
            std::fs::read_to_string(fx.root.path().join("system/backend_index/index.txt")).unwrap(),
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
            "email.render",
            StepRun::in_process(|_ctx| async {
                Err(StepError::new(
                    FailureKind::Data,
                    anyhow::anyhow!("boom: unparseable row"),
                ))
            }),
        )
        .input("email/raw")
        .output("email/rendered_md");
        let broken = Graph::build(vec![
            download(
                "slack",
                fx.slack_content.clone(),
                fx.runs["slack.download"].clone(),
            ),
            render("slack", fx.runs["slack.render"].clone()),
            download(
                "email",
                fx.email_content.clone(),
                fx.runs["email.download"].clone(),
            ),
            failing_email_render,
            index(fx.runs["index"].clone(), fx.index_changed_inputs.clone()),
        ])
        .unwrap();

        // Run 1 (full): email downloads fine, its render fails. Now
        // `email/raw` is real but has never been rendered.
        let rep1 = runner(fx.root.path()).run(&broken).await.unwrap();
        assert!(!rep1.all_ok());
        assert!(matches!(
            rep1.step("email.render").status,
            StepStatus::Failed { .. }
        ));

        // Run 2: sync slack only. The email chain is untouched — no
        // poll, no retry — and slack still reaches the index.
        let g = fx.graph();
        let r2 = runner(fx.root.path()).only_fringe(["slack.download".to_string()]);
        let rep2 = r2.run(&g).await.unwrap();
        assert!(rep2.all_ok(), "{rep2:#?}");
        assert_eq!(fx.run_count("email.download"), 1, "no poll");
        assert_eq!(fx.run_count("email.render"), 0, "no retry");
        assert_eq!(rep2.step("email.render").status, StepStatus::NotSelected);
        // The index was blocked in run 1 (email.render failed), so this
        // is its first run: slack reaches it, email contributes nothing.
        assert_eq!(fx.run_count("index"), 1);

        // Run 3 (full): the pending render is picked back up.
        let rep3 = runner(fx.root.path()).run(&g).await.unwrap();
        assert!(rep3.all_ok(), "{rep3:#?}");
        assert_eq!(fx.run_count("email.render"), 1, "full run recovers it");
        assert_eq!(
            std::fs::read_to_string(fx.root.path().join("system/backend_index/index.txt")).unwrap(),
            "EMAIL V1\nSLACK V1\n"
        );
    }

    /// A step's config changed but its inputs did not: it re-runs.
    ///
    /// Without this, editing `[steps.params]` in `config.toml` — a
    /// widened date range, a changed render knob — silently does
    /// nothing until some input happens to move, and the tree keeps
    /// serving output built under the old config.
    #[tokio::test]
    async fn config_change_reruns_the_step_with_unchanged_inputs() {
        let fx = Fixture::new();
        let runs = Arc::new(AtomicU32::new(0));
        // Same id, same inputs, same outputs — only the step's own
        // definition differs, which is what a params edit amounts to.
        let render_v = |tag: &'static str, runs: Arc<AtomicU32>| {
            StepSpec::new(
                "slack.render",
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
            .output("slack/rendered_md")
            .code_version(tag)
        };
        let graph_with = |tag: &'static str, runs: Arc<AtomicU32>| {
            Graph::build(vec![
                download(
                    "slack",
                    fx.slack_content.clone(),
                    fx.runs["slack.download"].clone(),
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
            rep2.step("slack.render").status,
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
                rep3.step("slack.render").status,
                StepStatus::Succeeded { .. }
            ),
            "a config change must re-run the step: {:?}",
            rep3.step("slack.render").status
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
                fx.runs["slack.download"].clone(),
            ),
            render("slack", fx.runs["slack.render"].clone()),
        ])
        .unwrap();
        assert!(runner(fx.root.path()).run(&g1).await.unwrap().all_ok());

        let audit_runs = Arc::new(AtomicU32::new(0));
        let ar = audit_runs.clone();
        let audit = StepSpec::new(
            "slack.audit",
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
        .input("slack/raw")
        .output("slack/audit");

        let g2 = Graph::build(vec![
            download(
                "slack",
                fx.slack_content.clone(),
                fx.runs["slack.download"].clone(),
            ),
            render("slack", fx.runs["slack.render"].clone()),
            audit,
        ])
        .unwrap();
        let rep = runner(fx.root.path()).run(&g2).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        // The download re-polled and found nothing new...
        assert!(
            matches!(
                rep.step("slack.download").status,
                StepStatus::Succeeded { changed: 0 }
            ),
            "{:?}",
            rep.step("slack.download").status
        );
        // ...the existing render is up to date...
        assert_eq!(rep.step("slack.render").status, StepStatus::SkippedUpToDate);
        // ...and the new step still runs.
        assert_eq!(audit_runs.load(Ordering::SeqCst), 1);
        assert!(fx.root.path().join("slack/audit/a.txt").is_file());
    }

    /// An input that was real and then went away. Its version moves
    /// from a content hash to `absent`, which is a difference like any
    /// other, so its consumer re-runs and gets a chance to drop the
    /// output built from data that no longer exists.
    #[tokio::test]
    async fn deleted_input_reruns_its_consumer() {
        let root = tempfile::tempdir().unwrap();
        let staged = root.path().join("takeout/staged");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("chat.json"), "v1").unwrap();

        let runs = Arc::new(AtomicU32::new(0));
        let rn = runs.clone();
        let step = StepSpec::new(
            "takeout.render",
            StepRun::in_process(move |ctx: StepCtx| {
                let rn = rn.clone();
                async move {
                    rn.fetch_add(1, Ordering::SeqCst);
                    let dir = ctx.path_str("takeout/rendered_md");
                    std::fs::create_dir_all(&dir).unwrap();
                    let out = std::path::Path::new(&dir).join("chat.md");
                    match std::fs::read_to_string(ctx.path_str("takeout/staged/chat.json")) {
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
        .input("takeout/staged")
        .output("takeout/rendered_md");

        let g = Graph::build(vec![step]).unwrap();
        let r = runner(root.path());
        assert!(r.run(&g).await.unwrap().all_ok());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(root.path().join("takeout/rendered_md/chat.md").is_file());

        // The user deletes the staged export.
        std::fs::remove_dir_all(&staged).unwrap();

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
            rep.step("takeout.render").status,
            StepStatus::SkippedUpToDate
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failure_poisons_subtree_but_not_siblings() {
        let fx = Fixture::new();
        // Break slack.render by removing its input mid-way: simplest is
        // a fresh graph where slack.render always fails.
        let failing_render = StepSpec::new(
            "slack.render",
            StepRun::in_process(|_ctx| async {
                Err(StepError::new(
                    FailureKind::Data,
                    anyhow::anyhow!("boom: unparseable row"),
                ))
            }),
        )
        .input("slack/raw")
        .output("slack/rendered_md");
        let g = Graph::build(vec![
            download(
                "slack",
                fx.slack_content.clone(),
                fx.runs["slack.download"].clone(),
            ),
            failing_render,
            download(
                "email",
                fx.email_content.clone(),
                fx.runs["email.download"].clone(),
            ),
            render("email", fx.runs["email.render"].clone()),
            index(fx.runs["index"].clone(), fx.index_changed_inputs.clone()),
        ])
        .unwrap();

        let rep = runner(fx.root.path()).run(&g).await.unwrap();
        assert!(!rep.all_ok());
        assert_eq!(
            rep.step("slack.render").status,
            StepStatus::Failed {
                kind: FailureKind::Data
            }
        );
        // Data errors don't retry.
        assert_eq!(rep.step("slack.render").attempts, 1);
        // The sibling chain still ran to completion...
        assert!(matches!(
            rep.step("email.render").status,
            StepStatus::Succeeded { .. }
        ));
        // ...but the fan-in below the failure is blocked, not run.
        assert_eq!(
            rep.step("index").status,
            StepStatus::Blocked {
                on: "slack.render".to_string()
            }
        );
        assert_eq!(fx.run_count("index"), 0);
    }

    #[tokio::test]
    async fn transient_failures_retry_then_succeed() {
        let root = tempfile::tempdir().unwrap();
        let attempts_seen = Arc::new(AtomicU32::new(0));
        let a = attempts_seen.clone();
        let flaky = StepSpec::new(
            "flaky.download",
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
        )
        .output("flaky/raw");
        let g = Graph::build(vec![flaky]).unwrap();
        let rep = runner(root.path()).run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(rep.step("flaky.download").attempts, 3);
    }

    #[tokio::test]
    async fn failed_step_stays_dirty_and_recovers_next_run() {
        let root = tempfile::tempdir().unwrap();
        let fail_now = Arc::new(Mutex::new(true));
        let runs = Arc::new(AtomicU32::new(0));
        let (f, rn) = (fail_now.clone(), runs.clone());
        let dl = StepSpec::new(
            "src.download",
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
                        let pat = crate::ArtifactPat::parse("src/raw").unwrap();
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
        )
        .output("src/raw");
        let render_runs = Arc::new(AtomicU32::new(0));
        let g = Graph::build(vec![dl, render("src", render_runs.clone())]).unwrap();

        let r = runner(root.path());
        let rep1 = r.run(&g).await.unwrap();
        assert_eq!(
            rep1.step("src.download").status,
            StepStatus::Failed {
                kind: FailureKind::Auth
            }
        );
        // Auth doesn't retry.
        assert_eq!(rep1.step("src.download").attempts, 1);
        assert_eq!(
            rep1.step("src.render").status,
            StepStatus::Blocked {
                on: "src.download".to_string()
            }
        );

        // "Fix the credentials" and rerun: everything completes.
        *fail_now.lock().unwrap() = false;
        let rep2 = r.run(&g).await.unwrap();
        assert!(rep2.all_ok(), "{rep2:#?}");
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(render_runs.load(Ordering::SeqCst), 1);
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
            Event::ProgressInc { step, .. } if step == "slack.download"
        )));
        let finishes = events
            .iter()
            .filter(|e| matches!(e, Event::StepFinish { .. }))
            .count();
        assert_eq!(finishes, 5);
    }

    #[tokio::test]
    async fn external_input_change_triggers_rerun() {
        // A render-only pipeline over a user-staged tree (no download
        // step) — the `--skip-extract` / pre-staged `input_path` shape.
        let root = tempfile::tempdir().unwrap();
        let staged = root.path().join("takeout/staged");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("chat.json"), "v1").unwrap();

        let runs = Arc::new(AtomicU32::new(0));
        let rn = runs.clone();
        let step = StepSpec::new(
            "takeout.render",
            StepRun::in_process(move |ctx: StepCtx| {
                let rn = rn.clone();
                async move {
                    rn.fetch_add(1, Ordering::SeqCst);
                    let dir = ctx.path_str("takeout/rendered_md");
                    std::fs::create_dir_all(&dir).unwrap();
                    let text =
                        std::fs::read_to_string(ctx.path_str("takeout/staged/chat.json")).unwrap();
                    std::fs::write(dir.join("chat.md"), text).unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        )
        .input("takeout/staged")
        .output("takeout/rendered_md");

        let g = Graph::build(vec![step]).unwrap();
        let r = runner(root.path());
        r.run(&g).await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Unchanged staged tree → skip.
        let rep = r.run(&g).await.unwrap();
        assert_eq!(
            rep.step("takeout.render").status,
            StepStatus::SkippedUpToDate
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Edit the staged tree → rerun.
        std::fs::write(staged.join("chat.json"), "v2").unwrap();
        let rep = r.run(&g).await.unwrap();
        assert!(matches!(
            rep.step("takeout.render").status,
            StepStatus::Succeeded { changed: 1 }
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }
}
