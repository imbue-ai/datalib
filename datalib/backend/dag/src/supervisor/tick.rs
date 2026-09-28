//! The reconcile tick: from the graph, the open requests and the facts,
//! each step's state and the starts, stops and request closures to make.
//! Pure — no clock, no I/O; the host turns events into [`Facts`] and acts
//! on the [`Tick`]. The rules are `docs/dev/plans/supervisor.md` §2.2–§2.4.

use std::collections::{BTreeMap, BTreeSet};

use strum::{EnumString, IntoStaticStr, VariantArray};

use super::locks::Hold;

pub type StepIx = usize;
pub type SinkIx = usize;

/// A point in the supervisor's own order of events, handed out by the
/// host. Never a wall-clock stamp: "started after this request opened"
/// has to be exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seq(pub u64);

pub type LockIx = usize;

/// A named lock as the tick needs it (`locks.rs`).
#[derive(Debug, Clone)]
pub struct LockShape {
    pub name: String,
    pub slots: usize,
}

/// The graph as the tick needs it: who reads which sink. Each step writes
/// one, the one its own index names.
#[derive(Debug, Clone)]
pub struct Shape {
    pub steps: Vec<StepShape>,
    pub locks: Vec<LockShape>,
    /// Every step, producers before the steps that read them.
    pub topo: Vec<StepIx>,
}

#[derive(Debug, Clone)]
pub struct StepShape {
    pub reads: Vec<SinkIx>,
    /// The hash of the step's own definition; a change makes it stale.
    pub fingerprint: String,
    /// The named locks it holds while it runs.
    pub locks: Vec<(LockIx, Hold)>,
    /// False for a step that reads its inputs off disk rather than at a
    /// pinned commit: it and a writer of what it reads never overlap.
    pub pins_reads: bool,
}

/// What people have asked for: the open requests and the steps turned off.
#[derive(Debug, Clone, Default)]
pub struct Intent {
    pub requests: Vec<Request>,
    pub turned_off: BTreeSet<StepIx>,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub roots: Vec<StepIx>,
    pub opened: Seq,
}

/// What has happened, as the host last heard it.
#[derive(Debug, Clone)]
pub struct Facts {
    /// Each sink's published version; `None` if nothing was ever published.
    pub sinks: Vec<Option<String>>,
    pub steps: Vec<StepFacts>,
}

#[derive(Debug, Clone, Default)]
pub struct StepFacts {
    pub last_success: Option<Consumed>,
    /// The latest invocation to finish, whatever its outcome.
    pub last_attempt: Option<Attempt>,
    pub running: Option<Running>,
    /// Whether the step said its consumers may read its sink before it
    /// finishes.
    pub streams_output: bool,
}

/// What an invocation was started against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consumed {
    pub fingerprint: String,
    pub reads: BTreeMap<SinkIx, Option<String>>,
}

#[derive(Debug, Clone)]
pub struct Attempt {
    pub started: Seq,
    pub failed: bool,
    /// It was stopped — turned off, a stop, the host going — rather than
    /// ending on its own. That is neither a failure nor a run: the work is
    /// not done, and it runs again for a request that still wants it.
    pub stopped: bool,
    pub consumed: Consumed,
}

#[derive(Debug, Clone)]
pub struct Running {
    pub started: Seq,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    /// One per step, indexed like [`Shape::steps`].
    pub states: Vec<StepState>,
    pub starts: Vec<Start>,
    /// Running steps nobody wants any more, or that are turned off.
    pub stops: Vec<StepIx>,
    /// Requests that close now, by index into [`Intent::requests`].
    pub closed: Vec<(usize, Outcome)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub step: StepIx,
    pub consumed: Consumed,
}

/// `StateKind` is the word the record stores for a state
/// (`steps.state`), without what it waits on or is blocked by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumDiscriminants)]
#[strum_discriminants(
    name(StateKind),
    derive(EnumString, IntoStaticStr, VariantArray),
    strum(serialize_all = "snake_case")
)]
pub enum StepState {
    /// No open request wants it, and it is up to date.
    Idle,
    /// No open request wants it, and it is out of date.
    Stale,
    /// Wanted, and up to date.
    Fresh,
    Running,
    Off,
    /// Its retries ran out and nothing it reads has moved since.
    Failed,
    /// Out of date, but a producer it reads has never published
    /// anything and is not going to run, so there is nothing to read.
    Blocked(StepIx),
    Waiting(Wait),
}

