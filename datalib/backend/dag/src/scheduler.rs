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
//!   nothing about is content hashed instead — the one place the runner
//!   hashes anything, and only ever for a step that just ran.
//! * A step that does *not* run contributes the version recorded for its
//!   output last time, or `version::UNKNOWN` if there is none. The
//!   runner never reads a tree to version it on a step's behalf: it
//!   would be reading gigabytes to answer a question the step can answer
//!   from a commit hash, for work this run already decided not to do.
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
use crate::state::{CurrentRun, DagState, LastRun, StepState};
use crate::step::{
    ArtifactState, FailureKind, StepCtx, StepError, StepId, StepOutcome, StepRun, StepSpec,
};
use crate::version::{tree_version, UNKNOWN};

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
    /// step's output keeps the version recorded for it, so consumers
    /// compare against the right thing — or, with nothing recorded,
    /// [`crate::version::UNKNOWN`]. The runner does not read the tree
    /// to invent one.
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

        // Open the run record before anything runs, so a UI polling
        // mid-plan sees "started, nothing finished" rather than an empty
        // file. One clock for the whole run — the same value the steps
        // get in `DATALIB_DAG_NOW`, so a run's timestamps agree with
        // what its steps stamped into their own stores.
        let started_at = self
            .child_env
            .get(crate::subprocess::ENV_NOW)
            .cloned()
            .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());
        state.current_run = Some(CurrentRun {
            run_id: started_at.clone(),
            started_at: started_at.clone(),
            finished_at: None,
            plan: graph
                .topo
                .iter()
                .map(|&i| graph.steps[i].id.clone())
                .collect(),
            states: Default::default(),
        });
        state.save(&self.data_root).context("save dag state")?;

        let n = graph.steps.len();
        // Current version of every concrete artifact, filled in as
        // producers reach a terminal state. Every artifact has exactly
        // one producer — `Graph::build` rejects an input that names no
        // declared step — and a version is that producer's to report,
        // so the scheduler never hashes a tree on its own behalf.
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
            let mut dispatched = false;
            while running < self.parallelism {
                let Some(i) = ready.pop_front() else { break };
                match self.decide(graph, &state, &status, &runnable, &versions, i) {
                    Decision::Skip { status: st } => {
                        // The output keeps its last-recorded version,
                        // and if there isn't one we say so. There is
                        // deliberately no fallback here: hashing a tree
                        // the run was told not to touch is how a
                        // `--sync` of one small source came to spend
                        // forty seconds reading 3.4 GB of somebody
                        // else's Slack (#225). The cost was invisible
                        // because the fallback *succeeded* — a correct
                        // version, arrived at the slowest possible way,
                        // for a step nobody was going to run.
                        let out = graph.steps[i].output();
                        let v = state
                            .steps
                            .get(&graph.steps[i].id)
                            .and_then(|s| s.output_versions.get(out.as_str()))
                            .cloned()
                            .unwrap_or_else(|| UNKNOWN.to_string());
                        versions.insert(out.as_str().to_string(), v);
                        changed_now.insert(out.as_str().to_string(), false);
                        self.finish(graph, &mut state, &mut status, i, st, None, 0);
                        release_dependents(graph, &mut remaining_deps, &mut ready, i);
                    }
                    Decision::Block { on } => {
                        self.finish(
                            graph,
                            &mut state,
                            &mut status,
                            i,
                            StepStatus::Blocked { on },
                            None,
                            0,
                        );
                        release_dependents(graph, &mut remaining_deps, &mut ready, i);
                    }
                    Decision::Run { ctx } => {
                        running += 1;
                        dispatched = true;
                        mark_running(&mut state, &graph.steps[i].id, &now_stamp());
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

            // Persist the dispatch before waiting on it. `mark_running`
            // only mutates memory, and until this landed the *only*
            // writes were on terminal states — so a step's "running"
            // never reached the file, and the record went straight from
            // "not reached yet" to "succeeded". A reader polling
            // `dag_state.json` (which is every reader: `GET /api/dag`,
            // and so the Manage grid) could therefore never see a step
            // running, no matter how long it ran. Pressing Sync looked
            // like nothing had happened, which is exactly what it was
            // reported as.
            //
            // One write per dispatch batch, not per step, so the budget
            // this file documents — O(steps) per run, state transitions
            // only — still holds.
            if dispatched {
                state.save(&self.data_root).context("save dag state")?;
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
                        &*self.sink,
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
                                    // Carried over rather than defaulted:
                                    // this replaces the whole entry, and
                                    // `started_at` was recorded when the
                                    // step was dispatched. `finish` fills
                                    // in the rest a moment from now.
                                    last_run: state
                                        .steps
                                        .get(&spec.id)
                                        .and_then(|s| s.last_run.clone()),
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
                            &*self.sink,
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
            self.finish(
                graph,
                &mut state,
                &mut status,
                i,
                st,
                errors[i].clone(),
                attempts,
            );
            release_dependents(graph, &mut remaining_deps, &mut ready, i);
            // Persist after every terminal step so a crash mid-run
            // keeps the completed steps' bookkeeping.
            state.save(&self.data_root).context("save dag state")?;
        }

        // Close the run: a reader distinguishes "finished" from "still
        // going" by this field alone, so it has to land before the last
        // save rather than after it.
        if let Some(run) = state.current_run.as_mut() {
            run.finished_at = Some(now_stamp());
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
                    outputs: std::iter::once(spec.output())
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
                                .unwrap_or_else(|| UNKNOWN.to_string());
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
        // graph — a remote service for a download, a hand-staged
        // directory named by `params.common.input_path` — so the
        // scheduler cannot version it and always runs the step.
        // Internal incrementality is what makes that cheap.
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

    /// Terminal state for one step: emit it, record it in `status`, and
    /// write it into the run record.
    ///
    /// Every path a step can end on goes through here — succeeded,
    /// skipped, blocked, failed, not-selected — which is what makes the
    /// run record complete rather than best-effort.
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        graph: &Graph,
        state: &mut DagState,
        status: &mut [Option<StepStatus>],
        i: usize,
        st: StepStatus,
        error: Option<String>,
        attempts: u32,
    ) {
        let id = &graph.steps[i].id;
        self.sink.emit(&Event::StepFinish {
            step: id.clone(),
            status: st.as_str().to_string(),
            error: error.clone(),
        });
        let stamp = now_stamp();
        if let Some(run) = state.current_run.as_mut() {
            run.states.insert(id.clone(), st.as_str().to_string());
        }
        // `NotSelected` is a fact about this *run*, not about the step:
        // the run didn't ask for it, so nothing happened to it. Writing
        // that into `last_run` overwrote a real history — a step that
        // succeeded yesterday came back as "not selected", stamped with
        // the time of a run that never touched it, because every
        // per-source sync walks the whole graph to publish output
        // versions and reaches every step it isn't running.
        //
        // The run record still carries it (`current_run.states` above),
        // which is where "not in this sync" belongs: that map describes
        // one run, and is replaced wholesale by the next.
        if st != StepStatus::NotSelected {
            let entry = state.steps.entry(id.clone()).or_default();
            let last = entry.last_run.get_or_insert_with(|| LastRun {
                started_at: stamp.clone(),
                ..Default::default()
            });
            last.finished_at = Some(stamp);
            last.status = st.as_str().to_string();
            last.attempts = attempts;
            last.error = error;
        }
        status[i] = Some(st);
    }
}

/// A wall-clock stamp in the tree's convention: local time with an
/// explicit offset, per AGENTS.md.
fn now_stamp() -> String {
    datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs()
}

/// Open a step's run record when the scheduler dispatches it, so a
/// reader can tell "running" from "not reached yet".
fn mark_running(state: &mut DagState, id: &StepId, stamp: &str) {
    if let Some(run) = state.current_run.as_mut() {
        run.states.insert(id.clone(), "running".to_string());
    }
    state.steps.entry(id.clone()).or_default().last_run = Some(LastRun {
        started_at: stamp.to_string(),
        finished_at: None,
        status: String::new(),
        attempts: 0,
        error: None,
    });
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
/// This is the runner's only call to [`tree_version`], and it is only
/// reached with a step that just ran. When it fires it says so on the
/// event stream: an unreported version costs a full read of the output
/// tree, and #225 is the case for what a slow path nobody can see costs
/// in the end.
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
                    "reported no version for {path}; reading the whole tree to hash it.                      A version the step derives from what it wrote would be cheaper."
                ),
            });
            tree_version(&data_root.join(path))?
        }
    };
    Ok(vec![(path.to_string(), format!("{fingerprint}:{v}"))])
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

    /// A download step writing `content` to `<id>/data.txt` every
    /// invocation, honestly reporting whether it changed. Counts
    /// invocations.
    ///
    /// Note what it no longer has to do: its id *is* the tree it
    /// writes, so it reads `ctx.step_id` directly instead of stripping
    /// a `.download` suffix off it to rebuild a path.
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
                    })
                }
            }),
        )
    }

    /// A render step copying `<name>/raw/data.txt` →
    /// `<name>/rendered_md/data.md`, uppercased. Reports nothing (the
    /// scheduler content-hashes). Counts invocations.
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
    /// UI's worker write the same file, so the UI can show either.
    ///
    /// What it must carry: the plan before anything runs, a terminal
    /// state for *every* step (including the ones that were skipped or
    /// blocked, which never "ran"), a `finished_at` that distinguishes
    /// a completed run from a crashed one, and per-step timings that
    /// don't need a whole-run timestamp smeared across every source.
    /// The run id is the pinned `DATALIB_DAG_NOW`, verbatim.
    ///
    /// `datalib-dag` mints that value and hands it to the progress bus
    /// as the run id *before* calling `run`, so the two derive the same
    /// string independently. If they ever diverge, nothing errors — the
    /// bus just describes a run nobody is displaying, `/api/dag` filters
    /// every row out on the id mismatch, and the UI silently shows no
    /// progress at all. This is the coupling that keeps that from
    /// happening quietly.
    #[tokio::test]
    async fn the_run_id_is_the_pinned_now_so_the_bus_can_match_it() {
        let fx = Fixture::new();
        let g = fx.graph();
        let pinned = "2026-08-31T12:34:56+02:00";
        let r = runner(fx.root.path()).child_env(BTreeMap::from([(
            crate::subprocess::ENV_NOW.to_string(),
            pinned.to_string(),
        )]));
        assert!(r.run(&g).await.unwrap().all_ok());

        let st = DagState::load(fx.root.path()).unwrap();
        assert_eq!(
            st.current_run.expect("a run leaves a record").run_id,
            pinned,
            "the run id must be DATALIB_DAG_NOW verbatim — the binary \
             passes that same string to the progress bus"
        );
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
    ///
    /// `mark_running` has always written it into the in-memory state,
    /// but the only `save` calls were on terminal states — so
    /// `dag_state.json` went straight from "not reached yet" to
    /// "succeeded", and a reader could never catch a step in flight no
    /// matter how long it ran. That file is the *only* channel to a
    /// reader who did not spawn the run: `GET /api/dag` reads it, and
    /// the Manage grid reads that. Pressing Sync therefore looked like
    /// nothing had happened, all the way until the step finished.
    ///
    /// The assertion is deliberately made from disk, in another task,
    /// while the step is parked — reading `state` in-process would pass
    /// against the broken version.
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
    ///
    /// Every run walks the whole graph — out-of-scope steps still get a
    /// terminal status so the report is complete and so consumers see
    /// their recorded output versions. That walk used to write
    /// `NotSelected` into `last_run` like any other outcome, which meant
    /// a `--sync slack` erased email's record: a step that succeeded
    /// yesterday came back as "not selected", stamped with the time of a
    /// run that never touched it. In the grid that read as a source
    /// whose "last synced" moved every time some *other* source synced.
    ///
    /// `current_run.states` is where "not in this sync" belongs — it
    /// describes one run and is replaced wholesale by the next.
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
    ///
    /// This is the shape #225 was found in: an aborted download leaves
    /// `succeeded: false` and no recorded version behind, with the
    /// store still on disk. The runner used to hash that store to get a
    /// version for a step it had already ruled out of the run — forty
    /// seconds per `--sync`, for a 3.4 GB Slack store, producing a
    /// number nothing in the run compared against.
    ///
    /// The tree here is tiny, so the test asserts *what* the runner
    /// reported rather than how long it took. The two are the same
    /// fact: hashing a tree with a file in it yields a 64-character
    /// digest, and `unknown` is what you get only by not reading the
    /// tree at all.
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

    #[tokio::test]
    async fn failure_poisons_subtree_but_not_siblings() {
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
        // ...but the fan-in below the failure is blocked, not run.
        assert_eq!(
            rep.step("unified_index/grid").status,
            StepStatus::Blocked {
                on: "slack/rendered_md".to_string()
            }
        );
        assert_eq!(fx.run_count("unified_index/grid"), 0);
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
        assert_eq!(
            rep1.step("src/rendered_md").status,
            StepStatus::Blocked {
                on: "src/raw".to_string()
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
            Event::ProgressInc { step, .. } if step == "slack/raw"
        )));
        let finishes = events
            .iter()
            .filter(|e| matches!(e, Event::StepFinish { .. }))
            .count();
        assert_eq!(finishes, 5);
    }
}
