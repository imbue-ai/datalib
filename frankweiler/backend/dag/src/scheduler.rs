//! The runner: executes a [`Graph`] with bounded parallelism,
//! skipping steps whose inputs are unchanged, retrying failures by
//! kind, and poisoning the subtree below a failure.
//!
//! Scheduling semantics (from the design doc + addendum):
//!
//! * A step with no inputs (a download step — its real input is the
//!   remote service, which the scheduler can't version) is always
//!   invoked; its internal incrementality makes that cheap, and it
//!   reports whether its outputs actually changed.
//! * A step with inputs is invoked iff it has never succeeded or some
//!   input artifact's version differs from what it saw at its last
//!   success. An invoked step may still report all outputs unchanged,
//!   in which case its dependents skip.
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
    /// Subset-sync mode: when set, fringe steps (those with no
    /// declared inputs — the download steps, whose real input is a
    /// remote service) NOT in this set are treated as up to date
    /// instead of being invoked. Downstream steps still follow normal
    /// change propagation, so a shared fan-in (index/qmd) re-runs iff
    /// one of the selected chains actually moved. `None` (the
    /// default) invokes every fringe step.
    pub only_fringe: Option<std::collections::HashSet<String>>,
}

impl Runner {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            parallelism: 4,
            sink: Arc::new(NoopSink),
            retry: RetryPolicy::default(),
            only_fringe: None,
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
}

/// Terminal state of one step in one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    /// Ran to completion. `changed` = number of outputs whose version
    /// moved relative to the previous run.
    Succeeded {
        changed: usize,
    },
    /// Inputs unchanged since last success; not invoked.
    SkippedUpToDate,
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
            StepStatus::Blocked { .. } => "blocked",
            StepStatus::Failed { .. } => "failed",
        }
    }
    pub fn is_ok(&self) -> bool {
        matches!(
            self,
            StepStatus::Succeeded { .. } | StepStatus::SkippedUpToDate
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
    Run { ctx: StepCtx },
    Skip,
    Block { on: String },
}

impl Runner {
    pub async fn run(&self, graph: &Graph) -> Result<RunReport> {
        let mut state = DagState::load(&self.data_root).context("load dag state")?;

        let n = graph.steps.len();
        // Current version of every concrete artifact, filled in as
        // producers reach a terminal state. Externals have no producer
        // to report on them, so they're hashed up front.
        let mut versions: HashMap<String, String> = HashMap::new();
        // Whether each artifact's version moved this run (drives the
        // per-output `changed` flag in the report).
        let mut changed_now: HashMap<String, bool> = HashMap::new();
        for exts in &graph.external_inputs {
            for a in exts {
                let v = tree_version(&self.data_root.join(a.as_str()))
                    .with_context(|| format!("hash external input {a}"))?;
                versions.insert(a.as_str().to_string(), v);
            }
        }

        let mut status: Vec<Option<StepStatus>> = vec![None; n];
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
                match self.decide(graph, &state, &status, &versions, i) {
                    Decision::Skip => {
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
                        self.finish(graph, &mut status, i, StepStatus::SkippedUpToDate, None);
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
                        set.spawn(async move {
                            let (attempts, res) = invoke_with_retry(&run, ctx, &retry, &sink).await;
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
                    match resolve_outputs(&self.data_root, spec, &outcome.outputs, &prior_outs) {
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
                        if let Ok(resolved) =
                            resolve_outputs(&self.data_root, spec, &step_err.outputs, &prior_outs)
                        {
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

    fn decide(
        &self,
        graph: &Graph,
        state: &DagState,
        status: &[Option<StepStatus>],
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

        let spec = &graph.steps[i];
        let prev = state.steps.get(&spec.id);
        let first_run = !prev.map(|s| s.succeeded).unwrap_or(false);

        let mut changed_inputs = Vec::new();
        for a in &graph.resolved_inputs[i] {
            let now = versions.get(a.as_str());
            let before = prev.and_then(|s| s.input_versions.get(a.as_str()));
            match (now, before) {
                (Some(nv), Some(bv)) if nv == bv => {}
                // New input artifact, version moved, or (defensively)
                // no current version — treat as changed.
                _ => changed_inputs.push(a.clone()),
            }
        }

        // Subset-sync: a fringe step outside the selected set is
        // declared up to date, even on a first run — the user asked
        // for it not to be synced. Its outputs keep their recorded
        // versions, so nothing downstream is spuriously dirtied.
        if spec.inputs.is_empty() {
            if let Some(only) = &self.only_fringe {
                if !only.contains(&spec.id) {
                    return Decision::Skip;
                }
            }
        }

        // Fringe steps (no declared inputs) always run: their real
        // input is a remote service the scheduler can't version.
        let dirty = first_run || spec.inputs.is_empty() || !changed_inputs.is_empty();
        if !dirty {
            return Decision::Skip;
        }
        Decision::Run {
            ctx: StepCtx {
                step_id: spec.id.clone(),
                data_root: self.data_root.clone(),
                inputs: graph.resolved_inputs[i].clone(),
                changed_inputs: if first_run { vec![] } else { changed_inputs },
                first_run,
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
fn resolve_outputs(
    data_root: &std::path::Path,
    spec: &StepSpec,
    reported: &[ArtifactState],
    prior: &BTreeMap<String, String>,
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
            // The step vouched for a version: trust it. This is the
            // "thin declared output" from the design doc — mechanics
            // (row-set hash, dolt commit, …) stay hidden.
            Some(ArtifactState {
                version: Some(v), ..
            }) => v.clone(),
            // The step says "unchanged": carry the prior version
            // forward (hash if we never recorded one).
            Some(ArtifactState {
                changed: Some(false),
                ..
            }) => match prior.get(path) {
                Some(v) => v.clone(),
                None => tree_version(&data_root.join(path))?,
            },
            // Changed-without-version, or no report at all:
            // content-hash the tree.
            _ => tree_version(&data_root.join(path))?,
        };
        out.push((path.to_string(), v));
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
                crate::subprocess::run_subprocess(argv, env, &ctx, sink).await
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
                    let changed = std::fs::read_to_string(&file).ok().as_deref() != Some(&new);
                    std::fs::write(&file, &new).unwrap();
                    ctx.progress.set_length(Some(1));
                    ctx.progress.inc(1);
                    let pat = crate::ArtifactPat::parse(&format!(
                        "{}/raw",
                        ctx.step_id.strip_suffix(".download").unwrap()
                    ))
                    .unwrap();
                    Ok(StepOutcome {
                        outputs: vec![if changed {
                            ArtifactState::changed(&pat)
                        } else {
                            ArtifactState::unchanged(&pat)
                        }],
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
        assert_eq!(
            rep.step("email.download").status,
            StepStatus::SkippedUpToDate
        );
        assert_eq!(rep.step("email.render").status, StepStatus::SkippedUpToDate);
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
                                .with_outputs(vec![ArtifactState::changed(&pat)]),
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