impl StateKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// A producer it reads is running or about to.
    Upstream(StepIx),
    /// A step that reads its sink unpinned is running, and would read a
    /// write in progress.
    Reader(StepIx),
    /// A named lock it holds is held by others, as far as it can be.
    Lock(LockIx),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The first step, in topological order, that failed for it or was
    /// blocked.
    Failed {
        step: StepIx,
    },
}

pub fn tick(shape: &Shape, intent: &Intent, facts: &Facts) -> Tick {
    let n = shape.steps.len();
    let mut readers: Vec<Vec<StepIx>> = vec![Vec::new(); n];
    for (i, s) in shape.steps.iter().enumerate() {
        for &r in &s.reads {
            readers[r].push(i);
        }
    }

    let scopes: Vec<Vec<bool>> = intent
        .requests
        .iter()
        .map(|r| closure(shape, &readers, &r.roots))
        .collect();
    let wanting: Vec<Vec<usize>> = (0..n)
        .map(|i| (0..scopes.len()).filter(|&r| scopes[r][i]).collect())
        .collect();

    let mut held = vec![Held::default(); shape.locks.len()];
    for (i, f) in facts.steps.iter().enumerate() {
        if f.running.is_some() {
            for &(l, hold) in &shape.steps[i].locks {
                held[l].take(hold);
            }
        }
    }

    let mut states = vec![StepState::Idle; n];
    let mut busy: Vec<bool> = facts.steps.iter().map(|f| f.running.is_some()).collect();
    // Running, or due to run: what a consumer must not race.
    let mut pending = busy.clone();
    let mut failed_for: Vec<Vec<bool>> = vec![vec![false; intent.requests.len()]; n];
    let mut starts = Vec::new();
    let mut stops = Vec::new();

    for &i in &shape.topo {
        let step = &shape.steps[i];
        let f = &facts.steps[i];
        let now = consumed_now(step, facts);

        if f.running.is_some() {
            states[i] = StepState::Running;
            if intent.turned_off.contains(&i) || wanting[i].is_empty() {
                stops.push(i);
            }
            continue;
        }
        if intent.turned_off.contains(&i) {
            states[i] = StepState::Off;
            continue;
        }
        let stale = stale_by_inputs(f, &now);
        if wanting[i].is_empty() {
            states[i] = match (&f.last_attempt, stale) {
                (Some(a), _) if a.failed => StepState::Failed,
                (_, true) => StepState::Stale,
                (_, false) => StepState::Idle,
            };
            continue;
        }

        for &r in &wanting[i] {
            failed_for[i][r] = f.last_attempt.as_ref().is_some_and(|a| {
                a.failed
                    && !a.stopped
                    && a.started >= intent.requests[r].opened
                    && a.consumed == now
            });
        }
        let live: Vec<usize> = wanting[i]
            .iter()
            .copied()
            .filter(|&r| !failed_for[i][r])
            .collect();
        if live.is_empty() {
            states[i] = StepState::Failed;
            continue;
        }
        let due = stale
            || live
                .iter()
                .any(|&r| step.reads.is_empty() && !started_since(f, intent.requests[r].opened));
        if !due {
            states[i] = StepState::Fresh;
            continue;
        }

        if let Some(w) = blocking_producer(i, shape, facts, &states) {
            pending[i] = true;
            states[i] = StepState::Waiting(Wait::Upstream(w));
            continue;
        }
        if let Some(w) = nothing_to_read(step, facts) {
            if pending[w] {
                pending[i] = true;
                states[i] = StepState::Waiting(Wait::Upstream(w));
            } else {
                states[i] = StepState::Blocked(w);
            }
            continue;
        }
        pending[i] = true;
        if let Some(&r) = readers[i]
            .iter()
            .find(|&&r| busy[r] && !shape.steps[r].pins_reads)
        {
            states[i] = StepState::Waiting(Wait::Reader(r));
            continue;
        }
        let taken = step
            .locks
            .iter()
            .find(|&&(l, hold)| !held[l].allows(hold, shape.locks[l].slots));
        if let Some(&(l, _)) = taken {
            states[i] = StepState::Waiting(Wait::Lock(l));
            continue;
        }
        for &(l, hold) in &step.locks {
            held[l].take(hold);
        }
        busy[i] = true;
        states[i] = StepState::Running;
        starts.push(Start {
            step: i,
            consumed: now,
        });
    }

    let closed = (0..intent.requests.len())
        .filter(|&r| {
            (0..n).all(|i| {
                !scopes[r][i] || !matches!(states[i], StepState::Running | StepState::Waiting(_))
            })
        })
        .map(|r| {
            let failed = shape.topo.iter().copied().find(|&i| {
                failed_for[i][r] || (scopes[r][i] && matches!(states[i], StepState::Blocked(_)))
            });
            let outcome = match failed {
                Some(step) => Outcome::Failed { step },
                None => Outcome::Done,
            };
            (r, outcome)
        })
        .collect();

    Tick {
        states,
        starts,
        stops,
        closed,
    }
}

