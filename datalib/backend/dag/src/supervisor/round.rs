//! The supervisor's loop: every open request ticked until it closes. The
//! facts come from the record in `system/supervisor.sqlite` and go back to
//! it, and the events are the ones the run store and the server read.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::announce::{Listener, CONFIG_CHANGED};
use super::record::{InvocationEnd, InvocationRow};
use super::store::{RequestOutcome, Store, WipeKind, WipeRow};
use super::tick::{
    tick, Attempt, Consumed, Facts, Intent, LockShape, Outcome, Request, Running, Seq, Shape,
    StepFacts, StepShape, StepState as Row, Tick, Wait,
};
use super::wipe;
use crate::artifact::ArtifactPath;
use crate::events::{Event, PlannedStep, StepProgress};
use crate::graph::Graph;
use crate::scheduler::{
    fresh_version, invoke_with_retry, mark_running, new_run_id, now_stamp, reported_version,
    step_summary, QueueLedger, RunReport, Runner, StepReport, StepStatus,
};
use crate::step::{Exit, FailureKind, StepCtx, StepError, StepId, StepOutcome, StopSignal};
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

/// What the loop holds for one step through a round. A config swap
/// carries it to the step's place in the new graph.
#[derive(Default)]
struct Slot {
    /// Whether an open request has wanted it this round.
    ever_in_scope: bool,
    last_wanted: LastWanted,
    live: Option<Live>,
    ended: Option<Ended>,
    warned_not_streaming: bool,
    /// What the round's report says of it.
    status: Option<StepStatus>,
    attempts: u32,
    error: Option<String>,
}

impl Slot {
    fn remapped(mut self, to_new: &HashMap<usize, usize>) -> Slot {
        self.live = self.live.map(|l| Live {
            consumed: remap_consumed(l.consumed, to_new),
            ..l
        });
        self
    }
}

/// A step's state the last time a request wanted it, as far as what its
/// row settles on once none does. A step it was blocked on is named, so
/// no index goes stale when the config is swapped.
#[derive(Default)]
enum LastWanted {
    #[default]
    Unwanted,
    Off,
    Waiting,
    Blocked(String),
    Other,
}

impl LastWanted {
    fn of(graph: &Graph, state: Row) -> LastWanted {
        match state {
            Row::Idle | Row::Stale => LastWanted::Unwanted,
            Row::Off => LastWanted::Off,
            Row::Waiting(_) => LastWanted::Waiting,
            Row::Blocked(on) => LastWanted::Blocked(graph.steps[on].id.clone()),
            _ => LastWanted::Other,
        }
    }

    /// A step turned off took no part: none.
    fn settles(&self, cancelled: bool) -> Option<StepStatus> {
        Some(match self {
            LastWanted::Off => return None,
            LastWanted::Waiting if cancelled => StepStatus::Failed {
                kind: FailureKind::Cancelled,
            },
            LastWanted::Blocked(on) => StepStatus::Blocked { on: on.clone() },
            _ => StepStatus::SkippedUpToDate,
        })
    }
}

/// An invocation in flight.
struct Live {
    invocation: String,
    consumed: Consumed,
    /// Taken when the loop tells it to stop: what ends then was asked to.
    stop: Option<watch::Sender<bool>>,
}

type Done = (String, u32, Result<StepOutcome, StepError>);

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

/// Where the loop's requests and switches come from: the store, read again
/// whenever another process writes to it.
struct Mailbox<'a> {
    store: &'a Store,
    seen: Option<i64>,
    /// Whether the store has been read once: a request open before that
    /// was open before the loop loaded its graph.
    started: bool,
    /// Requests naming a step this loop's graph lacks, set aside until a
    /// config that has it is taken on, by this loop or the next. Each with
    /// the roots this graph lacks.
    deferred: BTreeMap<String, Vec<String>>,
    /// Requests asked to stop, closed only once a save has let go of them.
    stopped: Vec<String>,
}

