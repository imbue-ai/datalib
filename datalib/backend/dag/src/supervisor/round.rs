//! One round of the supervisor, which is what `datalib-dag` runs: a request
//! rooted at the sources asked for, ticked until it closes. The facts come
//! from `dag_state.json` and go back to it, and the events are the ones the
//! run store and the server already read.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::store::{RequestOutcome, Store};
use super::tick::{
    tick, Attempt, Class, Consumed, Facts, Intent, Outcome, Request, Running, Seq, Shape,
    StepFacts, StepShape, StepState as Row,
};
use crate::artifact::ArtifactPath;
use crate::events::{Event, StepProgress};
use crate::graph::Graph;
use crate::scheduler::{
    invoke_with_retry, mark_running, new_run_id, now_stamp, resolve_outputs, step_summary,
    QueueLedger, RunReport, Runner, StepReport, StepStatus,
};
use crate::state::{CurrentRun, DagState};
use crate::step::{Exit, FailureKind, StepCtx, StepError, StepOutcome, StopSignal};
use crate::version::UNKNOWN;

/// How an invocation ended, held until the step runs again (it was a
/// pass) or everything it reads has settled (it is done for now). While a
/// step it reads is still running or waiting, its row keeps reading
/// Running: the step is not finished, it is waiting for the next seal.
struct Ended {
    status: StepStatus,
    error: Option<String>,
    exit: Option<Exit>,
    attempts: u32,
    /// Whether its process has been closed with a `PassEnd` already.
    pass_ended: bool,
}

type Done = (usize, u32, Result<StepOutcome, StepError>, Consumed);

/// Where the loop's requests and pauses come from.
enum Mailbox<'a> {
    /// One request rooted at the runner's sources, opened by the loop and
    /// closed by it: a round, as `datalib-dag` without a store runs it.
    Fixed { opened: bool },
    /// Whatever `system/supervisor.sqlite` holds, read again whenever
    /// another process writes to it.
    Store { store: &'a Store, seen: Option<i64> },
}

/// An open request as the loop holds it: its row's id (`None` for the
/// fixed one) and the tick's view of it.
struct Open {
    id: Option<String>,
    request: Request,
}

/// How often a loop with steps running looks for new rows. A loop with
/// nothing running is not waiting on anything else, so it looks at the
/// same pace.
const MAILBOX_POLL: std::time::Duration = std::time::Duration::from_millis(250);

impl Runner {
    /// One request rooted at the runner's sources, run until it closes.
    pub async fn run(&self, graph: &Graph) -> Result<RunReport> {
        self.run_loop(graph, Mailbox::Fixed { opened: false }).await
    }

    /// Every request open in the store, and any opened while this runs,
    /// until none is left. The caller holds `runner-lock`.
    pub async fn serve(&self, graph: &Graph, store: &Store) -> Result<RunReport> {
        self.run_loop(graph, Mailbox::Store { store, seen: None })
            .await
    }

    async fn run_loop(&self, graph: &Graph, mut mailbox: Mailbox<'_>) -> Result<RunReport> {
        let plan: Vec<String> = graph
            .topo
            .iter()
            .map(|&i| graph.steps[i].id.clone())
            .collect();
        self.sink.emit(&Event::RunPlan {
            steps: plan.clone(),
        });
        let mut state = DagState::load(&self.data_root).context("load dag state")?;

        // One clock and one id for the round: the same values the steps get
        // in `DATALIB_DAG_NOW` and `DATALIB_DAG_RUN_ID`.
        let started_at = self
            .child_env
            .get(crate::subprocess::ENV_NOW)
            .cloned()
            .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());
        let run_id = self
            .child_env
            .get(crate::subprocess::ENV_RUN_ID)
            .cloned()
            .unwrap_or_else(new_run_id);
        state.current_run = Some(CurrentRun {
            run_id,
            started_at,
            finished_at: None,
            plan,
            states: Default::default(),
        });
        state.save(&self.data_root).context("save dag state")?;

        let n = graph.steps.len();
        let shape = shape_of(graph);
        let mut facts = facts_of(graph, &state);
        let mut open: Vec<Open> = Vec::new();
        let mut paused: BTreeSet<usize> = BTreeSet::new();
        let mut ever_in_scope = vec![false; n];
        let mut seq = 0u64;