/// How much of one named lock the running steps hold.
#[derive(Debug, Clone, Copy, Default)]
struct Held {
    shared: usize,
    exclusive: bool,
}

impl Held {
    fn allows(&self, hold: Hold, slots: usize) -> bool {
        match hold {
            Hold::Shared => !self.exclusive && self.shared < slots,
            Hold::Exclusive => !self.exclusive && self.shared == 0,
        }
    }

    fn take(&mut self, hold: Hold) {
        match hold {
            Hold::Shared => self.shared += 1,
            Hold::Exclusive => self.exclusive = true,
        }
    }
}

/// The roots and everything that reads, transitively, what they write.
fn closure(shape: &Shape, readers: &[Vec<StepIx>], roots: &[StepIx]) -> Vec<bool> {
    let mut seen = vec![false; shape.steps.len()];
    let mut stack: Vec<StepIx> = roots.to_vec();
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        stack.extend(readers[i].iter().copied());
    }
    seen
}

fn consumed_now(step: &StepShape, facts: &Facts) -> Consumed {
    Consumed {
        fingerprint: step.fingerprint.clone(),
        reads: step
            .reads
            .iter()
            .map(|&s| (s, facts.sinks[s].clone()))
            .collect(),
    }
}

/// Never succeeded, its definition changed, or something it reads moved
/// since its last success. For a step with no reads this is only the
/// first two; whether a source is due is a question about requests.
fn stale_by_inputs(f: &StepFacts, now: &Consumed) -> bool {
    f.last_success.as_ref() != Some(now)
}

/// Running, or ran to an end of its own, since `opened`. A stopped run
/// did not.
fn started_since(f: &StepFacts, opened: Seq) -> bool {
    let running = f.running.as_ref().is_some_and(|r| r.started >= opened);
    let attempted = f
        .last_attempt
        .as_ref()
        .is_some_and(|a| !a.stopped && a.started >= opened);
    running || attempted
}

/// A producer of what the step reads, when nothing it reads has ever been
/// published. A fan-in reads whichever of its sources exist.
fn nothing_to_read(step: &StepShape, facts: &Facts) -> Option<StepIx> {
    if step.reads.iter().any(|&s| facts.sinks[s].is_some()) {
        return None;
    }
    step.reads.first().copied()
}