/// An open request as the loop holds it: its row's id and the tick's
/// view of it.
struct Open {
    id: String,
    request: Request,
}

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
            started: false,
            deferred: BTreeMap::new(),
            stopped: Vec::new(),
        };
        let plan: Vec<PlannedStep> = graph
            .topo
            .iter()
            .map(|&i| PlannedStep::of(&graph.steps[i]))
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

        // Made before the first look at the mailbox, so a row written
        // after that look is heard.
        let mut listener = Listener::new(store, "the loop").backstop(self.backstop);
        let mut current = graph.clone();
        // The host loaded the config before this period began, and it may
        // have moved since; after that, only when it is said to have.
        let mut config_moved = true;
        let mut next_graph: Option<Graph> = None;
        let mut said_waiting_to_swap = false;
        let mut shape = shape_of(graph, &self.lock_slots);
        let mut facts = facts_of(graph, &state);
        let mut open: Vec<Open> = Vec::new();
        let mut turned_off: BTreeMap<usize, String> = BTreeMap::new();
        let mut wipes: Vec<WipeRow> = Vec::new();
        let mut seq = 0u64;
        let mut slots: Vec<Slot> = graph.steps.iter().map(|_| Slot::default()).collect();
        // A step may seal and end several passes in one round; the summary
        // says whether the round as a whole moved its output.
        let at_start: HashMap<String, Option<String>> = state
            .steps
            .iter()
            .map(|(id, s)| (id.clone(), s.version.clone()))
            .collect();
        let mut queue = QueueLedger::new(graph.steps.len());
        let mut cancelled = false;
        let mut stop_rx = self.stop.clone();

        self.refresh(
            graph,
            &mut mailbox,
            &mut open,
            &mut turned_off,
            &mut wipes,
            &mut seq,
            &mut slots,
        )
        .await?;

        let (cp_tx, mut checkpoints) = tokio::sync::mpsc::unbounded_channel();
        let checkpoint = crate::step::CheckpointSink::new(cp_tx);
        let mut set: JoinSet<Done> = JoinSet::new();

        loop {
            if !cancelled && std::mem::take(&mut config_moved) {
                // One that does not load is waited out on the graph the
                // loop has; the Manage screen already says it is broken.
                if let Some(Ok(next)) = self.reload.as_ref().map(|s| s.load()) {
                    next_graph = Some(next);
                }
            }
            if let Some(next) = next_graph.take().filter(|_| !cancelled) {
                let roots: BTreeSet<usize> = open
                    .iter()
                    .flat_map(|o| o.request.roots.iter().copied())
                    .collect();
                let still_needed: Vec<&str> = current
                    .steps
                    .iter()
                    .enumerate()
                    .filter(|&(i, s)| {
                        (facts.steps[i].running.is_some() || roots.contains(&i))
                            && !next.by_id.contains_key(&s.id)
                    })
                    .map(|(_, s)| s.id.as_str())
                    .collect();
                if !still_needed.is_empty() {
                    // Not interrupted: a step dropped by a config saved
                    // mid-edit would lose hours of download to a typo.
                    if !std::mem::replace(&mut said_waiting_to_swap, true) {
                        for id in &still_needed {
                            self.note(
                                id,
                                "the config no longer has this step; this sync takes the \
                                 config on once it is done with it"
                                    .into(),
                            );
                        }
                    }
                    next_graph = Some(next);
                } else if !same_graph(&current, &next) {
                    said_waiting_to_swap = false;
                    let old = std::mem::replace(&mut current, next);
                    let graph = &current;
                    let from_old: Vec<Option<usize>> = graph
                        .steps
                        .iter()
                        .map(|s| old.by_id.get(&s.id).copied())
                        .collect();
                    let to_new: HashMap<usize, usize> = from_old
                        .iter()
                        .enumerate()
                        .filter_map(|(new, o)| Some(((*o)?, new)))
                        .collect();
                    // A step the new graph lacks, done but for what it
                    // reads settling, is done now, under the graph it ran in.
                    for (i, s) in slots.iter_mut().enumerate() {
                        if to_new.contains_key(&i) {
                            continue;
                        }
                        if let Some(e) = s.ended.take() {
                            self.finish(
                                &old,
                                &mut state,
                                &mut s.status,
                                i,
                                e.status,
                                e.error,
                                e.exit,
                                e.attempts,
                            );
                        }
                    }
                    shape = shape_of(graph, &self.lock_slots);
                    slots = carry(&mut slots, &from_old)
                        .into_iter()
                        .map(|s| s.remapped(&to_new))
                        .collect();
                    queue.remap(&from_old);
                    // The record holds everything the facts are made of
                    // but what is in flight, which carries over.
                    let before = std::mem::replace(&mut facts, facts_of(graph, &state));
                    for (f, o) in facts.steps.iter_mut().zip(&from_old) {
                        let Some(was) = o.map(|o| &before.steps[o]) else {
                            continue;
                        };
                        f.running = was.running.clone();
                        f.streams_output = was.streams_output;
                        f.last_attempt = was.last_attempt.clone().map(|a| Attempt {
                            consumed: remap_consumed(a.consumed, &to_new),
                            ..a
                        });
                    }
                    // Every root is in the new graph: a swap waits until it is.
                    for o in open.iter_mut() {
                        for r in o.request.roots.iter_mut() {
                            *r = to_new[r];
                        }
                        let reached = downstream_of(graph, &o.request.roots);
                        for (slot, s) in slots.iter_mut().zip(reached) {
                            slot.ever_in_scope |= s;
                        }
                    }
                    turned_off = turned_off_in(graph, store).await?;
                    // Read the requests again: one left for want of a step
                    // this graph has is taken on now.
                    mailbox.seen = None;
                    let mut added = Vec::new();
                    for (i, o) in from_old.iter().enumerate() {
                        let id = &graph.steps[i].id;
                        match o {
                            None => {
                                added.push(PlannedStep::of(&graph.steps[i]));
                                self.note(id, "added to the config during this sync".into());
                            }
                            Some(o) if old.fingerprints[*o] != graph.fingerprints[i] => {
                                self.note(id, "changed in the config during this sync".into())
                            }
                            Some(_) => {}
                        }
                    }
                    if !added.is_empty() {
                        self.sink.emit(&Event::RunPlan { steps: added });
                    }
                }
            }
            let graph = &current;
            if !cancelled {
                self.refresh(
                    graph,
                    &mut mailbox,
                    &mut open,
                    &mut turned_off,
                    &mut wipes,
                    &mut seq,
                    &mut slots,
                )
                .await?;
                let opened = self
                    .carry_out_wipes(
                        graph,
                        next_graph.as_ref(),
                        store,
                        &mut record,
                        &mut state,
                        &mut wipes,
                        &mut facts,
                        &mut slots,
                    )
                    .await?;
                if opened {
                    // The loop's own write does not move what `refresh`
                    // watches.
                    mailbox.seen = None;
                    self.refresh(
                        graph,
                        &mut mailbox,
                        &mut open,
                        &mut turned_off,
                        &mut wipes,
                        &mut seq,
                        &mut slots,
                    )
                    .await?;
                }
            }
            let wiping = wiping(graph, &wipes);
            // A step being wiped leaves the requests that named it: a reset
            // does not refill a download, and the config that drops a
            // purged group is not waited on. A request left with nothing is
            // stopped.
            open.retain_mut(|o| {
                o.request.roots.retain(|r| !wiping.contains_key(r));
                if o.request.roots.is_empty() {
                    mailbox.stopped.push(o.id.clone());
                }
                !o.request.roots.is_empty()
            });
            let intent = Intent {
                requests: open.iter().map(|o| o.request.clone()).collect(),
                turned_off: turned_off.keys().chain(wiping.keys()).copied().collect(),
            };
            let t = tick(&shape, &intent, &facts);
            for scope in &t.scopes {
                for (slot, &wanted) in slots.iter_mut().zip(scope) {
                    slot.ever_in_scope |= wanted;
                }
            }
            for (slot, &st) in slots.iter_mut().zip(&t.states) {
                match LastWanted::of(graph, st) {
                    LastWanted::Unwanted => {}
                    wanted => slot.last_wanted = wanted,
                }
            }
            let starting: BTreeSet<usize> = t.starts.iter().map(|s| s.step).collect();

            // A step between passes has not settled either, so the index
            // behind a render that is waiting on its download's next seal
            // keeps reading Running too, not just the render.
            let mut unsettled = vec![false; slots.len()];
            for &i in &graph.topo {
                let upstream = graph.deps_in_order(i).any(|p| unsettled[p]);
                unsettled[i] = matches!(t.states[i], Row::Running | Row::Waiting(_))
                    || (slots[i].ended.is_some() && upstream);
            }

            for (i, s) in slots.iter_mut().enumerate() {
                let Some(e) = s.ended.as_mut() else { continue };
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
                    s.ended = None;
                } else if !upstream_busy {
                    let e = s.ended.take().expect("checked above");
                    self.finish(
                        graph,
                        &mut state,
                        &mut s.status,
                        i,
                        e.status,
                        e.error,
                        e.exit,
                        e.attempts,
                    );
                }
            }

            // A step with nothing of its own left to finish says so now
            // rather than when the loop ends, which may be a long sync of
            // some other source away: one no open request wants any more,
            // and one up to date with nothing above it that could still
            // change what it reads.
            let mut above_unsettled = vec![false; slots.len()];
            for &i in &graph.topo {
                above_unsettled[i] = graph
                    .deps_in_order(i)
                    .any(|p| unsettled[p] || above_unsettled[p]);
            }
            for (i, s) in slots.iter_mut().enumerate() {
                let done = match t.states[i] {
                    Row::Idle | Row::Stale => true,
                    Row::Fresh => !above_unsettled[i],
                    _ => false,
                };
                if s.ended.is_none() && done {
                    self.settle_row(graph, &mut state, &mut queue, s, i, cancelled);
                }
            }

            // A request closing now, or stopped, is closed after the save
            // below: a reader that sees it closed finds no step serving it.
            let closing: BTreeSet<usize> = t.closed.iter().map(|&(r, _)| r).collect();
            // A step between passes waits on the first producer it reads
            // that has not settled.
            let held: Vec<Option<usize>> = slots
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    s.ended
                        .as_ref()
                        .and_then(|_| graph.deps_in_order(i).find(|&p| unsettled[p]))
                })
                .collect();
            // A step serves a request only while it has work left in it,
            // so a source whose part is done offers Sync again while the
            // index its request also reaches is still running.
            let working: Vec<bool> = (0..slots.len())
                .map(|i| {
                    held[i].is_some()
                        || match t.states[i] {
                            Row::Running | Row::Waiting(_) => true,
                            Row::Fresh => above_unsettled[i],
                            _ => false,
                        }
                })
                .collect();
            record_states(graph, &shape, &mut state, &t, &held, &turned_off, |i| {
                if !working[i] {
                    return Vec::new();
                }
                open.iter()
                    .enumerate()
                    .filter(|(r, _)| !closing.contains(r) && t.scopes[*r][i])
                    .map(|(_, o)| o.id.clone())
                    .collect()
            });
            say_wiping(graph, &mut state, &t, &wiping);
            record_deferred(graph, &mut state, &mailbox.deferred);

            for start in t.starts {
                let i = start.step;
                seq += 1;
                facts.steps[i].running = Some(Running { started: Seq(seq) });
                let (stop, rx) = watch::channel(false);
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
                slots[i].live = Some(Live {
                    invocation: invocation.id,
                    consumed: start.consumed,
                    stop: Some(stop),
                });
                let run = graph.steps[i].run.clone();
                let retry = self.retry.clone();
                let sink = self.sink.clone();
                let child_env = self.child_env.clone();
                let id = graph.steps[i].id.clone();
                set.spawn(async move {
                    let (attempts, res) =
                        invoke_with_retry(&run, ctx, &retry, &sink, &child_env).await;
                    (id, attempts, res)
                });
            }
            for &i in &t.stops {
                if let Some(stop) = slots[i].live.as_mut().and_then(|l| l.stop.take()) {
                    let _ = stop.send(true);
                }
            }
            record.save(&state).await?;

            for id in std::mem::take(&mut mailbox.stopped) {
                store
                    .close_request(&id, RequestOutcome::Stopped, None)
                    .await?;
            }
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
                // A config waiting on what just ended may bring requests
                // of its own, which this loop takes on rather than the next;
                // and a purge waits for the config that lets its groups go.
                if next_graph.is_some() {
                    continue;
                }
                if wipes.is_empty() {
                    break;
                }
            }
            let listening = !cancelled;
            anyhow::ensure!(
                !set.is_empty() || listening,
                "the round is open with nothing running and nothing to start: {:?}",
                t.states
            );

            // Not `biased`: a producer re-announces its seal until its
            // consumer has run, so a loop slower than that cadence always
            // has a seal waiting, and a seal-first select never reaches the
            // join that would run the consumer.
            tokio::select! {
                Some(signal) = checkpoints.recv() => {
                    // Every one queued in one turn, so repeats cost one tick.
                    for signal in std::iter::once(signal).chain(queued(&mut checkpoints)) {
                        self.on_signal(graph, signal, &mut facts, &mut state, &mut queue,
                            &mut slots).await;
                    }
                }
                Some(()) = wait_for_stop(&mut stop_rx), if !cancelled => {
                    // The host is going: stop what runs and take nothing
                    // new. The store's requests and wipes stay open, for
                    // whoever runs the loop next.
                    cancelled = true;
                    open.clear();
                    wipes.clear();
                }
                heard = listener.next(), if listening => {
                    config_moved |= heard.iter().any(|line| line == CONFIG_CHANGED);
                }
                joined = set.join_next() => {
                    // Seals before joins: a step sends its seal before its
                    // task can finish, so every seal this join follows is
                    // queued by now.
                    for signal in queued(&mut checkpoints) {
                        self.on_signal(graph, signal, &mut facts, &mut state, &mut queue,
                            &mut slots).await;
                    }
                    let (id, attempts, res) = joined
                        .expect("a live task implies a joinable one")
                        .context("step task panicked")?;
                    // A graph that drops a running step waits for it.
                    let i = graph.by_id[&id];
                    let live = slots[i].live.take().expect("a joined step was started");
                    let started = facts.steps[i].running.take().map(|r| r.started).unwrap_or(Seq(0));
                    let e = self.on_ended(graph, i, attempts, res, &live, &mut facts,
                        &mut state, &mut queue).await;
                    facts.steps[i].last_attempt = Some(Attempt {
                        started,
                        failed: !matches!(e.status, StepStatus::Succeeded { .. }),
                        // Asked to stop, and it did: one that says it
                        // failed has failed, whatever it was asked.
                        stopped: live.stop.is_none()
                            && e.status.state() == crate::run_state::RunState::Stopped,
                        consumed: live.consumed,
                    });
                    store.close_invocation(&live.invocation, &e.as_end()).await?;
                    slots[i].attempts = e.attempts;
                    slots[i].error = e.error.clone();
                    slots[i].ended = Some(e);
                    record.save(&state).await?;
                }
            }
        }

        let graph = &current;
        for (i, s) in slots.iter_mut().enumerate() {
            if let Some(e) = s.ended.take() {
                self.finish(
                    graph,
                    &mut state,
                    &mut s.status,
                    i,
                    e.status,
                    e.error,
                    e.exit,
                    e.attempts,
                );
            }
            self.settle_row(graph, &mut state, &mut queue, s, i, cancelled);
        }
        let at_rest = Intent {
            requests: Vec::new(),
            turned_off: turned_off.keys().copied().collect(),
        };
        let t = tick(&shape, &at_rest, &facts);
        record_states(
            graph,
            &shape,
            &mut state,
            &t,
            &vec![None; slots.len()],
            &turned_off,
            |_| Vec::new(),
        );
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
                let before = at_start.get(&graph.steps[i].id).cloned().flatten();
                let changed = facts.sinks[i].is_some() && facts.sinks[i] != before;
                Some(StepReport {
                    id: graph.steps[i].id.clone(),
                    status: slots[i].status.clone()?,
                    attempts: slots[i].attempts,
                    error: slots[i].error.clone(),
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

    /// Bring `open`, `turned_off` and `wipes` up to what the mailbox says.
    /// A request the loop has not seen before is opened now, so only an
    /// invocation started after this counts as serving it.
    #[allow(clippy::too_many_arguments)]
    async fn refresh(
        &self,
        graph: &Graph,
        mailbox: &mut Mailbox<'_>,
        open: &mut Vec<Open>,
        turned_off: &mut BTreeMap<usize, String>,
        wipes: &mut Vec<WipeRow>,
        seq: &mut u64,
        slots: &mut [Slot],
    ) -> Result<()> {
        let mut admit = |id: String, roots: Vec<usize>, open: &mut Vec<Open>| {
            for (slot, reached) in slots.iter_mut().zip(downstream_of(graph, &roots)) {
                slot.ever_in_scope |= reached;
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
        let Mailbox {
            store,
            seen,
            started,
            deferred,
            stopped,
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
                let first = !std::mem::replace(started, true);
                *seen = Some(version);
                let (rows, all_turned_off) = store.mailbox().await?;
                deferred.retain(|id, _| {
                    rows.iter()
                        .any(|r| &r.id == id && r.stop_requested_by.is_none())
                });
                let still_open: BTreeSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
                open.retain(|o| still_open.contains(o.id.as_str()));
                for row in rows {
                    let known = open.iter().position(|o| o.id == row.id);
                    if row.stop_requested_by.is_some() {
                        if let Some(k) = known {
                            open.remove(k);
                        }
                        if !stopped.contains(&row.id) {
                            stopped.push(row.id);
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
                                        "request {} names a step this loop's graph does not \
                                         have; waiting for a config that has it",
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
                    deferred.remove(&row.id);
                    let roots = row.roots.iter().map(|r| graph.by_id[r]).collect();
                    admit(row.id, roots, open);
                }
                *turned_off = turned_off_of(graph, &all_turned_off);
                *wipes = store.open_wipes().await?;
            }
        }
        Ok(())
    }

    /// Each wipe whose steps have stopped, done; each that cannot be done,
    /// closed with why. The rest wait, their steps held. Whether a reset
    /// opened a request for what reads what it emptied.
    #[allow(clippy::too_many_arguments)]
    async fn carry_out_wipes(
        &self,
        graph: &Graph,
        next_graph: Option<&Graph>,
        store: &Store,
        record: &mut Recorded<'_>,
        state: &mut Record,
        wipes: &mut Vec<WipeRow>,
        facts: &mut Facts,
        slots: &mut [Slot],
    ) -> Result<bool> {
        if wipes.is_empty() {
            return Ok(false);
        }
        // Read now rather than when announced: the config that drops a
        // purged group is written just before the purge is asked for.
        let fresh = self.reload.as_ref().and_then(|s| s.load().ok());
        let latest = fresh.as_ref().or(next_graph).unwrap_or(graph);
        let mut opened = false;
        for w in std::mem::take(wipes) {
            let Some(kind) = w.kind else {
                store
                    .close_wipe(&w.id, Some("a kind of wipe this build does not know"))
                    .await?;
                continue;
            };
            if let Some(why) = wipe::refusal(latest, kind, &w.targets) {
                store.close_wipe(&w.id, Some(&why)).await?;
                continue;
            }
            let held = wipe::held(graph, kind, &w.targets);
            let stopped = held
                .iter()
                .all(|&i| facts.steps[i].running.is_none() && slots[i].live.is_none());
            // A purge waits for the config swap too: a step the graph
            // still has would be written back into the record.
            let ready = match kind {
                WipeKind::Reset => stopped,
                WipeKind::Purge => held.is_empty(),
            };
            if !ready {
                wipes.push(w);
                continue;
            }
            for &i in &held {
                if let Some(e) = slots[i].ended.take() {
                    let s = &mut slots[i];
                    self.finish(
                        graph,
                        state,
                        &mut s.status,
                        i,
                        e.status,
                        e.error,
                        e.exit,
                        e.attempts,
                    );
                }
            }
            let done = match kind {
                WipeKind::Reset => self.reset_in_loop(graph, &w.targets, state, facts).await,
                WipeKind::Purge => delete_trees(&self.data_root, &w.targets)
                    .await
                    .map(|()| wipe::forget_groups(state, &w.targets)),
            };
            record.save(state).await?;
            match done {
                Ok(()) => {
                    let roots = match kind {
                        WipeKind::Reset => wipe::after_reset(graph, &w.targets),
                        WipeKind::Purge => Vec::new(),
                    };
                    if !roots.is_empty() {
                        store.open_request(&roots, &w.opened_by).await?;
                        opened = true;
                    }
                    store.close_wipe(&w.id, None).await?;
                }
                Err(e) => store.close_wipe(&w.id, Some(&format!("{e:#}"))).await?,
            }
        }
        Ok(opened)
    }

    /// Each target invoked with `--reset store` and recorded emptied, so
    /// what reads it sees a new version.
    async fn reset_in_loop(
        &self,
        graph: &Graph,
        targets: &[String],
        state: &mut Record,
        facts: &mut Facts,
    ) -> Result<()> {
        for step in targets {
            let (i, version) = self.reset_one(graph, step).await?;
            state.steps.insert(
                step.clone(),
                wipe::emptied(state.steps.get(step), version.clone()),
            );
            facts.sinks[i] = Some(version);
            facts.steps[i] = StepFacts {
                streams_output: graph.steps[i].streams_output,
                ..Default::default()
            };
        }
        Ok(())
    }

    /// One tick with no request open, its states saved: for a host whose
    /// loop is idle when a step is turned off or on, or that has just taken
    /// the lock from a loop that died with steps running.
    /// The switches it recorded, as the store held them: a host compares
    /// the switches it finds later with these, not with any it read before.
    pub async fn settle(&self, graph: &Graph, store: &Store) -> Result<BTreeMap<String, String>> {
        let mut record = Recorded::load(store).await?;
        let mut state = record.saved.clone();
        let all = store.turned_off().await?;
        let turned_off = turned_off_of(graph, &all);
        let intent = Intent {
            requests: Vec::new(),
            turned_off: turned_off.keys().copied().collect(),
        };
        let shape = shape_of(graph, &self.lock_slots);
        let t = tick(&shape, &intent, &facts_of(graph, &state));
        let held = vec![None; graph.steps.len()];
        record_states(graph, &shape, &mut state, &t, &held, &turned_off, |_| {
            Vec::new()
        });
        record.save(&state).await?;
        Ok(all)
    }

    fn note(&self, step: &str, msg: String) {
        self.sink.emit(&Event::Log {
            step: step.to_string(),
            level: crate::events::LogLevel::Info,
            msg,
            ts: None,
            stream: None,
            target: None,
            thread: None,
            fields: None,
        });
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

    /// A step a request wanted, with no row of its own yet and nothing
    /// left to finish, takes the one its last state settles on.
    fn settle_row(
        &self,
        graph: &Graph,
        state: &mut Record,
        queue: &mut QueueLedger,
        s: &mut Slot,
        i: usize,
        cancelled: bool,
    ) {
        if !s.ever_in_scope || s.status.is_some() {
            return;
        }
        let Some(st) = s.last_wanted.settles(cancelled) else {
            return;
        };
        if st == StepStatus::SkippedUpToDate {
            queue.cleared(graph, i, &*self.sink);
        }
        self.finish(graph, state, &mut s.status, i, st, None, None, 0);
    }

    /// Once per invocation that reported no version: what that costs its
    /// consumers, where a person reading the run's log will see it.
    fn say_unversioned(&self, spec: &crate::step::StepSpec) {
        self.sink.emit(&Event::Log {
            step: spec.id.clone(),
            level: crate::events::LogLevel::Info,
            msg: format!(
                "reported no version for {}, so every step reading it runs again; a \
                 version derived from what it wrote lets them skip when nothing changed",
                spec.output().as_str()
            ),
            ts: None,
            stream: None,
            target: None,
            thread: None,
            fields: None,
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn on_signal(
        &self,
        graph: &Graph,
        signal: crate::step::StepSignal,
        facts: &mut Facts,
        state: &mut Record,
        queue: &mut QueueLedger,
        slots: &mut [Slot],
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
        let fingerprint = slots[p]
            .live
            .as_ref()
            .map_or(&graph.fingerprints[p], |l| &l.consumed.fingerprint);
        let qualified = format!("{fingerprint}:{version}");
        let moved = facts.sinks[p].as_deref() != Some(qualified.as_str());
        facts.sinks[p] = Some(qualified.clone());
        state.steps.entry(step.clone()).or_default().version = Some(qualified.clone());
        queue.sealed(graph, p, &qualified, rows, &*self.sink);
        if moved {
            self.sink.emit(&Event::Checkpoint {
                step: step.clone(),
                version,
                rows,
            });
        }
        if !facts.steps[p].streams_output && !slots[p].warned_not_streaming {
            slots[p].warned_not_streaming = true;
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
        live: &Live,
        facts: &mut Facts,
        state: &mut Record,
        queue: &mut QueueLedger,
    ) -> Ended {
        let spec = &graph.steps[i];
        let consumed = &live.consumed;
        let fingerprint = &consumed.fingerprint;
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
        match res {
            Ok(outcome) => {
                let v = match reported_version(spec, fingerprint, &outcome.outputs) {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        self.say_unversioned(spec);
                        fresh_version(fingerprint, &live.invocation)
                    }
                    Err(e) => return failed(FailureKind::Data, format!("{e:#}")),
                };
                let moved = prior.as_ref() != Some(&v);
                facts.sinks[i] = Some(v.clone());
                let rows = outcome.outputs.first().and_then(|o| o.rows);
                queue.sealed(graph, i, &v, rows, &*self.sink);
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
                entry.version = Some(v);
                entry.succeeded = true;
                entry.fingerprint = fingerprint.clone();
                entry.needs_rerun = false;
                facts.steps[i].last_success = Some(consumed.clone());
                facts.steps[i].needs_rerun = false;
                Ended {
                    status: StepStatus::Succeeded {
                        changed: moved as usize,
                    },
                    error: None,
                    exit,
                    attempts,
                    pass_ended: false,
                }
            }
            Err(step_err) => {
                // What a failed or stopped step reports it committed, its
                // consumers read (plans/supervisor.md §2.5). One that reports
                // nothing moves nothing: its tree may be mid-write.
                if let Ok(Some(v)) = reported_version(spec, fingerprint, &step_err.outputs) {
                    facts.sinks[i] = Some(v.clone());
                    state.steps.entry(spec.id.clone()).or_default().version = Some(v);
                }
                failed(step_err.kind, format!("{:#}", step_err.error))
            }
        }
    }
}

/// Step index → the wipe holding it, for the steps this graph has.
fn wiping(graph: &Graph, wipes: &[WipeRow]) -> BTreeMap<usize, WipeKind> {
    wipes
        .iter()
        .filter_map(|w| Some((w.kind?, w)))
        .flat_map(|(kind, w)| {
            wipe::held(graph, kind, &w.targets)
                .into_iter()
                .map(move |i| (i, kind))
        })
        .collect()
}

/// A held step reads as off, which is not what a person did: say why.
fn say_wiping(graph: &Graph, state: &mut Record, t: &Tick, wiping: &BTreeMap<usize, WipeKind>) {
    for (&i, kind) in wiping {
        let why = match kind {
            WipeKind::Reset => "being reset",
            WipeKind::Purge => "being deleted",
        };
        if let Some(entry) = state.steps.get_mut(&graph.steps[i].id) {
            entry.state_detail = Some(match t.states[i] {
                Row::Running => format!("stopping: {why}"),
                _ => why.to_string(),
            });
        }
    }
}

/// The trees of `groups` under the data root. One already gone is not an
/// error: the record still needs forgetting.
async fn delete_trees(root: &std::path::Path, groups: &[String]) -> Result<()> {
    for group in groups {
        let tree = root.join(group);
        let removed = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(tree))
            .await
            .context("the delete panicked")?;
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => anyhow::bail!("delete {group}/: {e}"),
        }
    }
    Ok(())
}

/// Step index → who turned it off, for the steps this graph has.
async fn turned_off_in(graph: &Graph, store: &Store) -> Result<BTreeMap<usize, String>> {
    Ok(turned_off_of(graph, &store.turned_off().await?))
}

fn turned_off_of(graph: &Graph, all: &BTreeMap<String, String>) -> BTreeMap<usize, String> {
    all.iter()
        .filter_map(|(id, by)| Some((*graph.by_id.get(id)?, by.clone())))
        .collect()
}

/// Write what the tick made of each step into its record. A step between
/// passes (`held`: its invocation ended, and what it reads has not
/// settled) waits for the producer named there. `serving` names the open requests a step
/// has work left in, oldest first.
fn record_states(
    graph: &Graph,
    shape: &Shape,
    state: &mut Record,
    t: &Tick,
    held: &[Option<usize>],
    turned_off: &BTreeMap<usize, String>,
    serving: impl Fn(usize) -> Vec<String>,
) {
    let id = |j: usize| graph.steps[j].id.as_str();
    for (i, &st) in t.states.iter().enumerate() {
        let st = match held[i] {
            Some(p) => Row::Waiting(Wait::Upstream(p)),
            None => st,
        };
        let turned_off_by = turned_off.get(&i).cloned();
        let detail = match st {
            Row::Running if t.stops.contains(&i) => Some(match &turned_off_by {
                Some(by) => format!("stopping: turned off by {by}"),
                None => "stopping: no open request wants it".to_string(),
            }),
            Row::Off => turned_off_by
                .as_ref()
                .map(|by| format!("turned off by {by}")),
            Row::Blocked(p) => Some(format!(
                "{} has published nothing and is not going to run",
                id(p)
            )),
            Row::Waiting(Wait::Upstream(p)) => Some(format!("waiting for {}", id(p))),
            Row::Waiting(Wait::Reader(r)) => Some(format!(
                "waiting for {}, which reads what this writes",
                id(r)
            )),
            Row::Waiting(Wait::Lock(l)) => {
                let holders: Vec<&str> = (0..t.states.len())
                    .filter(|&j| t.states[j] == Row::Running)
                    .filter(|&j| shape.steps[j].locks.iter().any(|&(h, _)| h == l))
                    .map(id)
                    .collect();
                Some(format!(
                    "waiting for lock {}, held by {}",
                    shape.locks[l].name,
                    holders.join(", ")
                ))
            }
            _ => None,
        };
        let entry = state.steps.entry(id(i).to_string()).or_default();
        entry.state = Some(st.into());
        entry.state_detail = detail;
        entry.turned_off_by = turned_off_by;
        entry.requests = serving(i);
    }
}

/// A step this loop's graph lacks reads as waiting while a request the
/// loop has set aside names it, and as nothing once none does.
fn record_deferred(graph: &Graph, state: &mut Record, deferred: &BTreeMap<String, Vec<String>>) {
    for root in deferred.values().flatten() {
        state.steps.entry(root.clone()).or_default();
    }
    for (id, st) in state.steps.iter_mut() {
        if graph.by_id.contains_key(id) {
            continue;
        }
        let requests: Vec<String> = deferred
            .iter()
            .filter(|(_, roots)| roots.contains(id))
            .map(|(r, _)| r.clone())
            .collect();
        let waiting = !requests.is_empty();
        st.state = waiting.then_some(super::tick::StateKind::Waiting);
        st.state_detail = waiting.then(|| {
            "waiting for the sync in progress to take on a config that has this step".to_string()
        });
        st.requests = requests;
    }
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

/// What is waiting on the channel now, without waiting for more.
fn queued<T>(rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> Vec<T> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// `v` over a new graph: `from_old[i]` is where step `i` of the new graph
/// was in the old one. A step new to the graph starts from nothing.
fn carry<T: Default>(v: &mut [T], from_old: &[Option<usize>]) -> Vec<T> {
    from_old
        .iter()
        .map(|o| o.map(|o| std::mem::take(&mut v[o])).unwrap_or_default())
        .collect()
}

fn remap_consumed(c: Consumed, to_new: &HashMap<usize, usize>) -> Consumed {
    Consumed {
        fingerprint: c.fingerprint,
        reads: c
            .reads
            .into_iter()
            .filter_map(|(p, v)| Some((*to_new.get(&p)?, v)))
            .collect(),
    }
}

/// The same steps in the same places, each with the same definition and
/// held apart the same way: nothing a swap would move. When a step may
/// run is not in its fingerprint, so it is compared here.
fn same_graph(a: &Graph, b: &Graph) -> bool {
    let when = |g: &Graph| -> Vec<_> {
        g.steps
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    s.streams_output,
                    s.reads_pinned,
                    s.locks.clone(),
                )
            })
            .collect()
    };
    a.fingerprints == b.fingerprints && a.locks == b.locks && when(a) == when(b)
}

/// Today's graph as the tick sees it: each step writes the sink its own
/// index names, reads the sinks of the steps it names as inputs, and holds
/// its named locks, sized as the config declares them unless `slots` (the
/// host's `--parallelism`) says otherwise.
fn shape_of(graph: &Graph, slots: &BTreeMap<String, usize>) -> Shape {
    let locks: Vec<LockShape> = graph
        .locks
        .iter()
        .map(|l| LockShape {
            name: l.name.clone(),
            slots: slots.get(&l.name).copied().unwrap_or(l.slots),
        })
        .collect();
    let lock_ix = |name: &str| {
        locks
            .iter()
            .position(|l| l.name == name)
            .unwrap_or_else(|| panic!("the loader declares every lock a step holds: {name}"))
    };
    let steps = graph
        .steps
        .iter()
        .enumerate()
        .map(|(i, spec)| StepShape {
            reads: graph.deps_in_order(i).collect(),
            fingerprint: graph.fingerprints[i].clone(),
            pins_reads: spec.reads_pinned,
            locks: super::locks::held_by(spec)
                .iter()
                .map(|(name, hold)| (lock_ix(name), *hold))
                .collect(),
        })
        .collect();
    Shape {
        steps,
        topo: graph.topo.clone(),
        locks,
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
            needs_rerun: recorded(i).is_some_and(|s| s.needs_rerun),
        })
        .collect();
    Facts { sinks, steps }
}

/// The steps that answered a launch's `--migrate` with `needs_rerun` and
/// have not succeeded since, turned-off ones aside: what the app offers to
/// run after an upgrade.
pub fn rerun_offer(graph: &Graph, state: &Record) -> Vec<StepId> {
    graph
        .steps
        .iter()
        .filter_map(|spec| state.steps.get(&spec.id).map(|st| (spec, st)))
        .filter(|(_, st)| st.needs_rerun && st.turned_off_by.is_none())
        .map(|(spec, _)| spec.id.clone())
        .collect()
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
    use crate::supervisor::reload::GraphSource;
    use crate::supervisor::tick::StateKind;
    use crate::EventSink;

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
    async fn a_turned_off_step_is_not_started_and_its_request_closes() {
        let f = fixture();
        f.go.store(true, Ordering::SeqCst);
        let other = Store::open(f.root.path()).await.unwrap();
        other.turn_off("a/raw", "claude").await.unwrap();
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
        assert_eq!(a.state, Some(StateKind::Off));
        assert_eq!(a.turned_off_by.as_deref(), Some("claude"));
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

    /// A config a test rewrites while the loop runs.
    struct Swappable(std::sync::Mutex<Graph>);

    impl Swappable {
        fn new(graph: &Graph) -> Arc<Self> {
            Arc::new(Self(std::sync::Mutex::new(graph.clone())))
        }

        fn set(&self, specs: Vec<StepSpec>) {
            *self.0.lock().unwrap() = Graph::build(specs).unwrap();
        }
    }

    impl GraphSource for Swappable {
        fn load(&self) -> Result<Graph> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    /// What the server does when its watch sees `config.toml` move.
    fn announce_config(root: &std::path::Path) {
        use crate::supervisor::announce::{announce, listeners_dir, CONFIG_CHANGED, FROM_SERVER};
        announce(&listeners_dir(root), FROM_SERVER, CONFIG_CHANGED);
    }

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<Event>>);

    impl EventSink for Recorder {
        fn emit(&self, event: &Event) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    impl Recorder {
        fn logged(&self, step: &str, text: &str) -> bool {
            self.0.lock().unwrap().iter().any(
                |e| matches!(e, Event::Log { step: s, msg, .. } if s == step && msg.contains(text)),
            )
        }
    }

    struct Three {
        root: tempfile::TempDir,
        runs: [Arc<AtomicU32>; 3],
        go: Arc<AtomicBool>,
        events: Arc<Recorder>,
    }

    impl Three {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
                runs: std::array::from_fn(|_| Arc::new(AtomicU32::new(0))),
                go: Arc::new(AtomicBool::new(false)),
                events: Arc::default(),
            }
        }

        fn step(&self, k: usize) -> StepSpec {
            let id = ["a/raw", "b/raw", "c/raw"][k];
            let stopped = Arc::new(AtomicBool::new(false));
            source(id, self.runs[k].clone(), self.go.clone(), stopped)
        }

        fn runs(&self, k: usize) -> u32 {
            self.runs[k].load(Ordering::SeqCst)
        }

        fn serve(
            &self,
            graph: Graph,
            config: Arc<Swappable>,
        ) -> tokio::task::JoinHandle<Result<RunReport>> {
            let root = self.root.path().to_path_buf();
            let events = self.events.clone();
            tokio::spawn(async move {
                let store = Store::open(&root).await?;
                let report = Runner::new(&root)
                    .sink(events)
                    .reload_from(config)
                    .serve(&graph, &store)
                    .await;
                store.close().await;
                report
            })
        }
    }

    /// The calendars on 2026-09-24: added to the config during a long gmail
    /// sync, they read Queued until it ended. The loop re-reads the config
    /// and starts them beside it.
    #[tokio::test]
    async fn a_source_added_to_the_config_mid_loop_starts_at_once() {
        let t = Three::new();
        let graph = Graph::build(vec![t.step(0), t.step(1)]).unwrap();
        let config = Swappable::new(&graph);
        let other = Store::open(t.root.path()).await.unwrap();
        let first = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let running = t.serve(graph, config.clone());
        until("a to start", || t.runs(0) == 1).await;

        config.set(vec![t.step(0), t.step(1), t.step(2)]);
        announce_config(t.root.path());
        let later = other.open_request(&["c/raw".into()], "ui").await.unwrap();
        until("c to start while a runs", || t.runs(2) == 1).await;
        assert_eq!(outcome(&other, &first).await, None, "a is still going");

        t.go.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
        for id in [&first, &later] {
            assert_eq!(outcome(&other, id).await, Some(Some(RequestOutcome::Done)));
        }
        assert_eq!((t.runs(0), t.runs(1)), (1, 0));
    }

    /// A config saved mid-edit can drop a step that is downloading. It is
    /// not interrupted: the loop keeps its graph until the step ends, and
    /// what the new config adds waits with it.
    #[tokio::test]
    async fn a_config_that_drops_a_running_step_is_taken_on_once_it_ends() {
        let t = Three::new();
        let graph = Graph::build(vec![t.step(0), t.step(1)]).unwrap();
        let config = Swappable::new(&graph);
        let other = Store::open(t.root.path()).await.unwrap();
        let first = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let running = t.serve(graph, config.clone());
        until("a to start", || t.runs(0) == 1).await;

        config.set(vec![t.step(1), t.step(2)]);
        announce_config(t.root.path());
        let later = other.open_request(&["c/raw".into()], "ui").await.unwrap();
        let c = until_recorded(t.root.path(), "c/raw", "waiting", |st| {
            st.state == Some(StateKind::Waiting)
        })
        .await;
        assert_eq!(c.requests, [later.as_str()]);
        assert_eq!(t.runs(2), 0, "c waits for the step the config dropped");

        t.go.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
        assert_eq!(
            outcome(&other, &first).await,
            Some(Some(RequestOutcome::Done))
        );
        assert_eq!(
            outcome(&other, &later).await,
            Some(Some(RequestOutcome::Done))
        );
        assert_eq!(t.runs(2), 1);
    }

    /// A step edited while it runs finishes on the definition it started
    /// with, and its record says so: the next busy period reads that
    /// record, and the new definition must read as out of date there too.
    /// The request that still wants it then runs it again under the new one.
    #[tokio::test]
    async fn a_step_edited_while_it_runs_records_what_it_ran_and_runs_again() {
        let root = tempfile::tempdir().unwrap();
        let runs = Arc::new(AtomicU32::new(0));
        let gates = [
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ];
        let step = |code_version: &'static str| {
            let (runs, gates) = (runs.clone(), gates.clone());
            StepSpec::new(
                "a/raw",
                StepRun::in_process(move |ctx: StepCtx| {
                    let (runs, gates) = (runs.clone(), gates.clone());
                    async move {
                        let run = runs.fetch_add(1, Ordering::SeqCst) as usize;
                        while !gates[run.min(1)].load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("f"), code_version).unwrap();
                        Ok(StepOutcome::default())
                    }
                }),
            )
            .code_version(code_version)
        };
        let graph = Graph::build(vec![step("1")]).unwrap();
        let ran_with = graph.fingerprints[0].clone();
        let config = Swappable::new(&graph);
        let events = Arc::new(Recorder::default());
        let other = Store::open(root.path()).await.unwrap();
        let first = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let running = {
            let (root, config, events) =
                (root.path().to_path_buf(), config.clone(), events.clone());
            tokio::spawn(async move {
                let store = Store::open(&root).await?;
                let report = Runner::new(&root)
                    .sink(events)
                    .reload_from(config)
                    .serve(&graph, &store)
                    .await;
                store.close().await;
                report
            })
        };
        until("a to start", || runs.load(Ordering::SeqCst) == 1).await;

        config.set(vec![step("2")]);
        announce_config(root.path());
        let edited = config.load().unwrap().fingerprints[0].clone();
        assert_ne!(edited, ran_with);
        until("the loop to take the edit on", || {
            events.logged("a/raw", "changed in the config")
        })
        .await;
        gates[0].store(true, Ordering::SeqCst);
        until("a to run again", || runs.load(Ordering::SeqCst) == 2).await;
        let record = crate::supervisor::record::recorded(root.path()).await;
        assert_eq!(record.steps["a/raw"].fingerprint, ran_with);

        gates[1].store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
        assert_eq!(
            outcome(&other, &first).await,
            Some(Some(RequestOutcome::Done))
        );
        let record = crate::supervisor::record::recorded(root.path()).await;
        assert_eq!(record.steps["a/raw"].fingerprint, edited);
    }

    /// A step that counts its runs and writes its tree.
    fn counted(id: &str, inputs: &[&str], runs: Arc<AtomicU32>) -> StepSpec {
        let mut spec = StepSpec::new(
            id,
            StepRun::in_process(move |ctx: StepCtx| {
                let runs = runs.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let dir = ctx.path_str(&ctx.step_id);
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(dir.join("f"), "x").unwrap();
                    Ok(StepOutcome::default())
                }
            }),
        );
        for i in inputs {
            spec = spec.input(i);
        }
        spec
    }

    /// After an upgrade `b`'s render answered `--migrate` with
    /// `needs_rerun`. A sync of `a` leaves it alone; the offer names it; a
    /// request rooted at the offer runs it without `b`'s download, and its
    /// success takes it off the offer.
    #[tokio::test]
    async fn a_render_that_needs_a_rerun_waits_for_the_offer() {
        let root = tempfile::tempdir().unwrap();
        let b_raw = Arc::new(AtomicU32::new(0));
        let b_render = Arc::new(AtomicU32::new(0));
        let graph = Graph::build(vec![
            counted("a/raw", &[], Arc::default()),
            counted("a/render", &["a/raw"], Arc::default()),
            counted("b/raw", &[], b_raw.clone()),
            counted("b/render", &["b/raw"], b_render.clone()),
            counted("index/grid", &["a/render", "b/render"], Arc::default()),
        ])
        .unwrap();
        Runner::new(root.path())
            .run_roots(&graph, &["a/raw", "b/raw"])
            .await
            .unwrap();
        assert_eq!(b_render.load(Ordering::SeqCst), 1);
        let store = Store::open(root.path()).await.unwrap();
        let saved = store.load_record().await.unwrap();
        let mut marked = saved.clone();
        marked.steps.get_mut("b/render").unwrap().needs_rerun = true;
        store.save_record(&saved, &marked).await.unwrap();
        store.close().await;

        Runner::new(root.path())
            .run_roots(&graph, &["a/raw"])
            .await
            .unwrap();
        assert_eq!(
            b_render.load(Ordering::SeqCst),
            1,
            "not pulled into a's sync"
        );
        let record = crate::supervisor::record::recorded(root.path()).await;
        let offer = rerun_offer(&graph, &record);
        assert_eq!(offer, ["b/render"]);

        let roots: Vec<&str> = offer.iter().map(String::as_str).collect();
        Runner::new(root.path())
            .run_roots(&graph, &roots)
            .await
            .unwrap();
        assert_eq!(b_render.load(Ordering::SeqCst), 2);
        assert_eq!(b_raw.load(Ordering::SeqCst), 1, "its download did not run");
        let record = crate::supervisor::record::recorded(root.path()).await;
        assert!(rerun_offer(&graph, &record).is_empty());
        assert_eq!(
            record.steps["index/grid"].reads.get("b/render"),
            record.steps["b/render"].version.as_ref(),
            "the index read b's render after it ran"
        );
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
        assert_eq!(a.requests, [id.as_str()]);
        let b = crate::supervisor::record::recorded(f.root.path())
            .await
            .steps["b/raw"]
            .clone();
        assert_eq!(b.state, Some(StateKind::Stale), "never succeeded");
        assert!(b.requests.is_empty());

        f.go.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
        let a = crate::supervisor::record::recorded(f.root.path())
            .await
            .steps["a/raw"]
            .clone();
        assert_eq!(a.state, Some(StateKind::Idle));
        assert!(a.requests.is_empty());
    }

    /// Two sources' syncs share the index both feed. The one whose download
    /// is done serves nothing — its row offers Sync, not a Stop of a sync
    /// it has no part left in — while the index serves both until it runs.
    #[tokio::test]
    async fn a_step_done_with_its_part_serves_no_request_while_the_fan_in_waits() {
        let root = tempfile::tempdir().unwrap();
        let (quick, held) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
        );
        let runs = || Arc::new(AtomicU32::new(0));
        let stopped = || Arc::new(AtomicBool::new(false));
        let graph = Arc::new(
            Graph::build(vec![
                source("a/raw", runs(), quick.clone(), stopped()),
                source("b/raw", runs(), held.clone(), stopped()),
                source("index/all", runs(), quick.clone(), stopped())
                    .input("a/raw")
                    .input("b/raw"),
            ])
            .unwrap(),
        );
        let other = Store::open(root.path()).await.unwrap();
        let ra = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let rb = other.open_request(&["b/raw".into()], "ui").await.unwrap();
        let running = {
            let (root, graph) = (root.path().to_path_buf(), graph.clone());
            tokio::spawn(async move {
                let store = Store::open(&root).await?;
                let report = Runner::new(&root).serve(&graph, &store).await;
                store.close().await;
                report
            })
        };

        let a = until_recorded(root.path(), "a/raw", "done with its part", |st| {
            st.state == Some(StateKind::Fresh)
        })
        .await;
        assert!(a.requests.is_empty(), "{:?}", a.requests);
        let record = crate::supervisor::record::recorded(root.path()).await;
        assert_eq!(record.steps["b/raw"].requests, [rb.as_str()]);
        assert_eq!(
            record.steps["index/all"].requests,
            [ra.as_str(), rb.as_str()]
        );

        held.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
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
            st.requests.is_empty()
        })
        .await;
        assert_eq!(a.state, Some(StateKind::Running), "still running");
        assert_eq!(
            a.state_detail.as_deref(),
            Some("stopping: no open request wants it")
        );
        // The loop saves the row before it closes the request, so a closed
        // request has no step serving it; the reverse order is not promised.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while outcome(&other, &id).await != Some(Some(RequestOutcome::Stopped)) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the request to close as stopped: {:?}",
                outcome(&other, &id).await
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let_go.store(true, Ordering::SeqCst);
        running.await.unwrap().unwrap();
        let a = crate::supervisor::record::recorded(root.path()).await.steps["a/raw"].clone();
        assert_ne!(a.state, Some(StateKind::Running));
        assert_eq!(a.last_run.unwrap().status, "stopped");
    }

    /// A turn-off landing while no loop runs reaches the rows only through a
    /// tick; `settle` is that tick, with no request open.
    #[tokio::test]
    async fn a_turn_off_while_the_loop_is_idle_reads_once_settled() {
        let f = fixture();
        let store = Store::open(f.root.path()).await.unwrap();
        let runner = Runner::new(f.root.path());
        store.turn_off("a/raw", "claude").await.unwrap();
        runner.settle(&f.graph, &store).await.unwrap();
        let a = crate::supervisor::record::recorded(f.root.path())
            .await
            .steps["a/raw"]
            .clone();
        assert_eq!(a.state, Some(StateKind::Off));
        assert_eq!(a.turned_off_by.as_deref(), Some("claude"));
        assert_eq!(a.state_detail.as_deref(), Some("turned off by claude"));

        store.turn_on("a/raw").await.unwrap();
        runner.settle(&f.graph, &store).await.unwrap();
        let a = crate::supervisor::record::recorded(f.root.path())
            .await
            .steps["a/raw"]
            .clone();
        assert_eq!(
            (a.state, a.turned_off_by, a.state_detail),
            (Some(StateKind::Stale), None, None)
        );
    }

    /// A loop with nowhere to re-read its graph cannot take on a source
    /// added to the config while it runs. Its request waits for the next
    /// loop, which loads the config again, rather than failing as a step
    /// that does not exist.
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
        assert_eq!(c.requests, [later.as_str()]);
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

    /// With nothing asked of it the loop returns rather than waiting for a
    /// request, and starts no step.
    #[tokio::test]
    async fn a_loop_with_no_open_request_ends_at_once() {
        let f = fixture();
        // A loop that waited for a request would never return; the deadline
        // only tells that from slow store I/O on a loaded runner, and stays
        // under the `small` test timeout so a hang fails here by name.
        tokio::time::timeout(Duration::from_secs(30), serve(&f))
            .await
            .expect("the loop to return with no request open")
            .unwrap()
            .unwrap();
        assert_eq!(f.runs[0].load(Ordering::SeqCst), 0);
    }
}