        let mut status: Vec<Option<StepStatus>> = vec![None; n];
        let mut attempts_taken = vec![0u32; n];
        let mut errors: Vec<Option<String>> = vec![None; n];
        let mut changed_now: HashMap<String, bool> = HashMap::new();
        let mut ended: Vec<Option<Ended>> = (0..n).map(|_| None).collect();
        let mut stops: Vec<Option<watch::Sender<bool>>> = (0..n).map(|_| None).collect();
        let mut warned_not_streaming = vec![false; n];
        let mut queue = QueueLedger::new(n);
        let mut last_states: Vec<Row> = vec![Row::Idle; n];
        let mut cancelled = false;
        let mut stop_rx = self.stop.clone();

        self.refresh(
            graph,
            &mut mailbox,
            &mut open,
            &mut paused,
            &mut seq,
            &mut ever_in_scope,
        )
        .await?;
        for i in (0..n).filter(|&i| !ever_in_scope[i]) {
            self.finish(
                graph,
                &mut state,
                &mut status,
                i,
                StepStatus::NotSelected,
                None,
                None,
                0,
            );
        }

        let (cp_tx, mut checkpoints) = tokio::sync::mpsc::unbounded_channel();
        let checkpoint = crate::step::CheckpointSink::new(cp_tx);
        let mut set: JoinSet<Done> = JoinSet::new();

        loop {
            if !cancelled {
                self.refresh(
                    graph,
                    &mut mailbox,
                    &mut open,
                    &mut paused,
                    &mut seq,
                    &mut ever_in_scope,
                )
                .await?;
            }
            let intent = Intent {
                requests: open.iter().map(|o| o.request.clone()).collect(),
                paused: paused.clone(),
            };
            let t = tick(&shape, &intent, &facts, &self.budgets);
            if !intent.requests.is_empty() {
                last_states = t.states.clone();
            }
            let starting: BTreeSet<usize> = t.starts.iter().map(|s| s.step).collect();

            // A step between passes has not settled either, so the index
            // behind a render that is waiting on its download's next seal
            // keeps reading Running too, not just the render.
            let mut unsettled = vec![false; n];
            for &i in &graph.topo {
                let upstream = graph.deps_in_order(i).any(|p| unsettled[p]);
                unsettled[i] = matches!(t.states[i], Row::Running | Row::Waiting(_))
                    || (ended[i].is_some() && upstream);
            }

            for (i, slot) in ended.iter_mut().enumerate() {
                let Some(e) = slot.as_mut() else { continue };
                let again = starting.contains(&i);
                let upstream_busy = graph.deps_in_order(i).any(|p| unsettled[p]);
                if (again || upstream_busy) && !e.pass_ended {
                    e.pass_ended = true;
                    self.sink.emit(&Event::PassEnd {
                        step: graph.steps[i].id.clone(),
                        exit_code: e.exit.and_then(|x| x.code),
                        signal: e.exit.and_then(|x| x.signal),
                    });
                }
                if again {
                    *slot = None;
                } else if !upstream_busy {
                    let e = slot.take().expect("checked above");
                    self.finish(
                        graph,
                        &mut state,
                        &mut status,
                        i,
                        e.status,
                        e.error,
                        e.exit,
                        e.attempts,
                    );
                }
            }

            for start in t.starts {
                let i = start.step;
                seq += 1;
                facts.steps[i].running = Some(Running { started: Seq(seq) });
                let (tx, rx) = watch::channel(false);
                stops[i] = Some(tx);
                let ctx = self.ctx_for(graph, &facts, i, &start.consumed, &checkpoint, rx);
                mark_running(&mut state, &graph.steps[i].id, &now_stamp());
                let run = graph.steps[i].run.clone();
                let retry = self.retry.clone();
                let sink = self.sink.clone();
                let child_env = self.child_env.clone();
                let consumed = start.consumed;
                set.spawn(async move {
                    let (attempts, res) =
                        invoke_with_retry(&run, ctx, &retry, &sink, &child_env).await;
                    (i, attempts, res, consumed)
                });
            }
            for i in t.stops {
                if let Some(tx) = stops[i].take() {
                    let _ = tx.send(true);
                }
            }
            state.save(&self.data_root).context("save dag state")?;

            for &(r, outcome) in t.closed.iter().rev() {
                let closed = open.remove(r);
                if let (Some(id), Mailbox::Store { store, .. }) = (&closed.id, &mailbox) {
                    let (outcome, step) = match outcome {
                        Outcome::Done => (RequestOutcome::Done, None),
                        Outcome::Failed { step } => {
                            (RequestOutcome::Failed, Some(graph.steps[step].id.as_str()))
                        }
                    };
                    store.close_request(id, outcome, step).await?;
                }
            }
            if open.is_empty() && set.is_empty() {
                break;
            }
            let polling = matches!(mailbox, Mailbox::Store { .. }) && !cancelled;
            anyhow::ensure!(
                !set.is_empty() || polling,
                "the round is open with nothing running and nothing to start: {:?}",
                t.states
            );

            // Seals before joins: a step sends its seal before its task can
            // finish, so a queued seal predates a queued join.
            tokio::select! {
                biased;
                Some(signal) = checkpoints.recv() => {
                    self.on_signal(graph, signal, &mut facts, &mut state, &mut changed_now,
                        &mut queue, &mut warned_not_streaming).await;
                }
                Some(()) = wait_for_stop(&mut stop_rx), if !cancelled => {
                    // The host is going: stop what runs and take nothing
                    // new. The store's requests stay open, for whoever runs
                    // the loop next.
                    cancelled = true;
                    open.clear();
                }
                _ = tokio::time::sleep(MAILBOX_POLL), if polling => {}
                joined = set.join_next() => {
                    let (i, attempts, res, consumed) = joined
                        .expect("a live task implies a joinable one")
                        .context("step task panicked")?;
                    stops[i] = None;
                    let started = facts.steps[i].running.take().map(|r| r.started).unwrap_or(Seq(0));
                    let e = self.on_ended(graph, i, attempts, res, &consumed, &mut facts,
                        &mut state, &mut changed_now, &mut queue).await;
                    facts.steps[i].last_attempt = Some(Attempt {
                        started,
                        failed: !matches!(e.status, StepStatus::Succeeded { .. }),
                        consumed,
                    });
                    attempts_taken[i] = e.attempts;
                    errors[i] = e.error.clone();
                    ended[i] = Some(e);
                    state.save(&self.data_root).context("save dag state")?;
                }
            }
        }

