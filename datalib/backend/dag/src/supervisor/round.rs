//! The supervisor's loop: every open request ticked until it closes, and
//! the record kept true while none is open. The facts come from the record
//! in `system/supervisor.sqlite` and go back to it, and the events are the
//! ones the run store and the server read.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::announce::{Listener, CONFIG_CHANGED};
use super::inputs;
use super::loop_host::LoopHost;
use super::pass::PassState;
use super::record::{InvocationEnd, InvocationRow};
use super::store::{JobKind, JobRow, RequestOutcome, Store};
use super::tick::{
    tick, Attempt, Consumed, Facts, Intent, LockShape, Outcome, Request, Running, Seq, Shape,
    StepFacts, StepShape, StepState as Row, Tick, Wait,
};
use super::wipe;
use crate::artifact::ArtifactPath;
use crate::disk_space::{self, DiskFloor, LowDisk};
use crate::events::{Event, PlannedStep, StepProgress};
use crate::graph::Graph;
use crate::run_state::RunState;
use crate::scheduler::{
    apply_answer, fresh_version, invoke_with_retry, mark_running, new_run_id, now_stamp,
    reported_version, step_summary, InvokeError, PrepareError, QueueLedger, RunReport, Runner,
    StepReport, StepStatus, REFUSED_PREPARE,
};
use crate::step::{Exit, FailureKind, StepCtx, StepError, StepId, StepOutcome, StopSignal};
use crate::subprocess::Shrink;
use crate::supervisor::record::{CurrentRun, Record};
use crate::version::UNKNOWN;

/// How often an open round looks at the disk while the config sets a
/// floor. Nothing announces a disk filling or being freed.
const DISK_LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

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

/// What the loop holds for one step, by its id: a config swap leaves it
/// where it is. Besides the round's bookkeeping, the facts the record does
/// not hold, which [`facts_of`] lays over the record's.
#[derive(Default)]
struct Slot {
    running: Option<Running>,
    /// The latest invocation to finish, whatever its outcome.
    last_attempt: Option<Tried>,
    /// What the step said of its output at its last start, over what its
    /// config entry says.
    streams_output: Option<bool>,
    /// Whether an open request has wanted it this round.
    ever_in_scope: bool,
    last_wanted: LastWanted,
    live: Option<Live>,
    ended: Option<Ended>,
    warned_not_streaming: bool,
    said_waiting_for_disk: bool,
    /// What the round's report says of it.
    status: Option<StepStatus>,
    attempts: u32,
    error: Option<String>,
    /// Passes that failed and were run again this round.
    failed_passes: u32,
}

type Slots = BTreeMap<String, Slot>;

/// Step `i` of `graph`'s slot, made if it has none.
fn slot_of<'a>(slots: &'a mut Slots, graph: &Graph, i: usize) -> &'a mut Slot {
    slots.entry(graph.steps[i].id.clone()).or_default()
}

fn slot_at<'a>(slots: &'a Slots, graph: &Graph, i: usize) -> Option<&'a Slot> {
    slots.get(&graph.steps[i].id)
}

/// What an invocation read, by producer id. The tick's [`Consumed`] is by
/// index in one graph; this means the same under the next config.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Read {
    fingerprint: String,
    reads: BTreeMap<String, Option<String>>,
}

impl Read {
    fn of(graph: &Graph, consumed: &Consumed) -> Read {
        Read {
            fingerprint: consumed.fingerprint.clone(),
            reads: consumed
                .reads
                .iter()
                .map(|(&p, v)| (graph.steps[p].id.clone(), v.clone()))
                .collect(),
        }
    }

    /// In `graph`'s indices: a producer it lacks is no longer read.
    fn in_graph(&self, graph: &Graph) -> Consumed {
        Consumed {
            fingerprint: self.fingerprint.clone(),
            reads: self
                .reads
                .iter()
                .filter_map(|(id, v)| Some((*graph.by_id.get(id)?, v.clone())))
                .collect(),
        }
    }
}

/// [`Attempt`], by producer id.
#[derive(Debug, Clone)]
struct Tried {
    started: Seq,
    failed: bool,
    stopped: bool,
    read: Read,
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
    consumed: Read,
    /// Taken when the loop tells it to stop: what ends then was asked to.
    stop: Option<watch::Sender<bool>>,
}

/// Who opens the jobs the loop opens for itself.
pub const LOOP_OPENER: &str = "the loop";

/// What a task the loop spawned ended with.
enum Done {
    /// A step's invocation: its id, its attempts, and how it ended.
    Step(String, u32, Result<StepOutcome, StepError>),
    /// A step's `--prepare`.
    Prepared(String, Result<StepOutcome, InvokeError>),
}

/// How many `--prepare` processes the pass runs at once. Most exit at once
/// with nothing to do, but after an upgrade that moved many raw stores each
/// one rewrites its store, and all of them at once would crowd the disk the
/// loop's other steps are using.
const PREPARES_AT_ONCE: usize = 4;

/// The prepare pass at the start of [`Runner::run_forever`]: every step
/// whose program takes the verb is run once with `--prepare` appended. A
/// step's `--prepare` starts once that of every step upstream of it has
/// exited, up to [`PREPARES_AT_ONCE`] at a time, while the rest of the loop
/// goes on. A step whose `--prepare` has not exited, and every step
/// downstream of one, does not start a normal run.
struct PreparePass {
    /// Its row in the store's `jobs`, whose `job_steps` say how it stands
    /// with each step, in `order`.
    job: String,
    order: Vec<String>,
    /// In graph order.
    not_started: Vec<String>,
    running: BTreeSet<String>,
    /// Who had each step turned off as the pass began: one the runner
    /// turned off for refusing is turned back on once its `--prepare`
    /// exits without refusing.
    turned_off_before: BTreeMap<String, String>,
}

impl PreparePass {
    fn ord(&self, step: &str) -> usize {
        self.order
            .iter()
            .position(|s| s == step)
            .expect("the pass runs --prepare only for the steps it planned")
    }

    /// The steps that may not start a normal run yet: each whose
    /// `--prepare` has not exited, and every step downstream of one.
    fn holds(&self, graph: &Graph) -> Vec<bool> {
        graph.downstream_of(&self.not_finished(graph))
    }

    fn not_finished(&self, graph: &Graph) -> Vec<usize> {
        self.not_started
            .iter()
            .chain(&self.running)
            .filter_map(|id| graph.by_id.get(id).copied())
            .collect()
    }

    /// The steps whose `--prepare` may start now, in graph order: no step
    /// upstream of one, however far back, has a `--prepare` still to
    /// start or still running.
    fn startable(&self, graph: &Graph) -> Vec<String> {
        let after: Vec<usize> = self
            .not_finished(graph)
            .into_iter()
            .flat_map(|i| graph.dependents[i].iter().copied())
            .collect();
        let behind = graph.downstream_of(&after);
        let room = PREPARES_AT_ONCE.saturating_sub(self.running.len());
        self.not_started
            .iter()
            .filter(|id| graph.by_id.get(*id).is_some_and(|&i| !behind[i]))
            .take(room)
            .cloned()
            .collect()
    }

    fn is_over(&self) -> bool {
        self.running.is_empty() && self.not_started.is_empty()
    }
}

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
    /// A request named a step the config file has and this graph does
    /// not: the loop re-reads the file before it waits again, rather than
    /// waiting for the server's watch to say it moved.
    reload: bool,
}

/// An open request as the loop holds it: its row's id, its roots by id,
/// and when the loop took it on.
struct Open {
    id: String,
    roots: Vec<String>,
    opened: Seq,
}

impl Open {
    /// The tick's view of it. A swap waits until every root is in the
    /// graph, so none is dropped here.
    fn request(&self, graph: &Graph) -> Request {
        Request {
            roots: self
                .roots
                .iter()
                .filter_map(|r| graph.by_id.get(r).copied())
                .collect(),
            opened: self.opened,
        }
    }
}

/// One busy period: from the first request or wipe the loop takes on with
/// none open, to the moment none is left and nothing runs. Only the run
/// store's grouping, and what its summary compares against.
struct Period {
    run_id: String,
    /// The same clock its steps get in `DATALIB_DAG_NOW`.
    clock: datalib_time::RunClock,
    /// Each step's version as the period began: the summary says whether
    /// the period as a whole moved a step's output.
    at_start: HashMap<String, Option<String>>,
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
    /// until none is left: one busy period, its steps given the runner's
    /// own environment and its events its own sink. The caller holds
    /// `runner-lock`.
    pub async fn serve(&self, graph: &Graph, store: &Store) -> Result<RunReport> {
        let report = self.drive(graph, store, None, true).await?;
        Ok(report.expect("a loop that serves until idle ends the period it opened"))
    }

