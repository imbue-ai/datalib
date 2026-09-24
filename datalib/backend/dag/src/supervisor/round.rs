//! The supervisor's loop: every open request ticked until it closes. The
//! facts come from the record in `system/supervisor.sqlite` and go back to
//! it, and the events are the ones the run store and the server read.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::record::{InvocationEnd, InvocationRow};
use super::store::{RequestOutcome, Store};
use super::tick::{
    tick, Attempt, Class, Consumed, Facts, Intent, Outcome, Request, Running, Seq, Shape,
    StepFacts, StepShape, StepState as Row, Tick, Wait,
};
use crate::artifact::ArtifactPath;
use crate::events::{Event, StepProgress};
use crate::graph::Graph;
use crate::scheduler::{
    invoke_with_retry, mark_running, new_run_id, now_stamp, resolve_outputs, step_summary,
    QueueLedger, RunReport, Runner, StepReport, StepStatus,
};
use crate::step::{Exit, FailureKind, StepCtx, StepError, StepOutcome, StopSignal};
use crate::supervisor::record::{CurrentRun, Record};
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

impl Ended {
    fn as_end(&self) -> InvocationEnd {
        let failure_kind = match &self.status {
            StepStatus::Failed { kind } => Some(<&str>::from(*kind).to_string()),
            _ => None,
        };
        InvocationEnd {
            outcome: self.status.state().as_str().to_string(),
            failure_kind,
            error: self.error.clone(),
            attempts: self.attempts,
            exit_code: self.exit.and_then(|x| x.code),
            signal: self.exit.and_then(|x| x.signal),
        }
    }
}

type Done = (usize, u32, Result<StepOutcome, StepError>, Consumed);

/// The record as the loop holds it, beside the one it last saved: a save
/// writes the difference.
struct Recorded<'a> {
    store: &'a Store,
    saved: Record,
}

impl<'a> Recorded<'a> {
    async fn load(store: &'a Store) -> Result<Recorded<'a>> {
        let saved = store.load_record().await.context("load the record")?;
        Ok(Recorded { store, saved })
    }

    async fn save(&mut self, state: &Record) -> Result<()> {
        self.store
            .save_record(&self.saved, state)
            .await
            .context("save the record")?;
        self.saved = state.clone();
        Ok(())
    }
}

/// Where the loop's requests and pauses come from: the store, read again
/// whenever another process writes to it.
struct Mailbox<'a> {
    store: &'a Store,
    seen: Option<i64>,
    /// Requests naming a step this loop's graph lacks, left for the next
    /// loop: it loads the config again, and may know them. Each with the
    /// roots this graph lacks.
    deferred: BTreeMap<String, Vec<String>>,
}

/// An open request as the loop holds it: its row's id, the tick's view
/// of it, and the steps it wants.
struct Open {
    id: String,
    request: Request,
    scope: Vec<bool>,
}

/// How often a loop with steps running looks for new rows. A loop with
/// nothing running is not waiting on anything else, so it looks at the
/// same pace.
const MAILBOX_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// A request for every source, served until it closes: what a test that
/// is not about requests wants of the loop.
#[cfg(test)]
impl Runner {
    pub(crate) async fn run(&self, graph: &Graph) -> Result<RunReport> {
        let sources: Vec<&str> = graph.fringe_ids();
        self.run_roots(graph, &sources).await
    }

    pub(crate) async fn run_roots(&self, graph: &Graph, roots: &[&str]) -> Result<RunReport> {
        let store = Store::open(&self.data_root).await?;
        let roots: Vec<String> = roots.iter().map(|r| r.to_string()).collect();
        store.open_request(&roots, "test").await?;
        let report = self.serve(graph, &store).await;
        store.close().await;
        report
    }
}