/// A producer of something `i` reads that `i` must wait for. Two reasons,
/// and only two: it is running and does not stream, so its sink may be
/// half-written; or it is about to run, held only by a lock or a reader,
/// and will rewrite what `i` would read. A producer that is
/// itself waiting on something upstream may not run for a long time, and a
/// fan-in that waited on it would wait for its slowest source. A step that
/// reads unpinned treats even a streaming producer as half-written.
fn blocking_producer(
    i: StepIx,
    shape: &Shape,
    facts: &Facts,
    states: &[StepState],
) -> Option<StepIx> {
    shape.steps[i].reads.iter().copied().find(|&w| {
        w != i
            && match states[w] {
                StepState::Running => !facts.steps[w].streams_output || !shape.steps[i].pins_reads,
                StepState::Waiting(Wait::Lock(_) | Wait::Reader(_)) => true,
                _ => false,
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default locks, as the loader declares them, with room for one
    /// index at a time.
    const NETWORK: LockIx = 0;
    const CPU: LockIx = 1;
    const INDEX: LockIx = 2;

    fn default_locks() -> Vec<LockShape> {
        [("network", 4), ("cpu", 4), ("index", 1)]
            .into_iter()
            .map(|(name, slots)| LockShape {
                name: name.into(),
                slots,
            })
            .collect()
    }

    /// `reads` are step indices, which are also sink indices.
    fn shape(reads: &[&[StepIx]]) -> Shape {
        let steps: Vec<StepShape> = reads
            .iter()
            .enumerate()
            .map(|(i, r)| StepShape {
                reads: r.to_vec(),
                fingerprint: format!("fp{i}"),
                locks: vec![(if r.is_empty() { NETWORK } else { CPU }, Hold::Shared)],
                pins_reads: true,
            })
            .collect();
        Shape {
            topo: (0..steps.len()).collect(),
            steps,
            locks: default_locks(),
        }
    }

    /// source 0 → render 1 → index 2.
    fn chain() -> Shape {
        let mut s = shape(&[&[], &[0], &[1]]);
        s.steps[2].locks = vec![(INDEX, Hold::Shared)];
        s
    }

    /// Every step succeeded against the sinks as they are now.
    fn all_fresh(shape: &Shape, at: u64) -> Facts {
        let sinks: Vec<Option<String>> = (0..shape.steps.len())
            .map(|s| Some(format!("v{s}")))
            .collect();
        let steps = shape
            .steps
            .iter()
            .map(|st| {
                let consumed = Consumed {
                    fingerprint: st.fingerprint.clone(),
                    reads: st.reads.iter().map(|&r| (r, sinks[r].clone())).collect(),
                };
                StepFacts {
                    last_success: Some(consumed.clone()),
                    last_attempt: Some(Attempt {
                        started: Seq(at),
                        failed: false,
                        stopped: false,
                        consumed,
                    }),
                    running: None,
                    streams_output: false,
                }
            })
            .collect();
        Facts { sinks, steps }
    }

    fn request(roots: &[StepIx], opened: u64) -> Intent {
        Intent {
            requests: vec![Request {
                roots: roots.to_vec(),
                opened: Seq(opened),
            }],
            turned_off: BTreeSet::new(),
        }
    }

    fn started(t: &Tick) -> Vec<StepIx> {
        t.starts.iter().map(|s| s.step).collect()
    }

    fn run(facts: &mut Facts, step: StepIx, at: u64) {
        facts.steps[step].running = Some(Running { started: Seq(at) });
    }

    /// The step finishes: it succeeds against `consumed` and publishes
    /// `version`, or fails having published nothing.
    fn finish(facts: &mut Facts, step: StepIx, consumed: Consumed, outcome: Result<&str, ()>) {
        let r = facts.steps[step].running.take().expect("was running");
        if let Ok(version) = outcome {
            facts.sinks[step] = Some(version.to_string());
            facts.steps[step].last_success = Some(consumed.clone());
        }
        facts.steps[step].last_attempt = Some(Attempt {
            started: r.started,
            failed: outcome.is_err(),
            stopped: false,
            consumed,
        });
    }

    fn start_of(t: &Tick, step: StepIx) -> Consumed {
        t.starts
            .iter()
            .find(|s| s.step == step)
            .unwrap_or_else(|| panic!("step {step} was not started: {t:?}"))
            .consumed
            .clone()
    }

    #[test]
    fn a_graph_all_fresh_under_a_derived_root_starts_nothing() {
        let s = chain();
        let facts = all_fresh(&s, 1);
        let t = tick(&s, &request(&[1], 5), &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.closed, vec![(0, Outcome::Done)]);
    }

    #[test]
    fn stale_steps_with_no_open_request_start_nothing() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        for f in &mut facts.steps {
            f.last_success = None;
        }
        let t = tick(&s, &Intent::default(), &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states, vec![StepState::Stale; 3]);
    }

    #[test]
    fn a_request_starts_its_source_and_holds_the_rest_until_it_lands() {
        let s = chain();
        let facts = all_fresh(&s, 1);
        let t = tick(&s, &request(&[0], 5), &facts);
        assert_eq!(started(&t), vec![0]);
        assert_eq!(t.states[1], StepState::Fresh);
        assert!(t.closed.is_empty());
    }

    #[test]
    fn a_whole_chain_runs_one_hop_at_a_time_and_then_closes() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let intent = request(&[0], 5);

        let t = tick(&s, &intent, &facts);
        let c0 = start_of(&t, 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Ok("v0'"));

        let t = tick(&s, &intent, &facts);
        assert_eq!(started(&t), vec![1]);
        assert_eq!(
            t.states[2],
            StepState::Fresh,
            "up to date with what 1 has published"
        );
        assert!(t.closed.is_empty(), "1 is running");
        let c1 = start_of(&t, 1);
        assert_eq!(c1.reads[&0].as_deref(), Some("v0'"));
        run(&mut facts, 1, 7);
        finish(&mut facts, 1, c1, Ok("v1'"));

        let t = tick(&s, &intent, &facts);
        assert_eq!(started(&t), vec![2]);
        let c2 = start_of(&t, 2);
        run(&mut facts, 2, 8);
        finish(&mut facts, 2, c2, Ok("v2'"));

        let t = tick(&s, &intent, &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.closed, vec![(0, Outcome::Done)]);
    }

    #[test]
    fn a_render_that_moved_nothing_leaves_the_index_alone() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let intent = request(&[0], 5);
        let c0 = start_of(&tick(&s, &intent, &facts), 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Ok("v0'"));
        let c1 = start_of(&tick(&s, &intent, &facts), 1);
        run(&mut facts, 1, 7);
        finish(&mut facts, 1, c1, Ok("v1"));

        let t = tick(&s, &intent, &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[2], StepState::Fresh);
        assert_eq!(t.closed, vec![(0, Outcome::Done)]);
    }

    #[test]
    fn a_source_running_since_before_the_request_runs_once_more() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 0, 3);
        let intent = request(&[0], 5);

        let t = tick(&s, &intent, &facts);
        assert!(t.starts.is_empty());
        assert_eq!(t.states[0], StepState::Running);
        assert!(t.stops.is_empty(), "a wanted step is not stopped");

        let c0 = facts.steps[0].last_success.clone().unwrap();
        finish(&mut facts, 0, c0, Ok("v0"));
        let t = tick(&s, &intent, &facts);
        assert_eq!(started(&t), vec![0]);
    }

    #[test]
    fn a_source_that_ran_after_the_request_opened_is_fresh_for_it() {
        let s = chain();
        let facts = all_fresh(&s, 6);
        let t = tick(&s, &request(&[0], 5), &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.closed, vec![(0, Outcome::Done)]);
    }

    #[test]
    fn a_consumer_does_not_start_while_a_producer_that_does_not_stream_runs() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        facts.steps[1].last_success = None;
        run(&mut facts, 0, 6);
        facts.sinks[0] = Some("v0-sealed".into());

        let t = tick(&s, &request(&[0], 5), &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[1], StepState::Waiting(Wait::Upstream(0)));
    }

    #[test]
    fn a_streaming_producer_lets_its_consumer_start_once_it_has_published() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        facts.steps[0].streams_output = true;
        run(&mut facts, 0, 6);
        let intent = request(&[0], 5);

        let t = tick(&s, &intent, &facts);
        assert_eq!(
            t.states[1],
            StepState::Fresh,
            "nothing published yet, so nothing to read"
        );

        facts.sinks[0] = Some("v0-sealed".into());
        let t = tick(&s, &intent, &facts);
        assert_eq!(started(&t), vec![1]);
        assert_eq!(start_of(&t, 1).reads[&0].as_deref(), Some("v0-sealed"));
    }

    #[test]
    fn a_seal_during_a_pass_owes_exactly_one_more() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        facts.steps[0].streams_output = true;
        run(&mut facts, 0, 6);
        let intent = request(&[0], 5);

        facts.sinks[0] = Some("seal-1".into());
        let c1 = start_of(&tick(&s, &intent, &facts), 1);
        run(&mut facts, 1, 7);
        facts.sinks[0] = Some("seal-2".into());
        facts.sinks[0] = Some("seal-3".into());
        let t = tick(&s, &intent, &facts);
        assert!(t.starts.is_empty(), "one instance at a time: {t:?}");

        finish(&mut facts, 1, c1, Ok("v1-a"));
        let t = tick(&s, &intent, &facts);
        assert_eq!(started(&t), vec![1]);
        let c1 = start_of(&t, 1);
        assert_eq!(c1.reads[&0].as_deref(), Some("seal-3"));
        run(&mut facts, 1, 8);
        finish(&mut facts, 1, c1, Ok("v1-b"));

        let t = tick(&s, &intent, &facts);
        assert!(!started(&t).contains(&1), "{t:?}");
    }

    #[test]
    fn an_exhausted_failure_closes_the_request_failed_and_is_not_restarted() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let intent = request(&[0], 5);
        let c0 = start_of(&tick(&s, &intent, &facts), 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Err(()));

        let t = tick(&s, &intent, &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[0], StepState::Failed);
        assert_eq!(t.closed, vec![(0, Outcome::Failed { step: 0 })]);
    }

    #[test]
    fn a_new_request_retries_a_failed_step() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let c0 = start_of(&tick(&s, &request(&[0], 5), &facts), 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Err(()));

        let t = tick(&s, &request(&[0], 9), &facts);
        assert_eq!(started(&t), vec![0]);
    }

    #[test]
    fn a_failed_consumer_tries_again_when_its_producer_moves() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        facts.steps[0].streams_output = true;
        run(&mut facts, 0, 6);
        let intent = request(&[0], 5);
        facts.sinks[0] = Some("seal-1".into());
        let c1 = start_of(&tick(&s, &intent, &facts), 1);
        run(&mut facts, 1, 7);
        finish(&mut facts, 1, c1, Err(()));

        let t = tick(&s, &intent, &facts);
        assert_eq!(t.states[1], StepState::Failed);
        assert!(t.starts.is_empty());

        facts.sinks[0] = Some("seal-2".into());
        let t = tick(&s, &intent, &facts);
        assert_eq!(started(&t), vec![1]);
    }

    #[test]
    fn a_consumer_reads_what_a_failed_producer_committed() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let intent = request(&[0], 5);
        let c0 = start_of(&tick(&s, &intent, &facts), 0);
        run(&mut facts, 0, 6);
        facts.sinks[0] = Some("partial".into());
        finish(&mut facts, 0, c0, Err(()));

        let t = tick(&s, &intent, &facts);
        assert_eq!(started(&t), vec![1]);
        assert_eq!(start_of(&t, 1).reads[&0].as_deref(), Some("partial"));
    }

    /// Two streaming sources into one index, on a first sync. `a` has
    /// sealed and its render has run; `b` has sealed nothing, so its render
    /// waits. The index must not wait with it: `a`'s rows reach the grid
    /// while `b` is still downloading.
    #[test]
    fn a_fan_in_does_not_wait_for_a_render_still_waiting_on_its_download() {
        //  a 0 → render 1 ┐
        //                  ├→ index 4
        //  b 2 → render 3 ┘
        let mut s = shape(&[&[], &[0], &[], &[2], &[1, 3]]);
        s.steps[4].locks = vec![(INDEX, Hold::Shared)];
        let mut facts = all_fresh(&s, 1);
        for i in [0, 2] {
            facts.steps[i].streams_output = true;
            run(&mut facts, i, 6);
        }
        for i in [2, 3, 4] {
            facts.steps[i].last_success = None;
        }
        facts.sinks[2] = None;
        facts.sinks[3] = None;

        let t = tick(&s, &request(&[0, 2], 5), &facts);
        assert_eq!(t.states[3], StepState::Waiting(Wait::Upstream(2)));
        assert!(started(&t).contains(&4), "{t:?}");
    }

    /// A render that is about to start will rewrite what the index would
    /// read, so the index lets it go first rather than running twice.
    #[test]
    fn a_consumer_waits_for_a_producer_held_only_by_a_lock() {
        let mut s = chain();
        s.steps[1].locks = vec![(CPU, Hold::Shared)];
        let mut facts = all_fresh(&s, 1);
        facts.sinks[0] = Some("v0-new".into());
        facts.steps[2].last_success = None;
        s.locks[CPU].slots = 0;
        let t = tick(&s, &request(&[1], 5), &facts);
        assert_eq!(t.states[1], StepState::Waiting(Wait::Lock(CPU)));
        assert_eq!(t.states[2], StepState::Waiting(Wait::Upstream(1)));
    }

    /// The qmd index globs a render's `.md` files off disk, so it must not
    /// read while the render writes — streaming or not — and the render
    /// must not write while it reads.
    #[test]
    fn an_unpinned_reader_waits_for_a_streaming_producer_to_finish() {
        let mut s = chain();
        s.steps[2].pins_reads = false;
        let mut facts = all_fresh(&s, 1);
        facts.steps[1].streams_output = true;
        run(&mut facts, 1, 6);
        facts.sinks[1] = Some("v1-sealed".into());

        let t = tick(&s, &request(&[1], 5), &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[2], StepState::Waiting(Wait::Upstream(1)));

        s.steps[2].pins_reads = true;
        let t = tick(&s, &request(&[1], 5), &facts);
        assert_eq!(started(&t), vec![2], "a pinned reader reads each seal");
    }

    #[test]
    fn a_writer_waits_while_an_unpinned_reader_of_its_sink_runs() {
        let mut s = chain();
        s.steps[2].pins_reads = false;
        let mut facts = all_fresh(&s, 1);
        facts.sinks[0] = Some("v0-new".into());
        run(&mut facts, 2, 6);

        let t = tick(&s, &request(&[1], 5), &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[1], StepState::Waiting(Wait::Reader(2)));

        s.steps[2].pins_reads = true;
        let t = tick(&s, &request(&[1], 5), &facts);
        assert_eq!(started(&t), vec![1], "a pinned reader holds nobody back");
    }

    /// A first sync whose download fails has written nothing. Its render
    /// has nothing to read, so it waits for a download that works instead
    /// of running against a store that is not there.
    #[test]
    fn a_consumer_of_a_producer_that_never_published_is_blocked() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        facts.sinks[0] = None;
        facts.steps[0].last_success = None;
        let intent = request(&[0], 5);
        let c0 = start_of(&tick(&s, &intent, &facts), 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Err(()));

        let t = tick(&s, &intent, &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[1], StepState::Blocked(0));
        assert_eq!(t.closed, vec![(0, Outcome::Failed { step: 0 })]);
    }

    #[test]
    fn a_blocked_step_fails_a_request_that_reaches_it_but_not_its_producer() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        facts.sinks[0] = None;
        let t = tick(&s, &request(&[1], 5), &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.closed, vec![(0, Outcome::Failed { step: 1 })]);
    }

    #[test]
    fn a_turned_off_step_never_starts_and_does_not_hold_its_request_open() {
        let s = chain();
        let facts = all_fresh(&s, 1);
        let mut intent = request(&[0], 5);
        intent.turned_off.insert(0);

        let t = tick(&s, &intent, &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[0], StepState::Off);
        assert_eq!(t.closed, vec![(0, Outcome::Done)]);
    }

    /// A source stopped by turning it off, and turned on while its request is
    /// still open, runs again: a stopped run is neither a failure nor a
    /// run. Before, it counted as both, and the request closed failed.
    #[test]
    fn a_step_stopped_by_a_turn_off_runs_again_on_turn_on() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let intent = request(&[0], 5);
        let c0 = start_of(&tick(&s, &intent, &facts), 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Err(()));
        facts.steps[0].last_attempt.as_mut().unwrap().stopped = true;

        let t = tick(&s, &intent, &facts);
        assert_eq!(started(&t), vec![0], "{t:?}");
        assert!(t.closed.is_empty());
    }

    #[test]
    fn turning_off_a_running_step_stops_it() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 0, 6);
        let mut intent = request(&[0], 5);
        intent.turned_off.insert(0);
        assert_eq!(tick(&s, &intent, &facts).stops, vec![0]);
    }

    #[test]
    fn a_running_step_no_open_request_wants_is_stopped() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 0, 6);
        let t = tick(&s, &Intent::default(), &facts);
        assert_eq!(t.stops, vec![0]);
        assert_eq!(t.states[0], StepState::Running);
    }

    #[test]
    fn stopping_one_request_leaves_the_fan_in_to_the_other() {
        // gmail 0 → render 1 ┐
        //                     ├→ index 4
        // slack 2 → render 3 ┘
        let mut s = shape(&[&[], &[0], &[], &[2], &[1, 3]]);
        s.steps[4].locks = vec![(INDEX, Hold::Shared)];
        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 4, 6);
        let both = Intent {
            requests: vec![
                Request {
                    roots: vec![0],
                    opened: Seq(5),
                },
                Request {
                    roots: vec![2],
                    opened: Seq(5),
                },
            ],
            turned_off: BTreeSet::new(),
        };
        assert!(tick(&s, &both, &facts).stops.is_empty());

        let slack_only = Intent {
            requests: vec![both.requests[1].clone()],
            turned_off: BTreeSet::new(),
        };
        let t = tick(&s, &slack_only, &facts);
        assert!(t.stops.is_empty(), "{t:?}");
        assert!(!started(&t).contains(&0), "gmail is nobody's any more");
        assert_eq!(t.states[0], StepState::Idle);
    }

    #[test]
    fn a_lock_holds_the_steps_past_its_slots() {
        let mut s = shape(&[&[], &[], &[]]);
        s.locks[NETWORK].slots = 2;
        let facts = all_fresh(&s, 1);
        let t = tick(&s, &request(&[0, 1, 2], 5), &facts);
        assert_eq!(started(&t), vec![0, 1]);
        assert_eq!(t.states[2], StepState::Waiting(Wait::Lock(NETWORK)));
        assert!(t.closed.is_empty());
    }

    #[test]
    fn a_running_step_counts_against_its_locks() {
        let mut s = shape(&[&[], &[]]);
        s.locks[NETWORK].slots = 1;
        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 0, 3);
        let t = tick(&s, &request(&[0, 1], 5), &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[1], StepState::Waiting(Wait::Lock(NETWORK)));
    }

    /// A lock of one slot is a mutex: two sources that share an account's
    /// quota never run together, whatever their budget allows.
    #[test]
    fn a_named_mutex_keeps_its_holders_apart_and_nobody_else() {
        let mut s = shape(&[&[], &[], &[]]);
        s.locks.push(LockShape {
            name: "quota".into(),
            slots: 1,
        });
        let quota = s.locks.len() - 1;
        for i in [0, 1] {
            s.steps[i].locks.push((quota, Hold::Shared));
        }
        let facts = all_fresh(&s, 1);
        let t = tick(&s, &request(&[0, 1, 2], 5), &facts);
        assert_eq!(started(&t), vec![0, 2]);
        assert_eq!(t.states[1], StepState::Waiting(Wait::Lock(quota)));
    }

    /// Exclusive takes every slot: it waits for every shared holder, and
    /// every shared holder waits for it.
    #[test]
    fn an_exclusive_holder_runs_alone() {
        let mut s = shape(&[&[], &[], &[]]);
        s.locks.push(LockShape {
            name: "gpu".into(),
            slots: 3,
        });
        let gpu = s.locks.len() - 1;
        s.steps[0].locks.push((gpu, Hold::Shared));
        s.steps[1].locks.push((gpu, Hold::Exclusive));
        s.steps[2].locks.push((gpu, Hold::Shared));

        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 0, 3);
        let t = tick(&s, &request(&[0, 1, 2], 5), &facts);
        assert_eq!(t.states[1], StepState::Waiting(Wait::Lock(gpu)));
        assert_eq!(started(&t), vec![2], "shared holders share");

        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 1, 3);
        let t = tick(&s, &request(&[0, 1, 2], 5), &facts);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[0], StepState::Waiting(Wait::Lock(gpu)));
        assert_eq!(t.states[2], StepState::Waiting(Wait::Lock(gpu)));
    }

    #[test]
    fn a_changed_definition_makes_a_fresh_step_stale() {
        let mut s = chain();
        let facts = all_fresh(&s, 1);
        s.steps[1].fingerprint = "fp1-edited".into();
        let t = tick(&s, &request(&[1], 5), &facts);
        assert_eq!(started(&t), vec![1]);
    }

    #[test]
    fn a_step_outside_every_request_is_left_alone_however_stale() {
        let s = shape(&[&[], &[0], &[], &[2]]);
        let mut facts = all_fresh(&s, 1);
        facts.steps[3].last_success = None;
        let t = tick(&s, &request(&[0], 5), &facts);
        assert_eq!(started(&t), vec![0]);
        assert_eq!(t.states[3], StepState::Stale);
    }
}