        for (i, slot) in ended.iter_mut().enumerate() {
            if let Some(e) = slot.take() {
                self.finish(
                    graph,
                    &mut state,
                    &mut status,
                    i,
                    e.status,
                    e.error,
                    e.exit,
                    e.attempts,
                );
            }
        }
        for i in 0..n {
            if status[i].is_some() {
                continue;
            }
            let st = match last_states[i] {
                Row::Waiting(_) if cancelled => StepStatus::Failed {
                    kind: FailureKind::Cancelled,
                },
                Row::Blocked(on) => StepStatus::Blocked {
                    on: graph.steps[on].id.clone(),
                },
                _ => {
                    queue.cleared(graph, i, &*self.sink);
                    StepStatus::SkippedUpToDate
                }
            };
            self.finish(graph, &mut state, &mut status, i, st, None, None, 0);
        }

        if let Some(run) = state.current_run.as_mut() {
            run.finished_at = Some(now_stamp());
        }
        state.save(&self.data_root).context("save dag state")?;

        let steps = graph
            .topo
            .iter()
            .map(|&i| {
                let path = graph.steps[i].output().as_str().to_string();
                let now = facts.sinks[i]
                    .clone()
                    .unwrap_or_else(|| UNKNOWN.to_string());
                let changed = changed_now.get(&path).copied().unwrap_or(false);
                StepReport {
                    id: graph.steps[i].id.clone(),
                    status: status[i].clone().expect("every step has a status"),
                    attempts: attempts_taken[i],
                    error: errors[i].clone(),
                    outputs: vec![(path, now, changed)],
                }
            })
            .collect();
        let report = RunReport { steps };
        self.sink.emit(&Event::RunSummary {
            steps: report.steps.iter().map(step_summary).collect(),
        });
        Ok(report)
    }

    /// Bring `open` and `paused` up to what the mailbox says. A request
    /// the loop has not seen before is opened now, so only an invocation
    /// started after this counts as serving it.
    #[allow(clippy::too_many_arguments)]
    async fn refresh(
        &self,
        graph: &Graph,
        mailbox: &mut Mailbox<'_>,
        open: &mut Vec<Open>,
        paused: &mut BTreeSet<usize>,
        seq: &mut u64,
        ever_in_scope: &mut [bool],
    ) -> Result<()> {
        let mut admit = |id: Option<String>, roots: Vec<usize>, open: &mut Vec<Open>| {
            for (i, reached) in downstream_of(graph, &roots).into_iter().enumerate() {
                ever_in_scope[i] |= reached;
            }
            *seq += 1;
            open.push(Open {
                id,
                request: Request {
                    roots,
                    opened: Seq(*seq),
                },
            });
        };
        match mailbox {
            Mailbox::Fixed { opened } => {
                if !*opened {
                    *opened = true;
                    admit(None, self.roots(graph), open);
                }
            }
            Mailbox::Store { store, seen } => {
                let version = store.data_version().await?;
                if *seen == Some(version) {
                    return Ok(());
                }
                *seen = Some(version);
                let rows = store.open_requests().await?;
                let still_open: BTreeSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
                open.retain(|o| o.id.as_deref().is_none_or(|id| still_open.contains(id)));
                for row in rows {
                    let known = open.iter().position(|o| o.id.as_deref() == Some(&row.id));
                    if row.stop_requested_by.is_some() {
                        if let Some(k) = known {
                            open.remove(k);
                        }
                        store
                            .close_request(&row.id, RequestOutcome::Stopped, None)
                            .await?;
                        continue;
                    }
                    if known.is_some() {
                        continue;
                    }
                    let unknown = row.roots.iter().find(|r| !graph.by_id.contains_key(*r));
                    if let Some(unknown) = unknown {
                        self.sink.emit(&Event::Log {
                            step: unknown.clone(),
                            level: crate::events::LogLevel::Warn,
                            msg: format!(
                                "request {} names a step this config does not have; closing it",
                                row.id
                            ),
                            ts: None,
                            stream: None,
                            target: None,
                            thread: None,
                            fields: None,
                        });
                        store
                            .close_request(&row.id, RequestOutcome::Failed, Some(unknown))
                            .await?;
                        continue;
                    }
                    let roots = row.roots.iter().map(|r| graph.by_id[r]).collect();
                    admit(Some(row.id), roots, open);
                }
                *paused = store
                    .paused()
                    .await?
                    .into_keys()
                    .filter_map(|id| graph.by_id.get(&id).copied())
                    .collect();
            }
        }
        Ok(())
    }

    fn roots(&self, graph: &Graph) -> Vec<usize> {
        (0..graph.steps.len())
            .filter(|&i| graph.steps[i].inputs.is_empty())
            .filter(|&i| {
                self.only_fringe
                    .as_ref()
                    .is_none_or(|only| only.contains(&graph.steps[i].id))
            })
            .collect()
    }

    fn ctx_for(
        &self,
        graph: &Graph,
        facts: &Facts,
        i: usize,
        consumed: &Consumed,
        checkpoint: &crate::step::CheckpointSink,
        stop: watch::Receiver<bool>,
    ) -> StepCtx {
        let spec = &graph.steps[i];
        // "What moved" only means something against a success under the same
        // definition; otherwise the step redoes all of its work.
        let changed_inputs: Vec<ArtifactPath> = match &facts.steps[i].last_success {
            Some(last) if last.fingerprint == consumed.fingerprint => graph
                .deps_in_order(i)
                .filter(|p| last.reads.get(p) != consumed.reads.get(p))
                .map(|p| graph.steps[p].output())
                .collect(),
            _ => vec![],
        };
        StepCtx {
            step_id: spec.id.clone(),
            group: spec.group.clone(),
            group_type: spec.group_type.clone(),
            function: spec.function.clone(),
            data_root: self.data_root.clone(),
            inputs: graph.resolved_inputs[i].clone(),
            changed_inputs,
            reads: paths_of(graph, consumed).into_iter().collect(),
            checkpoint: checkpoint.clone(),
            progress: StepProgress::new(spec.id.clone(), self.sink.clone()),
            stop: StopSignal::new(stop),
        }
    }

    /// The version of what step `i` has published, read from its stores.
    /// `None` for a tree with no store, whose version is what the step
    /// reports. Not qualified with the step's fingerprint, as a reported
    /// version is: a commit names content, and a definition change that
    /// rewrites nothing leaves nothing new to read.
    async fn read_sink(&self, graph: &Graph, i: usize) -> Option<String> {
        let tree = self.data_root.join(graph.steps[i].output().as_str());
        match crate::sink::read_version(&tree).await {
            Ok(v) => v,
            Err(e) => {
                self.sink.emit(&Event::Log {
                    step: graph.steps[i].id.clone(),
                    level: crate::events::LogLevel::Warn,
                    msg: format!(
                        "could not read its stores' heads, using the step's report: {e:#}"
                    ),
                    ts: None,
                    stream: None,
                    target: None,
                    thread: None,
                    fields: None,
                });
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn on_signal(
        &self,
        graph: &Graph,
        signal: crate::step::StepSignal,
        facts: &mut Facts,
        state: &mut DagState,
        changed_now: &mut HashMap<String, bool>,
        queue: &mut QueueLedger,
        warned_not_streaming: &mut [bool],
    ) {
        use crate::step::StepSignal;
        let (step, version, rows) = match signal {
            StepSignal::Capabilities {
                step,
                streams_output,
            } => {
                if let Some(&p) = graph.by_id.get(&step) {
                    facts.steps[p].streams_output = streams_output;
                }
                return;
            }
            StepSignal::Checkpoint {
                step,
                version,
                rows,
            } => (step, version, rows),
        };
        let Some(&p) = graph.by_id.get(&step) else {
            return;
        };
        // A seal read off a step's stdout after its outcome landed would
        // rewind its sink to before that outcome.
        if facts.steps[p].running.is_none() {
            return;
        }
        // The commit `main` is at now, which is at least the one the step
        // announced: it publishes before it says so.
        let qualified = match self.read_sink(graph, p).await {
            Some(read) => read,
            None => format!("{}:{}", graph.fingerprints[p], version),
        };
        let out = graph.steps[p].output().as_str().to_string();
        let moved = facts.sinks[p].as_deref() != Some(qualified.as_str());
        facts.sinks[p] = Some(qualified.clone());
        state
            .steps
            .entry(step.clone())
            .or_default()
            .output_versions
            .insert(out.clone(), qualified.clone());
        changed_now.insert(out, moved);
        queue.sealed(graph, p, &qualified, rows, &*self.sink);
        if moved {
            self.sink.emit(&Event::Checkpoint {
                step: step.clone(),
                version,
                rows,
            });
        }
        if !facts.steps[p].streams_output && !warned_not_streaming[p] {
            warned_not_streaming[p] = true;
            self.sink.emit(&Event::Log {
                step,
                level: crate::events::LogLevel::Warn,
                msg: "checkpointed but does not declare streams_output; no consumer will \
                      start before it finishes"
                    .to_string(),
                ts: None,
                stream: None,
                target: None,
                thread: None,
                fields: None,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn on_ended(
        &self,
        graph: &Graph,
        i: usize,
        attempts: u32,
        res: Result<StepOutcome, StepError>,
        consumed: &Consumed,
        facts: &mut Facts,
        state: &mut DagState,
        changed_now: &mut HashMap<String, bool>,
        queue: &mut QueueLedger,
    ) -> Ended {
        let spec = &graph.steps[i];
        let fingerprint = &graph.fingerprints[i];
        let prior = state
            .steps
            .get(&spec.id)
            .map(|s| s.output_versions.clone())
            .unwrap_or_default();
        let exit = match &res {
            Ok(o) => o.exit,
            Err(e) => e.exit,
        };
        let failed = |kind, error: String| Ended {
            status: StepStatus::Failed { kind },
            error: Some(error),
            exit,
            attempts,
            pass_ended: false,
        };
        // Read after every invocation, whatever it reported and however it
        // ended: a writer's open publishes a commit its crashed predecessor
        // left, so even a step that failed at once can move its sink.
        let read = self.read_sink(graph, i).await;
        match res {
            Ok(outcome) => {
                let resolved = match &read {
                    Some(v) => check_reported(spec, &outcome.outputs)
                        .map(|()| vec![(spec.output().as_str().to_string(), v.clone())]),
                    None => resolve_outputs(
                        &self.data_root,
                        spec,
                        fingerprint,
                        &outcome.outputs,
                        &*self.sink,
                    ),
                };
                let resolved = match resolved {
                    Ok(r) => r,
                    Err(e) => return failed(FailureKind::Data, format!("{e:#}")),
                };
                let mut changed = 0usize;
                for (path, v) in &resolved {
                    let moved = prior.get(path) != Some(v);
                    changed += moved as usize;
                    facts.sinks[i] = Some(v.clone());
                    changed_now.insert(path.clone(), moved);
                    let rows = outcome
                        .outputs
                        .iter()
                        .find(|o| o.path.as_str() == path)
                        .and_then(|o| o.rows);
                    queue.sealed(graph, i, v, rows, &*self.sink);
                }
                let consumed_paths = paths_of(graph, consumed);
                queue.consumed(
                    graph,
                    i,
                    &consumed_paths,
                    |p| facts.steps[p].running.is_none(),
                    &*self.sink,
                );
                let entry = state.steps.entry(spec.id.clone()).or_default();
                entry.input_versions = consumed_paths.into_iter().collect();
                entry.output_versions = resolved.into_iter().collect();
                entry.succeeded = true;
                entry.fingerprint = fingerprint.clone();
                facts.steps[i].last_success = Some(consumed.clone());
                Ended {
                    status: StepStatus::Succeeded { changed },
                    error: None,
                    exit,
                    attempts,
                    pass_ended: false,
                }
            }
            Err(step_err) => {
                // What a failed step committed, its consumers read
                // (plans/supervisor.md §2.5). A store's `main` holds only
                // what was published; without a store, only what the step
                // reported, since an unreported tree may be mid-write.
                if let Some(v) = read {
                    let path = spec.output().as_str().to_string();
                    changed_now.insert(path.clone(), prior.get(&path) != Some(&v));
                    facts.sinks[i] = Some(v.clone());
                    state
                        .steps
                        .entry(spec.id.clone())
                        .or_default()
                        .output_versions
                        .insert(path, v);
                } else if !step_err.outputs.is_empty() {
                    if let Ok(resolved) = resolve_outputs(
                        &self.data_root,
                        spec,
                        fingerprint,
                        &step_err.outputs,
                        &*self.sink,
                    ) {
                        let entry = state.steps.entry(spec.id.clone()).or_default();
                        for (path, v) in resolved {
                            changed_now.insert(path.clone(), prior.get(&path) != Some(&v));
                            facts.sinks[i] = Some(v.clone());
                            entry.output_versions.insert(path, v);
                        }
                    }
                }
                failed(step_err.kind, format!("{:#}", step_err.error))
            }
        }
    }
}

/// A step reports only on the tree it writes; the version itself is read
/// from the store.
fn check_reported(
    spec: &crate::step::StepSpec,
    reported: &[crate::step::ArtifactState],
) -> Result<()> {
    let output = spec.output();
    for r in reported {
        anyhow::ensure!(
            r.path.as_str() == output.as_str(),
            "step {:?} reported on {:?}, but a step writes only the tree its id names ({:?})",
            spec.id,
            r.path.as_str(),
            output.as_str()
        );
    }
    Ok(())
}

/// Resolves when a stop has been asked for; never, with no stop wired.
async fn wait_for_stop(rx: &mut Option<watch::Receiver<bool>>) -> Option<()> {
    let asked = match rx {
        Some(rx) => rx.wait_for(|stop| *stop).await.is_ok(),
        None => false,
    };
    if !asked {
        std::future::pending::<()>().await;
    }
    Some(())
}

/// Today's graph as the tick sees it: each step writes the sink its own
/// index names, and reads the sinks of the steps it names as inputs.
fn shape_of(graph: &Graph) -> Shape {
    let steps = graph
        .steps
        .iter()
        .enumerate()
        .map(|(i, spec)| StepShape {
            writes: i,
            reads: graph.deps_in_order(i).collect(),
            fingerprint: graph.fingerprints[i].clone(),
            pins_reads: spec.reads_pinned,
            class: if spec.inputs.is_empty() {
                Class::Network
            } else if spec.group_type.is_none() {
                Class::Index
            } else {
                Class::Cpu
            },
        })
        .collect();
    Shape {
        steps,
        sink_count: graph.steps.len(),
        topo: graph.topo.clone(),
    }
}

fn facts_of(graph: &Graph, state: &DagState) -> Facts {
    let recorded = |i: usize| state.steps.get(&graph.steps[i].id);
    let sinks: Vec<Option<String>> = (0..graph.steps.len())
        .map(|i| {
            recorded(i)
                .and_then(|s| s.output_versions.get(graph.steps[i].output().as_str()))
                .filter(|v| v.as_str() != UNKNOWN)
                .cloned()
        })
        .collect();
    let steps = (0..graph.steps.len())
        .map(|i| StepFacts {
            last_success: recorded(i).filter(|s| s.succeeded).map(|s| Consumed {
                fingerprint: s.fingerprint.clone(),
                reads: graph
                    .deps_in_order(i)
                    .map(|p| {
                        let v = s
                            .input_versions
                            .get(graph.steps[p].output().as_str())
                            .filter(|v| v.as_str() != UNKNOWN)
                            .cloned();
                        (p, v)
                    })
                    .collect(),
            }),
            last_attempt: None,
            running: None,
            streams_output: graph.steps[i].streams_output,
        })
        .collect();
    Facts { sinks, steps }
}

/// The versions an invocation was started against, keyed the way
/// `dag_state.json` keys them: by the producer's output path.
fn paths_of(graph: &Graph, consumed: &Consumed) -> HashMap<String, String> {
    consumed
        .reads
        .iter()
        .filter_map(|(&p, v)| Some((graph.steps[p].output().as_str().to_string(), v.clone()?)))
        .collect()
}

fn downstream_of(graph: &Graph, roots: &[usize]) -> Vec<bool> {
    let mut seen = vec![false; graph.steps.len()];
    let mut stack = roots.to_vec();
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        stack.extend(graph.dependents[i].iter().copied());
    }
    seen
}

impl Graph {
    /// The producers step `i` reads, in the order its `inputs` name them.
    fn deps_in_order(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        self.resolved_inputs[i]
            .iter()
            .map(|a| self.by_id[a.as_str()])
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::step::{StepRun, StepSpec};

    /// Wait for what the next assertion is about, with a deadline that
    /// names what never came.
    async fn until(what: &str, ready: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A source that counts its runs, and holds each run until `go` is set
    /// — or until it is asked to stop, when it sets `stopped` and reports a
    /// cancel.
    fn source(
        id: &str,
        runs: Arc<AtomicU32>,
        go: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
    ) -> StepSpec {
        StepSpec::new(
            id,
            StepRun::in_process(move |ctx: StepCtx| {
                let (runs, go, stopped) = (runs.clone(), go.clone(), stopped.clone());
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let mut stop = ctx.stop.clone();
                    loop {
                        if go.load(Ordering::SeqCst) {
                            break;
                        }
                        if ctx.stop.is_requested() {
                            stop.requested().await;
                            stopped.store(true, Ordering::SeqCst);
                            return Err(StepError::new(
                                FailureKind::Cancelled,
                                anyhow::anyhow!("stopped"),
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join("f"), "x").unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        )
    }

    struct Fixture {
        root: tempfile::TempDir,
        runs: [Arc<AtomicU32>; 2],
        stopped: [Arc<AtomicBool>; 2],
        go: Arc<AtomicBool>,
        graph: Arc<Graph>,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let runs = [Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0))];
        let stopped = [
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ];
        let go = Arc::new(AtomicBool::new(false));
        let graph = Graph::build(vec![
            source("a/raw", runs[0].clone(), go.clone(), stopped[0].clone()),
            source("b/raw", runs[1].clone(), go.clone(), stopped[1].clone()),
        ])
        .unwrap();
        Fixture {
            root,
            runs,
            stopped,
            go,
            graph: Arc::new(graph),
        }
    }

    fn serve(f: &Fixture) -> tokio::task::JoinHandle<Result<RunReport>> {
        let (root, graph) = (f.root.path().to_path_buf(), f.graph.clone());
        tokio::spawn(async move {
            let store = Store::open(&root).await?;
            let report = Runner::new(&root).serve(&graph, &store).await;
            store.close().await;
            report
        })
    }

    async fn outcome(store: &Store, id: &str) -> Option<Option<RequestOutcome>> {
        store.request(id).await.unwrap().unwrap().closed
    }

    /// The mailbox: a request another process writes while the loop runs is
    /// served by that loop, beside the one it was already running.
    #[tokio::test]
    async fn a_request_written_while_the_loop_runs_is_served_beside_the_first() {
        let f = fixture();
        let other = Store::open(f.root.path()).await.unwrap();
        let first = other.open_request(&["a/raw".into()], "cli").await.unwrap();
        let running = serve(&f);
        until("a to start", || f.runs[0].load(Ordering::SeqCst) == 1).await;

        let second = other.open_request(&["b/raw".into()], "ui").await.unwrap();
        until("b to start beside a", || {
            f.runs[1].load(Ordering::SeqCst) == 1
        })
        .await;
        f.go.store(true, Ordering::SeqCst);

        running.await.unwrap().unwrap();
        for id in [&first, &second] {
            assert_eq!(outcome(&other, id).await, Some(Some(RequestOutcome::Done)));
        }
    }

    #[tokio::test]
    async fn a_stop_written_to_the_store_stops_that_request_and_no_other() {
        let f = fixture();
        let other = Store::open(f.root.path()).await.unwrap();
        let a = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let b = other.open_request(&["b/raw".into()], "cli").await.unwrap();
        let running = serve(&f);
        until("both to start", || {
            f.runs[0].load(Ordering::SeqCst) == 1 && f.runs[1].load(Ordering::SeqCst) == 1
        })
        .await;

        other.request_stop(&a, "claude").await.unwrap();
        until("the stop to reach a's step", || {
            f.stopped[0].load(Ordering::SeqCst)
        })
        .await;
        f.go.store(true, Ordering::SeqCst);

        let report = running.await.unwrap().unwrap();
        assert_eq!(
            outcome(&other, &a).await,
            Some(Some(RequestOutcome::Stopped))
        );
        assert_eq!(outcome(&other, &b).await, Some(Some(RequestOutcome::Done)));
        assert_eq!(
            report.step("a/raw").status,
            StepStatus::Failed {
                kind: FailureKind::Cancelled
            }
        );
        assert!(matches!(
            report.step("b/raw").status,
            StepStatus::Succeeded { .. }
        ));
    }

    #[tokio::test]
    async fn a_paused_step_is_not_started_and_its_request_closes() {
        let f = fixture();
        f.go.store(true, Ordering::SeqCst);
        let other = Store::open(f.root.path()).await.unwrap();
        other.pause("a/raw", "claude").await.unwrap();
        let id = other
            .open_request(&["a/raw".into(), "b/raw".into()], "ui")
            .await
            .unwrap();
        serve(&f).await.unwrap().unwrap();
        assert_eq!(f.runs[0].load(Ordering::SeqCst), 0);
        assert_eq!(f.runs[1].load(Ordering::SeqCst), 1);
        assert_eq!(outcome(&other, &id).await, Some(Some(RequestOutcome::Done)));
    }

    #[tokio::test]
    async fn a_request_naming_a_step_the_config_lacks_is_closed_failed() {
        let f = fixture();
        let other = Store::open(f.root.path()).await.unwrap();
        let id = other
            .open_request(&["gone/raw".into()], "cli")
            .await
            .unwrap();
        serve(&f).await.unwrap().unwrap();
        let row = other.request(&id).await.unwrap().unwrap();
        assert_eq!(row.closed, Some(Some(RequestOutcome::Failed)));
        assert_eq!(row.failed_step.as_deref(), Some("gone/raw"));
    }

    #[tokio::test]
    async fn a_loop_with_no_open_request_ends_at_once() {
        let f = fixture();
        tokio::time::timeout(Duration::from_secs(5), serve(&f))
            .await
            .expect("returns")
            .unwrap()
            .unwrap();
        assert_eq!(f.runs[0].load(Ordering::SeqCst), 0);
    }
}