impl Runner {
    /// Every request open in the store, and any opened while this runs,
    /// until none is left. The caller holds `runner-lock`.
    pub async fn serve(&self, graph: &Graph, store: &Store) -> Result<RunReport> {
        let mut mailbox = Mailbox {
            store,
            seen: None,
            deferred: BTreeMap::new(),
        };
        let plan: Vec<String> = graph
            .topo
            .iter()
            .map(|&i| graph.steps[i].id.clone())
            .collect();
        self.sink.emit(&Event::RunPlan { steps: plan });
        let mut record = Recorded::load(store).await?;
        let mut state = record.saved.clone();

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
            states: Default::default(),
        });
        record.save(&state).await?;

        let n = graph.steps.len();
        let shape = shape_of(graph);
        let mut facts = facts_of(graph, &state);
        let mut open: Vec<Open> = Vec::new();
        let mut paused: BTreeMap<usize, String> = BTreeMap::new();
        let mut ever_in_scope = vec![false; n];
        let mut seq = 0u64;

        let mut status: Vec<Option<StepStatus>> = vec![None; n];
        let mut attempts_taken = vec![0u32; n];
        let mut errors: Vec<Option<String>> = vec![None; n];
        let mut changed_now: HashMap<String, bool> = HashMap::new();
        let mut ended: Vec<Option<Ended>> = (0..n).map(|_| None).collect();
        let mut stops: Vec<Option<watch::Sender<bool>>> = (0..n).map(|_| None).collect();
        let mut invocations: Vec<Option<String>> = vec![None; n];
        let mut warned_not_streaming = vec![false; n];
        let mut queue = QueueLedger::new(n);
        // Each step's state the last time a request wanted it, which is
        // what its row settles on once none does.
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
                paused: paused.keys().copied().collect(),
            };
            let t = tick(&shape, &intent, &facts, &self.budgets);
            for (i, st) in t.states.iter().enumerate() {
                if !matches!(st, Row::Idle | Row::Stale) {
                    last_states[i] = *st;
                }
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

            // A step no open request wants any more, and with nothing of
            // its own left to finish, says so now rather than when the
            // loop ends, which may be a long sync of some other source
            // away.
            for i in 0..n {
                let unwanted = matches!(t.states[i], Row::Idle | Row::Stale);
                if ever_in_scope[i] && status[i].is_none() && ended[i].is_none() && unwanted {
                    let Some(st) = settled(graph, &last_states, i, cancelled) else {
                        continue;
                    };
                    if st == StepStatus::SkippedUpToDate {
                        queue.cleared(graph, i, &*self.sink);
                    }
                    self.finish(graph, &mut state, &mut status, i, st, None, None, 0);
                }
            }

            // A request closing now is closed after the save below, and a
            // reader that sees it closed must find no step still serving it.
            let closing: BTreeSet<usize> = t.closed.iter().map(|&(r, _)| r).collect();
            let held: Vec<bool> = ended.iter().map(Option::is_some).collect();
            record_states(graph, &mut state, &t, &held, &paused, |i| {
                let mut serving = open
                    .iter()
                    .enumerate()
                    .filter(|(r, _)| !closing.contains(r));
                serving.find(|(_, o)| o.scope[i]).map(|(_, o)| o.id.clone())
            });
            record_deferred(graph, &mut state, &mailbox.deferred);

            for start in t.starts {
                let i = start.step;
                seq += 1;
                facts.steps[i].running = Some(Running { started: Seq(seq) });
                let (tx, rx) = watch::channel(false);
                stops[i] = Some(tx);
                let ctx = self.ctx_for(graph, &facts, i, &start.consumed, &checkpoint, rx);
                let started_at = now_stamp();
                mark_running(&mut state, &graph.steps[i].id, &started_at);
                let invocation = InvocationRow {
                    id: uuid::Uuid::now_v7().to_string(),
                    step: graph.steps[i].id.clone(),
                    run_id: state
                        .current_run
                        .as_ref()
                        .map(|r| r.run_id.clone())
                        .unwrap_or_default(),
                    started_at_utc: started_at,
                };
                store.open_invocation(&invocation).await?;
                invocations[i] = Some(invocation.id);
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
            for &i in &t.stops {
                if let Some(tx) = stops[i].take() {
                    let _ = tx.send(true);
                }
            }
            record.save(&state).await?;

            for &(r, outcome) in t.closed.iter().rev() {
                let closed = open.remove(r);
                let (outcome, step) = match outcome {
                    Outcome::Done => (RequestOutcome::Done, None),
                    Outcome::Failed { step } => {
                        (RequestOutcome::Failed, Some(graph.steps[step].id.as_str()))
                    }
                };
                store.close_request(&closed.id, outcome, step).await?;
            }
            if open.is_empty() && set.is_empty() {
                break;
            }
            let polling = !cancelled;
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
                    if let Some(id) = invocations[i].take() {
                        store.close_invocation(&id, &e.as_end()).await?;
                    }
                    ended[i] = Some(e);
                    record.save(&state).await?;
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
            if status[i].is_some() || !ever_in_scope[i] {
                continue;
            }
            let Some(st) = settled(graph, &last_states, i, cancelled) else {
                continue;
            };
            if st == StepStatus::SkippedUpToDate {
                queue.cleared(graph, i, &*self.sink);
            }
            self.finish(graph, &mut state, &mut status, i, st, None, None, 0);
        }
        let at_rest = Intent {
            requests: Vec::new(),
            paused: paused.keys().copied().collect(),
        };
        let t = tick(&shape, &at_rest, &facts, &self.budgets);
        record_states(graph, &mut state, &t, &vec![false; n], &paused, |_| None);
        record_deferred(graph, &mut state, &mailbox.deferred);
        if let Some(run) = state.current_run.as_mut() {
            run.finished_at = Some(now_stamp());
        }
        record.save(&state).await?;

        // A step no request reached took no part, and has no line.
        let steps = graph
            .topo
            .iter()
            .filter_map(|&i| {
                let path = graph.steps[i].output().as_str().to_string();
                let now = facts.sinks[i]
                    .clone()
                    .unwrap_or_else(|| UNKNOWN.to_string());
                let changed = changed_now.get(&path).copied().unwrap_or(false);
                Some(StepReport {
                    id: graph.steps[i].id.clone(),
                    status: status[i].clone()?,
                    attempts: attempts_taken[i],
                    error: errors[i].clone(),
                    outputs: vec![(path, now, changed)],
                })
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
        paused: &mut BTreeMap<usize, String>,
        seq: &mut u64,
        ever_in_scope: &mut [bool],
    ) -> Result<()> {
        let mut admit = |id: String, roots: Vec<usize>, open: &mut Vec<Open>| {
            let scope = downstream_of(graph, &roots);
            for (i, &reached) in scope.iter().enumerate() {
                ever_in_scope[i] |= reached;
            }
            *seq += 1;
            open.push(Open {
                id,
                request: Request {
                    roots,
                    opened: Seq(*seq),
                },
                scope,
            });
        };
        let Mailbox {
            store,
            seen,
            deferred,
        } = mailbox;
        {
            {
                let version = store.data_version().await?;
                if *seen == Some(version) {
                    return Ok(());
                }
                // Open before this loop loaded its graph, or so near it
                // that the config it names cannot be newer than the one
                // loaded.
                let first = seen.is_none();
                *seen = Some(version);
                let rows = store.open_requests().await?;
                deferred.retain(|id, _| {
                    rows.iter()
                        .any(|r| &r.id == id && r.stop_requested_by.is_none())
                });
                let still_open: BTreeSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
                open.retain(|o| still_open.contains(o.id.as_str()));
                for row in rows {
                    let known = open.iter().position(|o| o.id == row.id);
                    if row.stop_requested_by.is_some() {
                        store
                            .close_request(&row.id, RequestOutcome::Stopped, None)
                            .await?;
                        if let Some(k) = known {
                            open.remove(k);
                        }
                        continue;
                    }
                    if known.is_some() {
                        continue;
                    }
                    let unknown = row.roots.iter().find(|r| !graph.by_id.contains_key(*r));
                    if let Some(unknown) = unknown {
                        if !first {
                            let lacked = row
                                .roots
                                .iter()
                                .filter(|r| !graph.by_id.contains_key(*r))
                                .cloned()
                                .collect();
                            if deferred.insert(row.id.clone(), lacked).is_none() {
                                self.sink.emit(&Event::Log {
                                    step: unknown.clone(),
                                    level: crate::events::LogLevel::Info,
                                    msg: format!(
                                        "request {} names a step the config this loop loaded \
                                         does not have; leaving it for the next loop",
                                        row.id
                                    ),
                                    ts: None,
                                    stream: None,
                                    target: None,
                                    thread: None,
                                    fields: None,
                                });
                            }
                            continue;
                        }
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
                    admit(row.id, roots, open);
                }
                *paused = paused_in(graph, store).await?;
            }
        }
        Ok(())
    }

    /// One tick with no request open, its states saved: for a host whose
    /// loop is idle when a pause or a resume lands, or that has just taken
    /// the lock from a loop that died with steps running.
    pub async fn settle(&self, graph: &Graph, store: &Store) -> Result<()> {
        let mut record = Recorded::load(store).await?;
        let mut state = record.saved.clone();
        let paused = paused_in(graph, store).await?;
        let intent = Intent {
            requests: Vec::new(),
            paused: paused.keys().copied().collect(),
        };
        let t = tick(
            &shape_of(graph),
            &intent,
            &facts_of(graph, &state),
            &self.budgets,
        );
        let held = vec![false; graph.steps.len()];
        record_states(graph, &mut state, &t, &held, &paused, |_| None);
        record.save(&state).await
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
            stop: StopSignal::new(stop).with_grace(self.stop_grace),
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
        state: &mut Record,
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
        state.steps.entry(step.clone()).or_default().version = Some(qualified.clone());
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
        state: &mut Record,
        changed_now: &mut HashMap<String, bool>,
        queue: &mut QueueLedger,
    ) -> Ended {
        let spec = &graph.steps[i];
        let fingerprint = &graph.fingerprints[i];
        let prior = state.steps.get(&spec.id).and_then(|s| s.version.clone());
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
                    let moved = prior.as_ref() != Some(v);
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
                entry.reads = consumed_paths.into_iter().collect();
                entry.version = resolved.into_iter().next().map(|(_, v)| v);
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
                    changed_now.insert(path, prior.as_ref() != Some(&v));
                    facts.sinks[i] = Some(v.clone());
                    state.steps.entry(spec.id.clone()).or_default().version = Some(v);
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
                            changed_now.insert(path, prior.as_ref() != Some(&v));
                            facts.sinks[i] = Some(v.clone());
                            entry.version = Some(v);
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

/// Step index → who paused it, for the steps this graph has.
async fn paused_in(graph: &Graph, store: &Store) -> Result<BTreeMap<usize, String>> {
    Ok(store
        .paused()
        .await?
        .into_iter()
        .filter_map(|(id, by)| Some((*graph.by_id.get(&id)?, by)))
        .collect())
}

/// Write what the tick made of each step into its record. A step between
/// passes (`held`: its invocation ended, and what it reads has not
/// settled) is still running. `serving` names the open request a step is
/// run for.
fn record_states(
    graph: &Graph,
    state: &mut Record,
    t: &Tick,
    held: &[bool],
    paused: &BTreeMap<usize, String>,
    serving: impl Fn(usize) -> Option<String>,
) {
    let id = |j: usize| graph.steps[j].id.as_str();
    for (i, &st) in t.states.iter().enumerate() {
        let st = if held[i] { Row::Running } else { st };
        let paused_by = paused.get(&i).cloned();
        let detail = match st {
            Row::Running if t.stops.contains(&i) => Some(match &paused_by {
                Some(by) => format!("stopping: paused by {by}"),
                None => "stopping: no open request wants it".to_string(),
            }),
            Row::Paused => paused_by.as_ref().map(|by| format!("paused by {by}")),
            Row::Blocked(p) => Some(format!(
                "{} has published nothing and is not going to run",
                id(p)
            )),
            Row::Waiting(Wait::Upstream(p)) => Some(format!("waiting for {}", id(p))),
            // Today each step writes the sink its own index names.
            Row::Waiting(Wait::Sink(s)) => Some(format!("waiting for another writer of {}", id(s))),
            Row::Waiting(Wait::Reader(r)) => Some(format!(
                "waiting for {}, which reads what this writes",
                id(r)
            )),
            Row::Waiting(Wait::Budget(class)) => {
                Some(format!("waiting for a free {} slot", class.as_str()))
            }
            _ => None,
        };
        let entry = state.steps.entry(id(i).to_string()).or_default();
        entry.state = Some(st.into());
        entry.state_detail = detail;
        entry.paused_by = paused_by;
        entry.request = serving(i);
    }
}

/// A step this loop's graph lacks reads as waiting while a request left
/// for the next loop names it, and as nothing once none does.
fn record_deferred(graph: &Graph, state: &mut Record, deferred: &BTreeMap<String, Vec<String>>) {
    for root in deferred.values().flatten() {
        state.steps.entry(root.clone()).or_default();
    }
    for (id, st) in state.steps.iter_mut() {
        if graph.by_id.contains_key(id) {
            continue;
        }
        let request = deferred
            .iter()
            .find(|(_, roots)| roots.contains(id))
            .map(|(r, _)| r.clone());
        st.state = request.as_ref().map(|_| super::tick::StateKind::Waiting);
        st.state_detail = request.as_ref().map(|_| {
            "waiting for the sync in progress to end: it loaded the config before this \
             step was in it"
                .to_string()
        });
        st.request = request;
    }
}

/// What a step's row settles on once no open request wants it and it has
/// nothing of its own left to finish. A paused step took no part: none.
fn settled(graph: &Graph, last_states: &[Row], i: usize, cancelled: bool) -> Option<StepStatus> {
    Some(match last_states[i] {
        Row::Paused => return None,
        Row::Waiting(_) if cancelled => StepStatus::Failed {
            kind: FailureKind::Cancelled,
        },
        Row::Blocked(on) => StepStatus::Blocked {
            on: graph.steps[on].id.clone(),
        },
        _ => StepStatus::SkippedUpToDate,
    })
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

fn facts_of(graph: &Graph, state: &Record) -> Facts {
    let recorded = |i: usize| state.steps.get(&graph.steps[i].id);
    let sinks: Vec<Option<String>> = (0..graph.steps.len())
        .map(|i| {
            recorded(i)
                .and_then(|s| s.version.clone())
                .filter(|v| v.as_str() != UNKNOWN)
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
                            .reads
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

/// The versions an invocation was started against, keyed the way the
/// record keys them: by the producer's output path.
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
    use crate::supervisor::record::StepRecord;
    use crate::supervisor::tick::StateKind;

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
        let a = crate::supervisor::record::recorded(f.root.path())
            .await
            .steps["a/raw"]
            .clone();
        assert_eq!(a.state, Some(StateKind::Paused));
        assert_eq!(a.paused_by.as_deref(), Some("claude"));
        assert_eq!(a.last_run, None, "a step it skipped took no part");
        assert_eq!(a.last_success_at, None);
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

    /// A step's record as the loop last saved it, once `ready` holds.
    async fn until_recorded(
        root: &std::path::Path,
        step: &str,
        what: &str,
        ready: impl Fn(&StepRecord) -> bool,
    ) -> StepRecord {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let record = crate::supervisor::record::recorded(root).await;
            if let Some(st) = record.steps.get(step).filter(|st| ready(st)) {
                return st.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {step} to be {what}: {:?}",
                record.steps.get(step)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A Manage row is the record's `state`, with no inference behind it:
    /// the step a request wants reads running and names that request, and
    /// one no request wants names none.
    #[tokio::test]
    async fn each_step_reads_as_the_tick_left_it_and_names_the_request_it_serves() {
        let f = fixture();
        let other = Store::open(f.root.path()).await.unwrap();
        let id = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let running = serve(&f);
        let a = until_recorded(f.root.path(), "a/raw", "running", |st| {
            st.state == Some(StateKind::Running)
        })
        .await;
        assert_eq!(a.request.as_deref(), Some(id.as_str()));
        let b = crate::supervisor::record::recorded(f.root.path())
            .await
            .steps["b/raw"]
            .clone();
        assert_eq!(b.state, Some(StateKind::Stale), "never succeeded");
        assert_eq!(b.request, None);

        f.go.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
        let a = crate::supervisor::record::recorded(f.root.path())
            .await
            .steps["a/raw"]
            .clone();
        assert_eq!((a.state, a.request), (Some(StateKind::Idle), None));
    }

    /// A stop closes the request at once, but the step it stopped is still
    /// checkpointing: until it exits its row reads running, serving no
    /// request, and says it is stopping — what the Stopping button is.
    #[tokio::test]
    async fn a_stopped_step_reads_stopping_until_it_has_exited() {
        let root = tempfile::tempdir().unwrap();
        let heard_stop = Arc::new(AtomicBool::new(false));
        let let_go = Arc::new(AtomicBool::new(false));
        let (heard, go) = (heard_stop.clone(), let_go.clone());
        let lingering = StepSpec::new(
            "a/raw",
            StepRun::in_process(move |ctx: StepCtx| {
                let (heard, go) = (heard.clone(), go.clone());
                async move {
                    let mut stop = ctx.stop.clone();
                    stop.requested().await;
                    heard.store(true, Ordering::SeqCst);
                    while !go.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(StepError::new(
                        FailureKind::Cancelled,
                        anyhow::anyhow!("stopped"),
                    ))
                }
            }),
        );
        let graph = Arc::new(Graph::build(vec![lingering]).unwrap());
        let other = Store::open(root.path()).await.unwrap();
        let id = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let running = {
            let (root, graph) = (root.path().to_path_buf(), graph.clone());
            tokio::spawn(async move {
                let store = Store::open(&root).await?;
                let report = Runner::new(&root).serve(&graph, &store).await;
                store.close().await;
                report
            })
        };

        until_recorded(root.path(), "a/raw", "running", |st| {
            st.state == Some(StateKind::Running)
        })
        .await;
        other.request_stop(&id, "ui").await.unwrap();
        until("the stop to reach the step", || {
            heard_stop.load(Ordering::SeqCst)
        })
        .await;
        let a = until_recorded(root.path(), "a/raw", "serving no request", |st| {
            st.request.is_none()
        })
        .await;
        assert_eq!(a.state, Some(StateKind::Running), "still running");
        assert_eq!(
            a.state_detail.as_deref(),
            Some("stopping: no open request wants it")
        );
        assert_eq!(
            outcome(&other, &id).await,
            Some(Some(RequestOutcome::Stopped))
        );

        let_go.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
        let a = crate::supervisor::record::recorded(root.path()).await.steps["a/raw"].clone();
        assert_ne!(a.state, Some(StateKind::Running));
        assert_eq!(a.last_run.unwrap().status, "stopped");
    }

    /// A pause landing while no loop runs reaches the rows only through a
    /// tick; `settle` is that tick, with no request open.
    #[tokio::test]
    async fn a_pause_while_the_loop_is_idle_reads_once_settled() {
        let f = fixture();
        let store = Store::open(f.root.path()).await.unwrap();
        let runner = Runner::new(f.root.path());
        store.pause("a/raw", "claude").await.unwrap();
        runner.settle(&f.graph, &store).await.unwrap();
        let a = crate::supervisor::record::recorded(f.root.path())
            .await
            .steps["a/raw"]
            .clone();
        assert_eq!(a.state, Some(StateKind::Paused));
        assert_eq!(a.paused_by.as_deref(), Some("claude"));
        assert_eq!(a.state_detail.as_deref(), Some("paused by claude"));

        store.resume("a/raw").await.unwrap();
        runner.settle(&f.graph, &store).await.unwrap();
        let a = crate::supervisor::record::recorded(f.root.path())
            .await
            .steps["a/raw"]
            .clone();
        assert_eq!(
            (a.state, a.paused_by, a.state_detail),
            (Some(StateKind::Stale), None, None)
        );
    }

    /// A source added to the config while a loop runs is not in the graph
    /// that loop loaded. Its request waits for the next loop, which loads
    /// the config again, rather than failing as a step that does not exist.
    #[tokio::test]
    async fn a_request_the_loop_cannot_place_mid_loop_is_left_for_the_next() {
        let f = fixture();
        let other = Store::open(f.root.path()).await.unwrap();
        let first = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let running = serve(&f);
        until("a to start", || f.runs[0].load(Ordering::SeqCst) == 1).await;

        let later = other.open_request(&["c/raw".into()], "ui").await.unwrap();
        // Written after the new request, so seeing it taken on means the
        // loop has read past the new one too.
        let marker = other.open_request(&["b/raw".into()], "ui").await.unwrap();
        until("the loop to read past the new request", || {
            f.runs[1].load(Ordering::SeqCst) == 1
        })
        .await;
        // Its row says it waits, and for whom, rather than nothing.
        let c = until_recorded(f.root.path(), "c/raw", "waiting", |st| {
            st.state == Some(StateKind::Waiting)
        })
        .await;
        assert_eq!(c.request.as_deref(), Some(later.as_str()));
        f.go.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();

        assert_eq!(outcome(&other, &later).await, None, "still open");
        for id in [&first, &marker] {
            assert_eq!(outcome(&other, id).await, Some(Some(RequestOutcome::Done)));
        }
    }

    /// A step a stopped request wanted, and nothing else does, reads as
    /// settled while the loop goes on with another source's sync. It used
    /// to keep its last record until the whole loop ended.
    #[tokio::test]
    async fn a_step_no_request_wants_any_more_settles_while_the_loop_goes_on() {
        let f = fixture();
        let render = StepSpec::new(
            "a/rendered",
            StepRun::in_process(|_| async { Ok(StepOutcome::default()) }),
        )
        .input("a/raw");
        let graph = Arc::new(
            Graph::build(vec![
                source(
                    "a/raw",
                    f.runs[0].clone(),
                    f.go.clone(),
                    f.stopped[0].clone(),
                ),
                source(
                    "b/raw",
                    f.runs[1].clone(),
                    f.go.clone(),
                    f.stopped[1].clone(),
                ),
                render,
            ])
            .unwrap(),
        );
        let f = Fixture { graph, ..f };
        let other = Store::open(f.root.path()).await.unwrap();
        let a = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        other.open_request(&["b/raw".into()], "ui").await.unwrap();
        let running = serve(&f);
        until("both to start", || {
            f.runs[0].load(Ordering::SeqCst) == 1 && f.runs[1].load(Ordering::SeqCst) == 1
        })
        .await;

        other.request_stop(&a, "ui").await.unwrap();
        let root = f.root.path().to_path_buf();
        let state_of = |step: &'static str| {
            let root = root.clone();
            async move {
                crate::supervisor::record::recorded(&root)
                    .await
                    .current_run
                    .and_then(|r| r.states.get(step).cloned())
            }
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while state_of("a/rendered").await.as_deref() != Some("skipped_up_to_date") {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for a's render to settle while b still runs"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(state_of("b/raw").await.as_deref(), Some("running"));

        f.go.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
    }

    /// Every process the loop starts has a row that says it is running
    /// until the loop has seen it end, and then how: what `status` and the
    /// next loop's take-over read.
    #[tokio::test]
    async fn a_step_the_loop_starts_is_an_invocation_until_it_ends() {
        let f = fixture();
        let other = Store::open(f.root.path()).await.unwrap();
        other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let running = serve(&f);
        until("a to start", || f.runs[0].load(Ordering::SeqCst) == 1).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let open = loop {
            let open = other.running_invocations().await.unwrap();
            if !open.is_empty() {
                break open;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a's invocation never appeared"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].step, "a/raw");

        f.go.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
        assert!(other.running_invocations().await.unwrap().is_empty());
        let outcome: String = sqlx::query_scalar("SELECT outcome FROM invocations WHERE id = ?")
            .bind(&open[0].id)
            .fetch_one(other.pool())
            .await
            .unwrap();
        assert_eq!(outcome, "succeeded");
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