    /// The loop for a host holding `runner-lock` until `stop`: every
    /// request and wipe served as it comes, and the record brought to the
    /// switches and the config while nothing runs. Each stretch with work
    /// open is a busy period, which `host` records.
    pub async fn run_forever(
        &self,
        graph: &Graph,
        store: &Store,
        host: &dyn LoopHost,
    ) -> Result<()> {
        self.drive(graph, store, Some(host), false)
            .await
            .map(|_| ())
    }

    /// [`Runner::run_forever`] until nothing is open and nothing runs: what
    /// a `datalib-pipeline` verb runs when no other process holds the lock.
    /// The last busy period's report.
    pub async fn run_until_idle(
        &self,
        graph: &Graph,
        store: &Store,
        host: &dyn LoopHost,
    ) -> Result<Option<RunReport>> {
        self.drive(graph, store, Some(host), true).await
    }

    /// `host` is `None` for [`Runner::serve`], which opens one period
    /// at once and returns its report when it ends. `until_idle` returns
    /// once nothing is open and nothing runs.
    async fn drive(
        &self,
        graph: &Graph,
        store: &Store,
        host: Option<&dyn LoopHost>,
        until_idle: bool,
    ) -> Result<Option<RunReport>> {
        let mut mailbox = Mailbox {
            store,
            seen: None,
            started: false,
            deferred: BTreeMap::new(),
            stopped: Vec::new(),
            reload: false,
        };
        let mut record = Recorded::load(store).await?;
        let mut state = record.saved.clone();
        let mut period = match host {
            None => Some(self.open_period(graph, &mut record, &mut state).await?),
            Some(_) => None,
        };
        let mut report = None;

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
        let mut slots: Slots = BTreeMap::new();
        let mut open: Vec<Open> = Vec::new();
        // Step id → who turned it off, whether or not the graph has it.
        let mut turned_off: BTreeMap<String, String> = BTreeMap::new();
        let mut wipes: Vec<JobRow> = Vec::new();
        let mut seq = 0u64;
        let mut queue = QueueLedger::default();
        let mut cancelled = false;
        // Whether the loop takes on what the mailbox brings: not once it is
        // told to stop, nor, for `serve`, once its one period is over.
        let mut taking = true;
        let mut stop_rx = self.stop.clone();
        let mut disk = DiskWatch::default();
        let mut prepare = match host {
            Some(_) => self.plan_prepare(graph, store).await?,
            None => None,
        };

        self.refresh(
            graph,
            &mut mailbox,
            period.is_some(),
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
            if taking && std::mem::take(&mut config_moved) {
                // One that does not load is waited out on the graph the
                // loop has; the Manage screen already says it is broken.
                if let Some(Ok(next)) = self.reload.as_ref().map(|s| s.load()) {
                    next_graph = Some(next);
                }
            }
            if let Some(next) = next_graph.take().filter(|_| taking) {
                let roots: BTreeSet<&str> = open
                    .iter()
                    .flat_map(|o| o.roots.iter().map(String::as_str))
                    .collect();
                let still_needed: Vec<&str> = current
                    .steps
                    .iter()
                    .map(|s| s.id.as_str())
                    .filter(|&id| {
                        let running = slots.get(id).is_some_and(|s| s.running.is_some());
                        (running || roots.contains(id)) && !next.by_id.contains_key(id)
                    })
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
                    // A step the new graph lacks, done but for what it
                    // reads settling, is done now, under the graph it ran in.
                    for (i, spec) in old.steps.iter().enumerate() {
                        if graph.by_id.contains_key(&spec.id) {
                            continue;
                        }
                        if let Some(s) = slots.get_mut(&spec.id) {
                            if let Some(e) = s.ended.take() {
                                self.finish_ended(&old, &mut state, s, i, e);
                            }
                        }
                    }
                    shape = shape_of(graph, &self.lock_slots);
                    for o in &open {
                        let reached = graph.downstream_of(&o.request(graph).roots);
                        for (i, _) in reached.iter().enumerate().filter(|(_, &r)| r) {
                            slot_of(&mut slots, graph, i).ever_in_scope = true;
                        }
                    }
                    // Read the requests again: one left for want of a step
                    // this graph has is taken on now.
                    mailbox.seen = None;
                    let mut added = Vec::new();
                    for (i, spec) in graph.steps.iter().enumerate().filter(|_| period.is_some()) {
                        match old.by_id.get(&spec.id) {
                            None => {
                                added.push(PlannedStep::of(spec));
                                self.note(&spec.id, "added to the config during this sync".into());
                            }
                            Some(&o) if old.fingerprints[o] != graph.fingerprints[i] => {
                                self.note(&spec.id, "changed in the config during this sync".into())
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
            if taking {
                self.refresh(
                    graph,
                    &mut mailbox,
                    period.is_some(),
                    &mut open,
                    &mut turned_off,
                    &mut wipes,
                    &mut seq,
                    &mut slots,
                )
                .await?;
                if std::mem::take(&mut mailbox.reload) {
                    config_moved = true;
                    continue;
                }
                if let Some(host) = host.filter(|_| period.is_none()) {
                    if !open.is_empty() || !wipes.is_empty() || prepare.is_some() {
                        let start = super::host::period_start(
                            &store.open_requests().await?,
                            &store.open_jobs().await?,
                        );
                        match host.open(start) {
                            Ok(env) => {
                                self.set_steps_env(env.into());
                                period =
                                    Some(self.open_period(graph, &mut record, &mut state).await?);
                                // What it took on was read with no period
                                // open, as strictly as a period's first read.
                                mailbox.started = true;
                                queue = QueueLedger::default();
                            }
                            Err(why) => {
                                tracing::warn!("supervisor: cannot sync: {why}");
                                for o in open.drain(..) {
                                    store
                                        .close_request(&o.id, RequestOutcome::Failed, None)
                                        .await?;
                                }
                                // Read again when the store next moves.
                                wipes.clear();
                            }
                        }
                    }
                }
                let opened = self
                    .carry_out_wipes(
                        graph,
                        next_graph.as_ref(),
                        store,
                        &mut record,
                        &mut state,
                        &mut wipes,
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
                        period.is_some(),
                        &mut open,
                        &mut turned_off,
                        &mut wipes,
                        &mut seq,
                        &mut slots,
                    )
                    .await?;
                }
            }
            if period.is_some() {
                if let Some(pass) = prepare.as_mut() {
                    for task in self.start_what_it_can(graph, store, pass).await? {
                        set.spawn(task);
                    }
                }
                if let Some(pass) = prepare.take_if(|p| p.is_over()) {
                    store.close_job(&pass.job, None).await?;
                    // A verb run from the command line does what it was
                    // asked and says what needs to run again; the app's
                    // loop runs it.
                    if !until_idle && self.open_the_reruns(graph, store, &state).await? {
                        mailbox.seen = None;
                        continue;
                    }
                }
            }
            let wiping = wiping(graph, &wipes);
            // A step being wiped leaves the requests that named it: a reset
            // does not refill a download, and the config that drops a
            // purged group is not waited on. A request left with nothing is
            // stopped.
            open.retain_mut(|o| {
                o.roots
                    .retain(|r| graph.by_id.get(r).is_none_or(|i| !wiping.contains_key(i)));
                if o.roots.is_empty() {
                    mailbox.stopped.push(o.id.clone());
                }
                !o.roots.is_empty()
            });
            let off = turned_off_of(graph, &turned_off);
            let intent = Intent {
                requests: open.iter().map(|o| o.request(graph)).collect(),
                turned_off: off.keys().chain(wiping.keys()).copied().collect(),
            };
            let low = disk.look(&self.data_root, graph.disk_floor);
            let mut facts = facts_of(graph, &state, &slots);
            facts.low_disk = low.is_some();
            if let Some(pass) = &prepare {
                for (f, held) in facts.steps.iter_mut().zip(pass.holds(graph)) {
                    f.preparing = held;
                }
            }
            let t = tick(&shape, &intent, &facts);
            for scope in &t.scopes {
                for (i, _) in scope.iter().enumerate().filter(|(_, &w)| w) {
                    slot_of(&mut slots, graph, i).ever_in_scope = true;
                }
            }
            for (i, &st) in t.states.iter().enumerate() {
                match LastWanted::of(graph, st) {
                    LastWanted::Unwanted => {}
                    wanted => slot_of(&mut slots, graph, i).last_wanted = wanted,
                }
            }
            let starting: BTreeSet<usize> = t.starts.iter().map(|s| s.step).collect();
            let n = graph.steps.len();
            let between_passes = |slots: &Slots, i: usize| {
                slot_at(slots, graph, i).is_some_and(|s| s.ended.is_some())
            };

            // A step between passes has not settled either, so the index
            // behind a render that is waiting on its download's next seal
            // keeps reading Running too, not just the render.
            let mut unsettled = vec![false; n];
            for &i in &graph.topo {
                let upstream = graph.deps_in_order(i).any(|p| unsettled[p]);
                unsettled[i] = matches!(t.states[i], Row::Running | Row::Waiting(_))
                    || (between_passes(&slots, i) && upstream);
            }

            for i in 0..n {
                let Some(s) = slots.get_mut(&graph.steps[i].id) else {
                    continue;
                };
                let Some(e) = s.ended.as_mut() else { continue };
                let again = starting.contains(&i);
                let upstream_busy = graph.deps_in_order(i).any(|p| unsettled[p]);
                if (again || upstream_busy) && !e.pass_ended {
                    e.pass_ended = true;
                    self.sink.emit(&Event::PassEnd {
                        step: graph.steps[i].id.clone(),
                        status: e.status.state(),
                        error: e.error.clone(),
                        exit_code: e.exit.and_then(|x| x.code),
                        signal: e.exit.and_then(|x| x.signal),
                    });
                }
                if again {
                    // Its run reports the pass that runs now, so a failure
                    // here would be lost but for this count.
                    if e.status.state() == RunState::Failed {
                        s.failed_passes += 1;
                    }
                    s.ended = None;
                } else if !upstream_busy {
                    let e = s.ended.take().expect("checked above");
                    self.finish_ended(graph, &mut state, s, i, e);
                }
            }

            // A step with nothing of its own left to finish says so now
            // rather than when the loop ends, which may be a long sync of
            // some other source away: one no open request wants any more,
            // and one up to date with nothing above it that could still
            // change what it reads.
            let mut above_unsettled = vec![false; n];
            for &i in &graph.topo {
                above_unsettled[i] = graph
                    .deps_in_order(i)
                    .any(|p| unsettled[p] || above_unsettled[p]);
            }
            for (i, &above) in above_unsettled.iter().enumerate() {
                let done = match t.states[i] {
                    Row::Idle | Row::Stale => true,
                    Row::Fresh => !above,
                    _ => false,
                };
                let Some(s) = slots.get_mut(&graph.steps[i].id) else {
                    continue;
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
            let held: Vec<Option<usize>> = (0..n)
                .map(|i| {
                    between_passes(&slots, i)
                        .then(|| graph.deps_in_order(i).find(|&p| unsettled[p]))
                        .flatten()
                })
                .collect();
            // A step serves a request only while it has work left in it,
            // so a source whose part is done offers Sync again while the
            // index its request also reaches is still running.
            let working: Vec<bool> = (0..n)
                .map(|i| {
                    held[i].is_some()
                        || match t.states[i] {
                            Row::Running | Row::Waiting(_) => true,
                            Row::Fresh => above_unsettled[i],
                            _ => false,
                        }
                })
                .collect();
            record_states(graph, &shape, &mut state, &t, &held, &off, low, |i| {
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
                slot_of(&mut slots, graph, i).running = Some(Running { started: Seq(seq) });
                facts.steps[i].running = Some(Running { started: Seq(seq) });
                let (stop, rx) = watch::channel(false);
                let ctx = self.ctx_for(graph, &facts, &state, i, &start.consumed, &checkpoint, rx);
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
                slot_of(&mut slots, graph, i).live = Some(Live {
                    invocation: invocation.id,
                    consumed: Read::of(graph, &start.consumed),
                    stop: Some(stop),
                });
                let run = graph.steps[i].run.clone();
                let retry = self.retry.clone();
                let sink = self.sink.clone();
                let child_env = self.steps_env();
                let id = graph.steps[i].id.clone();
                set.spawn(async move {
                    let (attempts, res) =
                        invoke_with_retry(&run, ctx, &retry, &sink, &child_env).await;
                    Done::Step(id, attempts, res)
                });
            }
            for &i in &t.stops {
                let id = &graph.steps[i].id;
                let Some(live) = slots.get_mut(id).and_then(|s| s.live.as_mut()) else {
                    continue;
                };
                if let Some(stop) = live.stop.take() {
                    if let Some(low) = low.filter(|_| !turned_off.contains_key(id)) {
                        self.note(id, format!("stopped: {}", low.describe()));
                    }
                    let _ = stop.send(true);
                }
            }
            // The step's log, not only its row, says so: a one-shot
            // `datalib-pipeline` prints the log and nothing of the row, and
            // would otherwise sit with no word on its stream of why.
            for i in 0..n {
                let waiting = t.states[i] == Row::Waiting(Wait::DiskSpace);
                let said = slot_at(&slots, graph, i).is_some_and(|s| s.said_waiting_for_disk);
                if waiting == said {
                    continue;
                }
                if let Some(low) = low.filter(|_| waiting) {
                    self.note(
                        &graph.steps[i].id,
                        format!("waiting for disk space: {}", low.describe()),
                    );
                }
                slot_of(&mut slots, graph, i).said_waiting_for_disk = waiting;
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
                if wipes.is_empty() && prepare.is_none() {
                    if let Some(p) = period.take() {
                        report = Some(
                            self.end_period(
                                graph,
                                &mut record,
                                &mut state,
                                &mut slots,
                                &mut queue,
                                cancelled,
                                p,
                                host,
                            )
                            .await?,
                        );
                        mailbox.started = false;
                        taking &= !until_idle;
                        // The next tick, with nothing open, is the record
                        // at rest.
                        continue;
                    }
                    if !taking || until_idle {
                        break;
                    }
                }
            }
            let listening = taking;
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
                        self.on_signal(graph, signal, &mut state, &mut queue, &mut slots);
                    }
                }
                Some(()) = wait_for_stop(&mut stop_rx), if !cancelled => {
                    // The host is going: stop what runs and take nothing
                    // new. The store's requests and wipes stay open, for
                    // whoever runs the loop next.
                    cancelled = true;
                    taking = false;
                    open.clear();
                    wipes.clear();
                    // The next host to take the lock runs it again.
                    prepare = None;
                }
                heard = listener.next(), if listening => {
                    config_moved |= heard.iter().any(|line| line == CONFIG_CHANGED);
                }
                _ = tokio::time::sleep(DISK_LOOK_EVERY),
                    if listening && period.is_some() && graph.disk_floor.is_some() => {}
                // Empty while the disk holds every step back.
                joined = set.join_next(), if !set.is_empty() => {
                    // Seals before joins: a step sends its seal before its
                    // task can finish, so every seal this join follows is
                    // queued by now.
                    for signal in queued(&mut checkpoints) {
                        self.on_signal(graph, signal, &mut state, &mut queue, &mut slots);
                    }
                    let joined = joined
                        .expect("a live task implies a joinable one")
                        .context("step task panicked")?;
                    let (id, attempts, res) = match joined {
                        Done::Step(id, attempts, res) => (id, attempts, res),
                        Done::Prepared(step, answer) => {
                            // A stop drops the pass, not the answer in flight.
                            let at = prepare.as_mut().map(|pass| {
                                pass.running.remove(&step);
                                (pass.job.clone(), pass.ord(&step),
                                    pass.turned_off_before.get(&step).cloned())
                            });
                            self.answered(graph, store, &mut state, &step, at, answer).await?;
                            mailbox.seen = None;
                            record.save(&state).await?;
                            continue;
                        }
                    };
                    // A graph that drops a running step waits for it.
                    let i = graph.by_id[&id];
                    let s = slots.get_mut(&id).expect("a joined step was started");
                    let live = s.live.take().expect("a joined step was started");
                    let started = s.running.take().map(|r| r.started).unwrap_or(Seq(0));
                    let e = self.on_ended(graph, i, attempts, res, &live, &slots,
                        &mut state, &mut queue);
                    let s = slots.get_mut(&id).expect("a joined step was started");
                    s.last_attempt = Some(Tried {
                        started,
                        failed: !matches!(e.status, StepStatus::Succeeded { .. }),
                        // Asked to stop, and it did: one that says it
                        // failed has failed, whatever it was asked.
                        stopped: live.stop.is_none()
                            && e.status.state() == crate::run_state::RunState::Stopped,
                        read: live.consumed,
                    });
                    store.close_invocation(&live.invocation, &e.as_end()).await?;
                    s.attempts = e.attempts;
                    s.error = e.error.clone();
                    s.ended = Some(e);
                    record.save(&state).await?;
                }
            }
        }

        Ok(report)
    }

    /// The pass a host's loop runs as it starts: every step whose program
    /// takes `--prepare`, in graph order, as a job in the store with a row
    /// per step. One a loop before this one left open is closed first.
    /// `None` when no step's program takes the verb, its job closed at
    /// once so a reader sees the pass is over.
    async fn plan_prepare(&self, graph: &Graph, store: &Store) -> Result<Option<PreparePass>> {
        for left in store.open_jobs().await? {
            if left.kind == Some(JobKind::Prepare) {
                store
                    .close_job(
                        &left.id,
                        Some("the loop running it ended; the next asks again"),
                    )
                    .await?;
            }
        }
        let asked = super::upgrade::steps_to_ask(graph);
        let job = store.open_job(JobKind::Prepare, &[], LOOP_OPENER).await?;
        if asked.is_empty() {
            store.close_job(&job, None).await?;
            return Ok(None);
        }
        tracing::info!(
            steps = asked.len(),
            "supervisor: asking each step to prepare"
        );
        let ids: Vec<&str> = asked.iter().map(String::as_str).collect();
        store.begin_job_steps(&job, &ids).await?;
        Ok(Some(PreparePass {
            job,
            order: asked.clone(),
            not_started: asked,
            running: BTreeSet::new(),
            turned_off_before: store.turned_off().await?,
        }))
    }

    /// A `--prepare` task for each step [`PreparePass::startable`] names,
    /// each marked running in the job. A step this graph no longer has is
    /// dropped from the pass, marked failed.
    async fn start_what_it_can(
        &self,
        graph: &Graph,
        store: &Store,
        pass: &mut PreparePass,
    ) -> Result<Vec<impl std::future::Future<Output = Done> + Send + 'static>> {
        let (kept, gone): (Vec<String>, Vec<String>) = std::mem::take(&mut pass.not_started)
            .into_iter()
            .partition(|id| graph.by_id.contains_key(id));
        pass.not_started = kept;
        for step in gone {
            let why = "the config no longer has this step";
            store
                .set_job_step(
                    &pass.job,
                    pass.ord(&step),
                    PassState::Failed,
                    Some(why),
                    None,
                )
                .await?;
        }
        let startable = pass.startable(graph);
        pass.not_started.retain(|id| !startable.contains(id));
        let mut tasks = Vec::new();
        for step in startable {
            store
                .set_job_step(&pass.job, pass.ord(&step), PassState::Running, None, None)
                .await?;
            let task = self.verb_task(&graph.steps[graph.by_id[&step]], crate::step::Verb::Prepare);
            pass.running.insert(step.clone());
            tasks.push(async move { Done::Prepared(step, task.await) });
        }
        Ok(tasks)
    }

    /// One step's answer to `--prepare`, in the loop's record: a new
    /// version or `needs_rerun` recorded; a refusal turns it off by
    /// [`REFUSED_PREPARE`] until a later pass finds it willing; anything
    /// else is said and the pass goes on.
    #[allow(clippy::too_many_arguments)]
    /// `at` is the pass's job, the step's place in it, and who had it
    /// turned off as the pass began; `None` once a stop dropped the pass.
    async fn answered(
        &self,
        graph: &Graph,
        store: &Store,
        state: &mut Record,
        step: &str,
        at: Option<(String, usize, Option<String>)>,
        answer: std::result::Result<StepOutcome, InvokeError>,
    ) -> Result<()> {
        let turned_off_before = at.as_ref().and_then(|(_, _, by)| by.clone());
        let answer = match answer {
            Ok(outcome) => match graph.by_id.get(step) {
                Some(&i) => apply_answer(graph, i, &outcome, state)
                    .map(|()| outcome.needs_rerun)
                    .map_err(|e| PrepareError::Failed(format!("{e:#}"))),
                None => Ok(false),
            },
            Err(InvokeError::Refused(why)) => {
                store
                    .turn_off(step, &format!("{REFUSED_PREPARE}: {why}"))
                    .await?;
                Err(PrepareError::Refused(why))
            }
            Err(InvokeError::Failed(e) | InvokeError::NotStarted(e)) => {
                Err(PrepareError::Failed(e))
            }
        };
        let refused_before = turned_off_before.is_some_and(|by| by.starts_with(REFUSED_PREPARE));
        if refused_before && !matches!(answer, Err(PrepareError::Refused(_))) {
            store.turn_on(step).await?;
        }
        let (state, error) = match answer {
            Ok(false) => (PassState::Done, None),
            Ok(true) => {
                tracing::info!(step, "supervisor: needs to run again before it is current");
                (PassState::Done, None)
            }
            Err(PrepareError::Refused(why)) => {
                let why = format!("refused, and is turned off until it no longer refuses: {why}");
                tracing::error!(step, "supervisor: could not prepare: {why}");
                (PassState::Failed, Some(why))
            }
            Err(PrepareError::Failed(why)) => {
                tracing::error!(step, "supervisor: could not prepare: {why}");
                (PassState::Failed, Some(why))
            }
        };
        if let Some((job, ord, _)) = at {
            store
                .set_job_step(&job, ord, state, error.as_deref(), None)
                .await?;
        }
        Ok(())
    }

    /// Run what the pass marked `needs_rerun`, without waiting to be
    /// asked: nothing reads a store in another shape, so what those steps
    /// wrote is not shown until they run, and none of them downloads. One
    /// request, unless one this requester opened is still open. Whether it
    /// opened one.
    async fn open_the_reruns(&self, graph: &Graph, store: &Store, state: &Record) -> Result<bool> {
        use super::serve::UPGRADE_REQUESTER;
        let roots = rerun_offer(graph, state);
        if roots.is_empty() {
            return Ok(false);
        }
        let open = store.open_requests().await?;
        if open.iter().any(|r| r.opened_by == UPGRADE_REQUESTER) {
            return Ok(false);
        }
        let id = store.open_request(&roots, UPGRADE_REQUESTER).await?;
        tracing::info!(
            request = %id,
            steps = %roots.join(", "),
            "supervisor: running the steps the prepare pass could not bring up to date"
        );
        Ok(true)
    }

    /// The period's record opened: its plan said, its run id and "now"
    /// recorded as the record's current run. The steps' environment
    /// carries both.
    async fn open_period(
        &self,
        graph: &Graph,
        record: &mut Recorded<'_>,
        state: &mut Record,
    ) -> Result<Period> {
        let child_env = self.steps_env();
        let plan: Vec<PlannedStep> = graph
            .topo
            .iter()
            .map(|&i| PlannedStep::of(&graph.steps[i]))
            .collect();
        self.sink.emit(&Event::RunPlan { steps: plan });
        let started_at = child_env
            .get(datalib_step_protocol::env::DATALIB_DAG_NOW)
            .cloned()
            .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs());
        let clock = datalib_time::RunClock::starting_at(
            datalib_time::parse_strict(&started_at)
                .unwrap_or_else(|_| datalib_time::IsoOffsetTimestamp::now_local()),
        );
        let run_id = child_env
            .get(datalib_step_protocol::env::DATALIB_DAG_RUN_ID)
            .cloned()
            .unwrap_or_else(new_run_id);
        state.current_run = Some(CurrentRun {
            run_id: run_id.clone(),
            started_at,
            finished_at: None,
            states: Default::default(),
        });
        record.save(state).await?;
        let at_start = state
            .steps
            .iter()
            .map(|(id, s)| (id.clone(), s.version.clone()))
            .collect();
        Ok(Period {
            run_id,
            clock,
            at_start,
        })
    }

    /// Nothing is open and nothing runs: each step the period reached
    /// finished, the record's run closed, and the summary sent. A step no
    /// request reached took no part, and has no line.
    #[allow(clippy::too_many_arguments)]
    async fn end_period(
        &self,
        graph: &Graph,
        record: &mut Recorded<'_>,
        state: &mut Record,
        slots: &mut Slots,
        queue: &mut QueueLedger,
        cancelled: bool,
        period: Period,
        host: Option<&dyn LoopHost>,
    ) -> Result<RunReport> {
        for i in 0..graph.steps.len() {
            let Some(s) = slots.get_mut(&graph.steps[i].id) else {
                continue;
            };
            if let Some(e) = s.ended.take() {
                self.finish_ended(graph, state, s, i, e);
            }
            self.settle_row(graph, state, queue, s, i, cancelled);
        }
        if let Some(run) = state.current_run.as_mut() {
            run.finished_at = Some(period.clock.now().to_rfc3339_micros());
        }
        record.save(state).await?;
        let facts = facts_of(graph, state, slots);
        let steps = graph
            .topo
            .iter()
            .filter_map(|&i| {
                let s = slot_at(slots, graph, i)?;
                let path = graph.steps[i].output().as_str().to_string();
                let now = facts.sinks[i]
                    .clone()
                    .unwrap_or_else(|| UNKNOWN.to_string());
                let before = period.at_start.get(&graph.steps[i].id).cloned().flatten();
                let changed = facts.sinks[i].is_some() && facts.sinks[i] != before;
                // A pass's own `changed` is against the seal before it; the
                // period's is against where the period started.
                let status = match s.status.clone()? {
                    StepStatus::Succeeded { .. } => StepStatus::Succeeded {
                        changed: changed as usize,
                    },
                    other => other,
                };
                Some(StepReport {
                    id: graph.steps[i].id.clone(),
                    status,
                    attempts: s.attempts,
                    error: s.error.clone(),
                    failed_passes: s.failed_passes,
                    outputs: vec![(path, now, changed)],
                })
            })
            .collect();
        let report = RunReport { steps };
        self.sink.emit(&Event::RunSummary {
            steps: report.steps.iter().map(step_summary).collect(),
        });
        if let Some(host) = host {
            host.close(&period.run_id);
        }
        // Nothing runs: what the slots hold of the facts the record does
        // not, a step's last attempt among them, ends with the period.
        slots.clear();
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
        in_period: bool,
        open: &mut Vec<Open>,
        turned_off: &mut BTreeMap<String, String>,
        wipes: &mut Vec<JobRow>,
        seq: &mut u64,
        slots: &mut Slots,
    ) -> Result<()> {
        let mut admit = |id: String, roots: Vec<String>, open: &mut Vec<Open>| {
            let at: Vec<usize> = roots.iter().map(|r| graph.by_id[r]).collect();
            for (i, _) in graph
                .downstream_of(&at)
                .iter()
                .enumerate()
                .filter(|(_, &r)| r)
            {
                slot_of(slots, graph, i).ever_in_scope = true;
            }
            *seq += 1;
            open.push(Open {
                id,
                roots,
                opened: Seq(*seq),
            });
        };
        let on_disk = |roots: &[String]| {
            self.reload
                .as_ref()
                .and_then(|s| s.load().ok())
                .is_some_and(|g| roots.iter().all(|r| g.by_id.contains_key(r)))
        };
        let Mailbox {
            store,
            seen,
            started,
            deferred,
            stopped,
            reload,
        } = mailbox;
        {
            {
                let version = store.data_version().await?;
                if *seen == Some(version) {
                    return Ok(());
                }
                // Read with no period open, or the first read of one: the
                // config it names cannot be newer than the graph the loop
                // has, so a step this graph lacks does not exist.
                let first = !*started;
                *started |= in_period;
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
                        // The config file can be ahead of the graph: the
                        // server says it moved only once its watch sees it,
                        // and a request written just after can arrive first.
                        let ahead = first && on_disk(&row.roots);
                        if !first || ahead {
                            let lacked = row
                                .roots
                                .iter()
                                .filter(|r| !graph.by_id.contains_key(*r))
                                .cloned()
                                .collect();
                            if deferred.insert(row.id.clone(), lacked).is_none() {
                                *reload |= ahead;
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
                    admit(row.id, row.roots, open);
                }
                *turned_off = all_turned_off;
                // The loop's own prepare pass is not a wipe it carries out.
                *wipes = store
                    .open_jobs()
                    .await?
                    .into_iter()
                    .filter(|j| j.kind != Some(JobKind::Prepare))
                    .collect();
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
        wipes: &mut Vec<JobRow>,
        slots: &mut Slots,
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
                    .close_job(&w.id, Some("a kind of wipe this build does not know"))
                    .await?;
                continue;
            };
            if let Some(why) = wipe::refusal(latest, kind, &w.targets) {
                store.close_job(&w.id, Some(&why)).await?;
                continue;
            }
            let held = wipe::held(graph, kind, &w.targets);
            let stopped = held.iter().all(|&i| {
                slot_at(slots, graph, i).is_none_or(|s| s.running.is_none() && s.live.is_none())
            });
            // A purge waits for the config swap too: a step the graph
            // still has would be written back into the record.
            let ready = match kind {
                JobKind::Reset | JobKind::Compact | JobKind::Collect => stopped,
                JobKind::Purge | JobKind::Prepare => held.is_empty(),
            };
            if !ready {
                wipes.push(w);
                continue;
            }
            for &i in &held {
                let Some(s) = slots.get_mut(&graph.steps[i].id) else {
                    continue;
                };
                if let Some(e) = s.ended.take() {
                    self.finish_ended(graph, state, s, i, e);
                }
            }
            let done = match kind {
                JobKind::Reset => self.reset_in_loop(graph, &w.targets, state, slots).await,
                JobKind::Purge => delete_trees(&self.data_root, &w.targets)
                    .await
                    .map(|()| wipe::forget_groups(state, &w.targets)),
                JobKind::Compact => {
                    self.shrink_in_loop(graph, &w, Shrink::Compact, state, store)
                        .await
                }
                JobKind::Collect => {
                    self.shrink_in_loop(graph, &w, Shrink::Collect, state, store)
                        .await
                }
                JobKind::Prepare => unreachable!("`wipe::refusal` refuses a prepare job"),
            };
            record.save(state).await?;
            match done {
                Ok(()) => {
                    let roots = match kind {
                        JobKind::Reset => wipe::after_reset(graph, &w.targets),
                        // A fold or a collection kept every store's content,
                        // so nothing downstream has anything new to read.
                        JobKind::Purge | JobKind::Compact | JobKind::Collect | JobKind::Prepare => {
                            Vec::new()
                        }
                    };
                    if !roots.is_empty() {
                        store.open_request(&roots, &w.opened_by).await?;
                        opened = true;
                    }
                    store.close_job(&w.id, None).await?;
                }
                Err(e) => store.close_job(&w.id, Some(&format!("{e:#}"))).await?,
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
        slots: &mut Slots,
    ) -> Result<()> {
        for step in targets {
            let (_, version) = self.reset_one(graph, step).await?;
            state.steps.insert(
                step.clone(),
                wipe::emptied(state.steps.get(step), version.clone()),
            );
            // What it said and how it last ended were of the store it
            // emptied.
            slots.remove(step);
        }
        Ok(())
    }

    /// Each step the compaction or collection asks invoked with `how`'s
    /// flag, and the version it reports recorded. One that fails is named
    /// and the rest go on.
    async fn shrink_in_loop(
        &self,
        graph: &Graph,
        w: &JobRow,
        how: Shrink,
        state: &mut Record,
        store: &Store,
    ) -> Result<()> {
        let asked = wipe::compact_targets(graph, &w.targets, how);
        let ids: Vec<&str> = asked.iter().map(|&i| graph.steps[i].id.as_str()).collect();
        store.begin_job_steps(&w.id, &ids).await?;
        let mut failed: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for (ord, &i) in asked.iter().enumerate() {
            let step = &graph.steps[i].id;
            store
                .set_job_step(&w.id, ord, PassState::Running, None, None)
                .await?;
            match self.shrink_one(graph, step, how).await {
                Ok((version, bytes)) => {
                    if let Some(version) = version {
                        state.steps.entry(step.clone()).or_default().version = Some(version);
                    }
                    store
                        .set_job_step(&w.id, ord, PassState::Done, None, bytes)
                        .await?;
                }
                Err(e) => {
                    store
                        .set_job_step(&w.id, ord, PassState::Failed, Some(&e), None)
                        .await?;
                    failed.entry(e).or_default().push(step);
                }
            }
        }
        anyhow::ensure!(
            failed.is_empty(),
            "could not free up disk in {}",
            wipe::by_cause(&failed)
        );
        Ok(())
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

    #[allow(clippy::too_many_arguments)]
    fn ctx_for(
        &self,
        graph: &Graph,
        facts: &Facts,
        state: &Record,
        i: usize,
        consumed: &Consumed,
        checkpoint: &crate::step::CheckpointSink,
        stop: watch::Receiver<bool>,
    ) -> StepCtx {
        let spec = &graph.steps[i];
        let producers: Vec<usize> = graph.deps_in_order(i).collect();
        let paths = |ps: Vec<usize>| -> Vec<ArtifactPath> {
            ps.into_iter().map(|p| graph.steps[p].output()).collect()
        };
        let changed_inputs = paths(inputs::changed(
            &producers,
            facts.steps[i].last_success.as_ref(),
            consumed,
        ));
        let recorded = |p: usize| {
            let last = state.steps.get(&graph.steps[p].id)?.last_run.as_ref()?;
            RunState::parse(&last.status)
        };
        let failed_inputs = paths(
            producers
                .iter()
                .copied()
                .filter(|&p| inputs::ended_short(&facts.steps[p], recorded(p)))
                .collect(),
        );
        StepCtx {
            step_id: spec.id.clone(),
            data_root: self.data_root.clone(),
            inputs: graph.resolved_inputs[i].clone(),
            changed_inputs,
            failed_inputs,
            reads: paths_of(graph, consumed).into_iter().collect(),
            checkpoint: checkpoint.clone(),
            progress: StepProgress::new(spec.id.clone(), self.sink.clone()),
            stop: StopSignal::new(stop).with_grace(self.stop_grace),
        }
    }

    /// A step done with its last invocation: its finish, after the count
    /// of the passes before that one that failed, which replaces the
    /// count its last run left.
    fn finish_ended(&self, graph: &Graph, state: &mut Record, s: &mut Slot, i: usize, e: Ended) {
        self.sink.emit(&Event::Metric {
            step: graph.steps[i].id.clone(),
            name: datalib_metrics::FAILED_PASSES.to_string(),
            labels: BTreeMap::new(),
            value: i64::from(s.failed_passes),
        });
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

    fn on_signal(
        &self,
        graph: &Graph,
        signal: crate::step::StepSignal,
        state: &mut Record,
        queue: &mut QueueLedger,
        slots: &mut Slots,
    ) {
        use crate::step::StepSignal;
        let (step, version, rows) = match signal {
            StepSignal::Capabilities {
                step,
                streams_output,
            } => {
                if graph.by_id.contains_key(&step) {
                    slots.entry(step).or_default().streams_output = Some(streams_output);
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
        let Some(s) = slots.get_mut(&step).filter(|s| s.running.is_some()) else {
            return;
        };
        let fingerprint = s
            .live
            .as_ref()
            .map_or(&graph.fingerprints[p], |l| &l.consumed.fingerprint);
        let qualified = format!("{fingerprint}:{version}");
        let entry = state.steps.entry(step.clone()).or_default();
        let moved = entry.version.as_deref() != Some(qualified.as_str());
        entry.version = Some(qualified.clone());
        queue.sealed(graph, p, &qualified, rows, &*self.sink);
        if moved {
            self.sink.emit(&Event::Checkpoint {
                step: step.clone(),
                version,
                rows,
            });
        }
        let streams = s.streams_output.unwrap_or(graph.steps[p].streams_output);
        if !streams && !s.warned_not_streaming {
            s.warned_not_streaming = true;
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
    fn on_ended(
        &self,
        graph: &Graph,
        i: usize,
        attempts: u32,
        res: Result<StepOutcome, StepError>,
        live: &Live,
        slots: &Slots,
        state: &mut Record,
        queue: &mut QueueLedger,
    ) -> Ended {
        let spec = &graph.steps[i];
        let consumed = &live.consumed.in_graph(graph);
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
                let rows = outcome.outputs.first().and_then(|o| o.rows);
                queue.sealed(graph, i, &v, rows, &*self.sink);
                let consumed_paths = paths_of(graph, consumed);
                queue.consumed(
                    graph,
                    i,
                    &consumed_paths,
                    |p| slot_at(slots, graph, p).is_none_or(|s| s.running.is_none()),
                    &*self.sink,
                );
                let entry = state.steps.entry(spec.id.clone()).or_default();
                entry.reads = consumed_paths.into_iter().collect();
                entry.version = Some(v);
                entry.succeeded = true;
                entry.fingerprint = fingerprint.clone();
                entry.needs_rerun = false;
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
                // consumers read (dag/README.md §"Versions: reported by the step"). One that reports
                // nothing moves nothing: its tree may be mid-write.
                if let Ok(Some(v)) = reported_version(spec, fingerprint, &step_err.outputs) {
                    state.steps.entry(spec.id.clone()).or_default().version = Some(v);
                }
                failed(step_err.kind, format!("{:#}", step_err.error))
            }
        }
    }
}

/// Step index → the wipe holding it, for the steps this graph has.
fn wiping(graph: &Graph, wipes: &[JobRow]) -> BTreeMap<usize, JobKind> {
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
fn say_wiping(graph: &Graph, state: &mut Record, t: &Tick, wiping: &BTreeMap<usize, JobKind>) {
    for (&i, kind) in wiping {
        let why = match kind {
            JobKind::Reset => "being reset",
            JobKind::Purge => "being deleted",
            JobKind::Compact => "being compacted",
            JobKind::Collect => "collecting unused space",
            JobKind::Prepare => "waiting for the upgrade",
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
/// error: the record still needs forgetting. A folder is deleted only by
/// its own name: on a Mac's disk, which ignores letter case, `Gone` would
/// otherwise delete `gone`.
async fn delete_trees(root: &std::path::Path, groups: &[String]) -> Result<()> {
    let folders: Vec<String> = std::fs::read_dir(root)
        .with_context(|| format!("list {}", root.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if let Some((group, folder)) = groups.iter().find_map(|g| {
        folders
            .iter()
            .find(|f| f.eq_ignore_ascii_case(g) && *f != g)
            .map(|f| (g, f))
    }) {
        anyhow::bail!("not deleting {folder:?}: it was asked for as {group:?}");
    }
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
fn turned_off_of(graph: &Graph, all: &BTreeMap<String, String>) -> BTreeMap<usize, String> {
    all.iter()
        .filter_map(|(id, by)| Some((*graph.by_id.get(id)?, by.clone())))
        .collect()
}

/// Write what the tick made of each step into its record. A step between
/// passes (`held`: its invocation ended, and what it reads has not
/// settled) waits for the producer named there. `serving` names the open requests a step
/// has work left in, oldest first.
#[allow(clippy::too_many_arguments)]
fn record_states(
    graph: &Graph,
    shape: &Shape,
    state: &mut Record,
    t: &Tick,
    held: &[Option<usize>],
    turned_off: &BTreeMap<usize, String>,
    low: Option<LowDisk>,
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
            Row::Running if t.stops.contains(&i) => Some(match (&turned_off_by, low) {
                (Some(by), _) => format!("stopping: turned off by {by}"),
                (None, Some(low)) => format!("stopping: {}", low.describe()),
                (None, None) => "stopping: no open request wants it".to_string(),
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
            Row::Waiting(Wait::Prepare) => Some(
                "waiting for the upgrade: this build has not yet asked it, or a step it reads, \
                 to make what it stored usable"
                    .to_string(),
            ),
            Row::Waiting(Wait::DiskSpace) => Some(match low {
                Some(low) => format!("waiting for disk space: {}", low.describe()),
                None => "waiting for disk space".to_string(),
            }),
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
        entry.waiting_for = match st {
            Row::Waiting(w) => Some(w.into()),
            _ => None,
        };
        entry.turned_off_by = turned_off_by;
        entry.requests = serving(i);
    }
}

/// Whether the disk holds the steps back, as of the loop's last look:
/// the floor has two lines, so the answer depends on the one before. A
/// crossing is logged once each way rather than on every look.
#[derive(Default)]
struct DiskWatch {
    low: bool,
    unmeasured: bool,
}

impl DiskWatch {
    /// The disk holding the steps back, if it does. A disk that cannot be
    /// measured holds nothing back: that is logged once, at ERROR.
    fn look(&mut self, root: &std::path::Path, floor: Option<DiskFloor>) -> Option<LowDisk> {
        let low = match floor.map(|f| (f, disk_space::space(root))) {
            None => None,
            Some((floor, Ok(s))) => {
                self.unmeasured = false;
                floor.holds(self.low, s.available).then_some(LowDisk {
                    available: s.available,
                    floor,
                })
            }
            Some((_, Err(e))) => {
                if !std::mem::replace(&mut self.unmeasured, true) {
                    tracing::error!(
                        "supervisor: cannot measure the free space under {}, so the \
                         [disk_space] floor holds nothing back: {e}",
                        root.display()
                    );
                }
                None
            }
        };
        match (low, self.low) {
            (Some(low), false) => tracing::warn!(
                "supervisor: {}: no step starts, and running ones are stopped, until space \
                 is freed",
                low.describe()
            ),
            (None, true) => {
                tracing::info!(
                    "supervisor: the data root's disk is back over the [disk_space] resume \
                     line; steps run"
                )
            }
            _ => {}
        }
        self.low = low.is_some();
        low
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
        st.waiting_for = waiting.then_some(super::tick::WaitKind::Config);
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
                    s.waits_for_settled_inputs,
                    s.locks.clone(),
                )
            })
            .collect()
    };
    a.fingerprints == b.fingerprints
        && a.locks == b.locks
        && a.disk_floor == b.disk_floor
        && when(a) == when(b)
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
            source: spec.is_source(),
            fingerprint: graph.fingerprints[i].clone(),
            pins_reads: spec.reads_pinned,
            waits_for_settled_inputs: spec.waits_for_settled_inputs,
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

/// The record's facts, with what only the loop holds laid over them.
fn facts_of(graph: &Graph, state: &Record, slots: &Slots) -> Facts {
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
            last_attempt: slot_at(slots, graph, i)
                .and_then(|s| s.last_attempt.as_ref())
                .map(|a| Attempt {
                    started: a.started,
                    failed: a.failed,
                    stopped: a.stopped,
                    consumed: a.read.in_graph(graph),
                }),
            running: slot_at(slots, graph, i).and_then(|s| s.running.clone()),
            streams_output: slot_at(slots, graph, i)
                .and_then(|s| s.streams_output)
                .unwrap_or(graph.steps[i].streams_output),
            needs_rerun: recorded(i).is_some_and(|s| s.needs_rerun),
            preparing: false,
        })
        .collect();
    Facts {
        sinks,
        steps,
        low_disk: false,
    }
}

/// The steps that answered a launch's `--migrate` with `needs_rerun` and
/// have not succeeded since, turned-off ones aside: what the loop runs
/// after an upgrade's pass, and what the app waits on.
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
    use crate::supervisor::tick::{StateKind, WaitKind};
    use crate::EventSink;

    use datalib_sql::test_sql;
    /// A purge's last stop before `remove_dir_all`. A Mac's disk ignores
    /// letter case, so `Gone` there is the folder `gone`; it is never
    /// deleted under a name that is not its own.
    #[tokio::test]
    async fn a_purge_deletes_no_folder_by_another_case() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir(td.path().join("gone")).unwrap();
        let err = delete_trees(td.path(), &["Gone".to_string()])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"gone\""), "{err}");
        assert!(td.path().join("gone").is_dir(), "the folder was deleted");
        delete_trees(td.path(), &["gone".to_string(), "never".to_string()])
            .await
            .unwrap();
        assert!(!td.path().join("gone").exists());
    }

    /// The prepare pass runs several `--prepare` processes at once, and
    /// starts a step's only after every upstream step's has exited, even
    /// through a step that does not take the verb. It once ran one at a
    /// time, so a step waited for every `--prepare` ahead of it in graph
    /// order.
    #[test]
    fn the_prepare_pass_runs_a_few_at_once_and_each_after_its_upstream() {
        let step = |id: &str, input: Option<&str>, prepares: bool| {
            let mut spec = StepSpec::new(
                id,
                StepRun::in_process(|_| async { Ok(crate::step::StepOutcome::default()) }),
            );
            if let Some(i) = input {
                spec = spec.input(i);
            }
            if prepares {
                spec.verbs.insert(crate::step::Verb::Prepare);
            }
            spec
        };
        let mut steps = vec![
            step("a/ingest", None, true),
            step("x/mid", Some("a/ingest"), false),
            step("a/render", Some("x/mid"), true),
        ];
        let others = ["b/ingest", "c/ingest", "d/ingest", "e/ingest"];
        steps.extend(others.iter().map(|id| step(id, None, true)));
        let graph = Graph::build(steps).unwrap();
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut asked = super::super::upgrade::steps_to_ask(&graph);
        asked.sort();
        assert_eq!(
            asked,
            ids(&["a/ingest", "a/render", "b/ingest", "c/ingest", "d/ingest", "e/ingest"])
        );
        // One graph order of the six, fixed so the test does not depend on
        // which one `Graph::build` picks.
        let mut pass = PreparePass {
            job: "job".into(),
            order: Vec::new(),
            not_started: ids(&[
                "b/ingest", "c/ingest", "d/ingest", "e/ingest", "a/ingest", "a/render",
            ]),
            running: BTreeSet::new(),
            turned_off_before: BTreeMap::new(),
        };
        let start = |pass: &mut PreparePass| {
            let now = pass.startable(&graph);
            pass.not_started.retain(|id| !now.contains(id));
            pass.running.extend(now.iter().cloned());
            now
        };

        assert_eq!(
            start(&mut pass),
            ids(&["b/ingest", "c/ingest", "d/ingest", "e/ingest"]),
            "four at once"
        );
        assert_eq!(start(&mut pass), ids(&[]), "no room for a fifth");
        pass.running.remove("b/ingest");
        assert_eq!(
            start(&mut pass),
            ids(&["a/ingest"]),
            "one `--prepare` exited"
        );
        pass.running.clear();
        pass.running.insert("a/ingest".into());
        assert_eq!(
            start(&mut pass),
            ids(&[]),
            "a/render is downstream of a/ingest through x/mid, and a/ingest's `--prepare` is running"
        );
        pass.running.clear();
        assert_eq!(start(&mut pass), ids(&["a/render"]));
        pass.running.clear();
        assert!(pass.is_over());
    }

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

    /// The sync driver's `syncs_under_a_floor_of_zero` on 2026-11-27: the
    /// server saved a config that adds a source and opened a request for it
    /// at once, and the request reached the idle loop before the server's
    /// watch said the config moved. It was closed failed for naming a step
    /// the loop did not have; it waits for the file's config instead.
    #[tokio::test]
    async fn a_request_for_a_step_the_config_file_has_but_the_loop_has_not_heard_of_runs() {
        let t = Three::new();
        let graph = Graph::build(vec![t.step(0)]).unwrap();
        let config = Swappable::new(&graph);
        let (stop, rx) = watch::channel(false);
        let root = t.root.path().to_path_buf();
        let runner = Runner::new(&root)
            .sink(t.events.clone())
            .reload_from(config.clone())
            .stop_on(rx);
        let told = Arc::new(Told::default());
        let host = told.clone();
        let task = tokio::spawn(async move {
            let store = Store::open(&root).await?;
            let ran = runner.run_forever(&graph, &store, &*host).await;
            store.close().await;
            ran
        });
        let other = Store::open(t.root.path()).await.unwrap();
        // The loop is up and idle before the config moves.
        let warm = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        t.go.store(true, Ordering::SeqCst);
        while outcome(&other, &warm).await.is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        config.set(vec![t.step(0), t.step(1)]);
        let id = other.open_request(&["b/raw".into()], "ui").await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while outcome(&other, &id).await.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the request never closed"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(outcome(&other, &id).await, Some(Some(RequestOutcome::Done)));
        assert_eq!(t.runs(1), 1);

        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
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
        assert_eq!(c.waiting_for, Some(WaitKind::Config));
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

    /// A floor no disk can meet, set while a download runs: it is
    /// stopped, its row says it waits for disk space, and the request
    /// stays open. Lifting the floor runs it again and the request closes.
    #[tokio::test]
    async fn a_disk_under_the_floor_stops_the_running_step_until_it_is_lifted() {
        let t = Three::new();
        let graph = Graph::build(vec![t.step(0)]).unwrap();
        let config = Swappable::new(&graph);
        let other = Store::open(t.root.path()).await.unwrap();
        let id = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let running = t.serve(graph.clone(), config.clone());
        until("a to start", || t.runs(0) == 1).await;

        let mut floored = graph.clone();
        floored.disk_floor = Some(DiskFloor {
            pause_below: u64::MAX,
            resume_at: u64::MAX,
        });
        *config.0.lock().unwrap() = floored;
        announce_config(t.root.path());
        let a = until_recorded(t.root.path(), "a/raw", "waiting for disk", |st| {
            st.state == Some(StateKind::Waiting)
        })
        .await;
        assert_eq!(a.waiting_for, Some(WaitKind::DiskSpace), "{a:?}");
        assert!(
            a.state_detail
                .as_deref()
                .is_some_and(|d| d.starts_with("waiting for disk space: ")),
            "{a:?}"
        );
        assert!(
            t.events.logged("a/raw", "stopped: "),
            "the step's log says why"
        );
        assert_eq!(outcome(&other, &id).await, None, "the request waits");

        t.go.store(true, Ordering::SeqCst);
        *config.0.lock().unwrap() = graph;
        announce_config(t.root.path());
        running.await.unwrap().unwrap();
        assert_eq!(outcome(&other, &id).await, Some(Some(RequestOutcome::Done)));
        assert_eq!(t.runs(0), 2, "stopped once, then run to the end");
        let a = crate::supervisor::record::recorded(t.root.path()).await;
        assert_eq!(
            a.steps["a/raw"].waiting_for, None,
            "a step no longer waiting says it waits for nothing"
        );
    }

    /// A floor no disk can meet from the start: the step never starts,
    /// and its log says why. A one-shot `datalib-pipeline` prints only the
    /// log, so without the line a run on a small disk sat silent until
    /// killed (the v0.45.0 docker doc test).
    #[tokio::test]
    async fn a_step_the_floor_keeps_from_starting_says_so_on_its_log() {
        let t = Three::new();
        let mut graph = Graph::build(vec![t.step(0)]).unwrap();
        graph.disk_floor = Some(DiskFloor {
            pause_below: u64::MAX,
            resume_at: u64::MAX,
        });
        let config = Swappable::new(&graph);
        let other = Store::open(t.root.path()).await.unwrap();
        let id = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let running = t.serve(graph.clone(), config.clone());
        let a = until_recorded(t.root.path(), "a/raw", "waiting for disk", |st| {
            st.state == Some(StateKind::Waiting)
        })
        .await;
        assert_eq!(a.waiting_for, Some(WaitKind::DiskSpace), "{a:?}");
        assert!(
            t.events.logged("a/raw", "waiting for disk space: "),
            "the step's log says why"
        );
        assert_eq!(t.runs(0), 0);

        t.go.store(true, Ordering::SeqCst);
        graph.disk_floor = None;
        *config.0.lock().unwrap() = graph;
        announce_config(t.root.path());
        running.await.unwrap().unwrap();
        assert_eq!(outcome(&other, &id).await, Some(Some(RequestOutcome::Done)));
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

    /// What a host's loop told its host, in order.
    #[derive(Default)]
    struct Told(std::sync::Mutex<Vec<String>>);

    impl LoopHost for Told {
        fn open(
            &self,
            start: datalib_runs::RunStart,
        ) -> std::result::Result<BTreeMap<String, String>, String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("open: {}", start.reason));
            Ok(BTreeMap::new())
        }
        fn close(&self, _run_id: &str) {
            self.0.lock().unwrap().push("close".into());
        }
    }

    impl Told {
        fn said(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// A host's loop, on its own task, stopped by the sender it returns.
    fn forever(
        f: &Fixture,
        told: Arc<Told>,
    ) -> (watch::Sender<bool>, tokio::task::JoinHandle<Result<()>>) {
        let (stop, rx) = watch::channel(false);
        let runner = Runner::new(f.root.path()).stop_on(rx);
        let graph = f.graph.clone();
        let root = f.root.path().to_path_buf();
        let task = tokio::spawn(async move {
            let store = Store::open(&root).await?;
            let ran = runner.run_forever(&graph, &store, &*told).await;
            store.close().await;
            ran
        });
        (stop, task)
    }

    async fn state_of(root: &std::path::Path, step: &str) -> StepRecord {
        let recorded = crate::supervisor::record::recorded(root).await;
        recorded.steps.get(step).cloned().unwrap_or_default()
    }

    /// The loop does not end when nothing is open. A turn-off or turn-on
    /// while it is idle reaches the record with no request and no period;
    /// a request opens a period, which closes once the request is done.
    /// Before the loop never exited, the idle side needed `settle`, a
    /// second way to run a tick, to do the first of these.
    #[tokio::test]
    async fn the_loop_keeps_the_record_while_idle_and_opens_a_period_per_stretch_of_work() {
        let f = fixture();
        let other = Store::open(f.root.path()).await.unwrap();
        let told = Arc::new(Told::default());
        let (stop, task) = forever(&f, told.clone());

        other.turn_off("a/raw", "claude").await.unwrap();
        let root = f.root.path();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while state_of(root, "a/raw").await.state != Some(StateKind::Off) {
            assert!(
                std::time::Instant::now() < deadline,
                "the turn-off never reached the record"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let a = state_of(root, "a/raw").await;
        assert_eq!(a.turned_off_by.as_deref(), Some("claude"));
        assert_eq!(a.state_detail.as_deref(), Some("turned off by claude"));
        other.turn_on("a/raw").await.unwrap();
        while state_of(root, "a/raw").await.state != Some(StateKind::Stale) {
            assert!(
                std::time::Instant::now() < deadline,
                "the turn-on never reached the record"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            told.said().is_empty(),
            "a switch opened a period: {:?}",
            told.said()
        );

        f.go.store(true, Ordering::SeqCst);
        let id = other.open_request(&["b/raw".into()], "ui").await.unwrap();
        while other.request(&id).await.unwrap().unwrap().closed.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the request never closed"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        until("the period to close", || told.said().len() == 2).await;
        assert_eq!(
            told.said(),
            ["open: a sync asked for by ui: b/raw", "close"],
            "one period for the one request"
        );
        assert!(!task.is_finished(), "the loop ended with nothing open");

        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(f.runs[1].load(Ordering::SeqCst), 1);
    }

    /// A request asked to stop before the loop took it on is closed where
    /// it stands, and opens no period.
    #[tokio::test]
    async fn a_request_stopped_before_the_loop_took_it_opens_no_period() {
        let f = fixture();
        let other = Store::open(f.root.path()).await.unwrap();
        let id = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        other.request_stop(&id, "ui").await.unwrap();
        let told = Arc::new(Told::default());
        let (stop, task) = forever(&f, told.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while other.request(&id).await.unwrap().unwrap().closed.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the request never closed"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            other.request(&id).await.unwrap().unwrap().closed,
            Some(Some(RequestOutcome::Stopped))
        );
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
        assert!(told.said().is_empty(), "{:?}", told.said());
        assert_eq!(f.runs[0].load(Ordering::SeqCst), 0);
    }

    /// A host that cannot open a period fails the requests that wanted
    /// one, and the loop goes on.
    #[tokio::test]
    async fn a_period_that_cannot_open_fails_its_requests() {
        struct Broken;
        impl LoopHost for Broken {
            fn open(
                &self,
                _: datalib_runs::RunStart,
            ) -> std::result::Result<BTreeMap<String, String>, String> {
                Err("no config".into())
            }
            fn close(&self, _: &str) {
                unreachable!("no period opened")
            }
        }
        let f = fixture();
        let other = Store::open(f.root.path()).await.unwrap();
        let (stop, rx) = watch::channel(false);
        let runner = Runner::new(f.root.path()).stop_on(rx);
        let graph = f.graph.clone();
        let root = f.root.path().to_path_buf();
        let task = tokio::spawn(async move {
            let store = Store::open(&root).await.unwrap();
            runner.run_forever(&graph, &store, &Broken).await
        });
        let id = other.open_request(&["a/raw".into()], "ui").await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while other.request(&id).await.unwrap().unwrap().closed.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the request never closed"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            other.request(&id).await.unwrap().unwrap().closed,
            Some(Some(RequestOutcome::Failed))
        );
        assert!(!task.is_finished());
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
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
        let outcome: String = test_sql!("SELECT outcome FROM invocations WHERE id = ?")
            .query_scalar()
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

    /// A round pinned with `--now` ends on that clock. The demo's bake,
    /// pinned to 2369, recorded its run as ending in the present, three
    /// centuries before it began.
    #[tokio::test]
    async fn a_pinned_round_ends_after_it_began() {
        let f = fixture();
        f.go.store(true, Ordering::SeqCst);
        let store = Store::open(f.root.path()).await.unwrap();
        store.open_request(&["a/raw".into()], "ui").await.unwrap();
        let pinned = "2369-04-15T00:00:00+00:00";
        let env = BTreeMap::from([(
            datalib_step_protocol::env::DATALIB_DAG_NOW.to_string(),
            pinned.to_string(),
        )]);
        Runner::new(f.root.path())
            .child_env(env)
            .serve(&f.graph, &store)
            .await
            .unwrap();

        let run = store.load_record().await.unwrap().current_run.unwrap();
        store.close().await;
        assert_eq!(run.started_at, pinned);
        let finished = run.finished_at.expect("the round closed its run");
        assert!(
            finished.as_str() >= pinned && finished.starts_with("2369-04-15T00:0"),
            "began {pinned}, ended {finished}"
        );
    }
}
