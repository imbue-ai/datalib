//! The reconcile tick: from the graph, the open requests and the facts,
//! each step's state and the starts, stops and request closures to make.
//! Pure — no clock, no I/O; the host turns events into [`Facts`] and acts
//! on the [`Tick`]. The rules are `docs/dev/plans/supervisor.md` §2.2–§2.4.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

pub type StepIx = usize;
pub type SinkIx = usize;

/// A point in the supervisor's own order of events, handed out by the
/// host. Never a wall-clock stamp: "started after this request opened"
/// has to be exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seq(pub u64);

/// Which budget a step's invocations count against, so a download
/// waiting on a rate limit does not keep a render from starting.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Class {
    Network,
    Cpu,
    Index,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// The graph as the tick needs it: who writes and reads which sink.
#[derive(Debug, Clone)]
pub struct Shape {
    pub steps: Vec<StepShape>,
    pub sink_count: usize,
    /// Every step, producers before the steps that read them.
    pub topo: Vec<StepIx>,
}

#[derive(Debug, Clone)]
pub struct StepShape {
    pub writes: SinkIx,
    pub reads: Vec<SinkIx>,
    /// The hash of the step's own definition; a change makes it stale.
    pub fingerprint: String,
    pub class: Class,
}

/// What people have asked for: the open requests and the paused steps.
#[derive(Debug, Clone, Default)]
pub struct Intent {
    pub requests: Vec<Request>,
    pub paused: BTreeSet<StepIx>,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub roots: Vec<StepIx>,
    pub opened: Seq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    pub network: usize,
    pub cpu: usize,
    pub index: usize,
}

impl Budgets {
    /// What `--parallelism N` means: N downloads and N renders at once,
    /// and the two index steps free to run beside each other.
    pub fn from_parallelism(n: usize) -> Self {
        Self {
            network: n,
            cpu: n,
            index: 2,
        }
    }

    fn of(&self, class: Class) -> usize {
        match class {
            Class::Network => self.network,
            Class::Cpu => self.cpu,
            Class::Index => self.index,
        }
    }
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
    /// Running steps nobody wants any more, or that are paused.
    pub stops: Vec<StepIx>,
    /// Requests that close now, by index into [`Intent::requests`].
    pub closed: Vec<(usize, Outcome)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub step: StepIx,
    pub consumed: Consumed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// No open request wants it, and it is up to date.
    Idle,
    /// No open request wants it, and it is out of date.
    Stale,
    /// Wanted, and up to date.
    Fresh,
    Running,
    Paused,
    /// Its retries ran out and nothing it reads has moved since.
    Failed,
    /// Out of date, but a producer it reads has never published
    /// anything and is not going to run, so there is nothing to read.
    Blocked(StepIx),
    Waiting(Wait),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// A producer it reads is running or about to.
    Upstream(StepIx),
    /// Another writer of its sink is running.
    Sink(SinkIx),
    Budget(Class),
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

pub fn tick(shape: &Shape, intent: &Intent, facts: &Facts, budgets: &Budgets) -> Tick {
    let n = shape.steps.len();
    let mut writers: Vec<Vec<StepIx>> = vec![Vec::new(); shape.sink_count];
    let mut readers: Vec<Vec<StepIx>> = vec![Vec::new(); shape.sink_count];
    for (i, s) in shape.steps.iter().enumerate() {
        writers[s.writes].push(i);
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

    let mut used: BTreeMap<Class, usize> = BTreeMap::new();
    for (i, f) in facts.steps.iter().enumerate() {
        if f.running.is_some() {
            *used.entry(shape.steps[i].class).or_default() += 1;
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
            if intent.paused.contains(&i) || wanting[i].is_empty() {
                stops.push(i);
            }
            continue;
        }
        if intent.paused.contains(&i) {
            states[i] = StepState::Paused;
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
                a.failed && a.started >= intent.requests[r].opened && a.consumed == now
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

        if let Some(w) = blocking_producer(i, shape, facts, &writers, &pending) {
            pending[i] = true;
            states[i] = StepState::Waiting(Wait::Upstream(w));
            continue;
        }
        if let Some(w) = nothing_to_read(step, facts, &writers) {
            if pending[w] {
                pending[i] = true;
                states[i] = StepState::Waiting(Wait::Upstream(w));
            } else {
                states[i] = StepState::Blocked(w);
            }
            continue;
        }
        pending[i] = true;
        if writers[step.writes].iter().any(|&w| busy[w]) {
            states[i] = StepState::Waiting(Wait::Sink(step.writes));
            continue;
        }
        let in_use = used.entry(step.class).or_default();
        if *in_use >= budgets.of(step.class) {
            states[i] = StepState::Waiting(Wait::Budget(step.class));
            continue;
        }
        *in_use += 1;
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

/// The roots and everything that reads, transitively, what they write.
fn closure(shape: &Shape, readers: &[Vec<StepIx>], roots: &[StepIx]) -> Vec<bool> {
    let mut seen = vec![false; shape.steps.len()];
    let mut stack: Vec<StepIx> = roots.to_vec();
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        stack.extend(readers[shape.steps[i].writes].iter().copied());
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

fn started_since(f: &StepFacts, opened: Seq) -> bool {
    let running = f.running.as_ref().is_some_and(|r| r.started >= opened);
    let attempted = f.last_attempt.as_ref().is_some_and(|a| a.started >= opened);
    running || attempted
}

/// A writer of what the step reads, when nothing it reads has ever been
/// published. A fan-in reads whichever of its sources exist.
fn nothing_to_read(step: &StepShape, facts: &Facts, writers: &[Vec<StepIx>]) -> Option<StepIx> {
    if step.reads.is_empty() || step.reads.iter().any(|&s| facts.sinks[s].is_some()) {
        return None;
    }
    writers[step.reads[0]].first().copied()
}

/// A producer of something `i` reads that is running or due, unless it is
/// running and streams: its consumers read each seal as it lands, and
/// staleness keeps them from running when nothing new has.
fn blocking_producer(
    i: StepIx,
    shape: &Shape,
    facts: &Facts,
    writers: &[Vec<StepIx>],
    pending: &[bool],
) -> Option<StepIx> {
    shape.steps[i]
        .reads
        .iter()
        .flat_map(|&s| writers[s].iter().copied())
        .find(|&w| {
            let f = &facts.steps[w];
            w != i && pending[w] && !(f.running.is_some() && f.streams_output)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGETS: Budgets = Budgets {
        network: 4,
        cpu: 4,
        index: 1,
    };

    /// A graph in the shape the tree has today: each step writes the sink
    /// its own index names. `reads` are step indices, which are then also
    /// sink indices.
    fn shape(reads: &[&[StepIx]]) -> Shape {
        let steps: Vec<StepShape> = reads
            .iter()
            .enumerate()
            .map(|(i, r)| StepShape {
                writes: i,
                reads: r.to_vec(),
                fingerprint: format!("fp{i}"),
                class: if r.is_empty() {
                    Class::Network
                } else {
                    Class::Cpu
                },
            })
            .collect();
        Shape {
            sink_count: steps.len(),
            topo: (0..steps.len()).collect(),
            steps,
        }
    }

    /// source 0 → render 1 → index 2.
    fn chain() -> Shape {
        let mut s = shape(&[&[], &[0], &[1]]);
        s.steps[2].class = Class::Index;
        s
    }

    /// Every step succeeded against the sinks as they are now.
    fn all_fresh(shape: &Shape, at: u64) -> Facts {
        let sinks: Vec<Option<String>> = (0..shape.sink_count)
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
            paused: BTreeSet::new(),
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
        let t = tick(&s, &request(&[1], 5), &facts, &BUDGETS);
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
        let t = tick(&s, &Intent::default(), &facts, &BUDGETS);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states, vec![StepState::Stale; 3]);
    }

    #[test]
    fn a_request_starts_its_source_and_holds_the_rest_until_it_lands() {
        let s = chain();
        let facts = all_fresh(&s, 1);
        let t = tick(&s, &request(&[0], 5), &facts, &BUDGETS);
        assert_eq!(started(&t), vec![0]);
        assert_eq!(t.states[1], StepState::Fresh);
        assert!(t.closed.is_empty());
    }

    #[test]
    fn a_whole_chain_runs_one_hop_at_a_time_and_then_closes() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let intent = request(&[0], 5);

        let t = tick(&s, &intent, &facts, &BUDGETS);
        let c0 = start_of(&t, 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Ok("v0'"));

        let t = tick(&s, &intent, &facts, &BUDGETS);
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

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert_eq!(started(&t), vec![2]);
        let c2 = start_of(&t, 2);
        run(&mut facts, 2, 8);
        finish(&mut facts, 2, c2, Ok("v2'"));

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.closed, vec![(0, Outcome::Done)]);
    }

    #[test]
    fn a_render_that_moved_nothing_leaves_the_index_alone() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let intent = request(&[0], 5);
        let c0 = start_of(&tick(&s, &intent, &facts, &BUDGETS), 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Ok("v0'"));
        let c1 = start_of(&tick(&s, &intent, &facts, &BUDGETS), 1);
        run(&mut facts, 1, 7);
        finish(&mut facts, 1, c1, Ok("v1"));

        let t = tick(&s, &intent, &facts, &BUDGETS);
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

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert!(t.starts.is_empty());
        assert_eq!(t.states[0], StepState::Running);
        assert!(t.stops.is_empty(), "a wanted step is not stopped");

        let c0 = facts.steps[0].last_success.clone().unwrap();
        finish(&mut facts, 0, c0, Ok("v0"));
        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert_eq!(started(&t), vec![0]);
    }

    #[test]
    fn a_source_that_ran_after_the_request_opened_is_fresh_for_it() {
        let s = chain();
        let facts = all_fresh(&s, 6);
        let t = tick(&s, &request(&[0], 5), &facts, &BUDGETS);
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

        let t = tick(&s, &request(&[0], 5), &facts, &BUDGETS);
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

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert_eq!(
            t.states[1],
            StepState::Fresh,
            "nothing published yet, so nothing to read"
        );

        facts.sinks[0] = Some("v0-sealed".into());
        let t = tick(&s, &intent, &facts, &BUDGETS);
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
        let c1 = start_of(&tick(&s, &intent, &facts, &BUDGETS), 1);
        run(&mut facts, 1, 7);
        facts.sinks[0] = Some("seal-2".into());
        facts.sinks[0] = Some("seal-3".into());
        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert!(t.starts.is_empty(), "one instance at a time: {t:?}");

        finish(&mut facts, 1, c1, Ok("v1-a"));
        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert_eq!(started(&t), vec![1]);
        let c1 = start_of(&t, 1);
        assert_eq!(c1.reads[&0].as_deref(), Some("seal-3"));
        run(&mut facts, 1, 8);
        finish(&mut facts, 1, c1, Ok("v1-b"));

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert!(!started(&t).contains(&1), "{t:?}");
    }

    #[test]
    fn an_exhausted_failure_closes_the_request_failed_and_is_not_restarted() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let intent = request(&[0], 5);
        let c0 = start_of(&tick(&s, &intent, &facts, &BUDGETS), 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Err(()));

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[0], StepState::Failed);
        assert_eq!(t.closed, vec![(0, Outcome::Failed { step: 0 })]);
    }

    #[test]
    fn a_new_request_retries_a_failed_step() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let c0 = start_of(&tick(&s, &request(&[0], 5), &facts, &BUDGETS), 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Err(()));

        let t = tick(&s, &request(&[0], 9), &facts, &BUDGETS);
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
        let c1 = start_of(&tick(&s, &intent, &facts, &BUDGETS), 1);
        run(&mut facts, 1, 7);
        finish(&mut facts, 1, c1, Err(()));

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert_eq!(t.states[1], StepState::Failed);
        assert!(t.starts.is_empty());

        facts.sinks[0] = Some("seal-2".into());
        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert_eq!(started(&t), vec![1]);
    }

    #[test]
    fn a_consumer_reads_what_a_failed_producer_committed() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        let intent = request(&[0], 5);
        let c0 = start_of(&tick(&s, &intent, &facts, &BUDGETS), 0);
        run(&mut facts, 0, 6);
        facts.sinks[0] = Some("partial".into());
        finish(&mut facts, 0, c0, Err(()));

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert_eq!(started(&t), vec![1]);
        assert_eq!(start_of(&t, 1).reads[&0].as_deref(), Some("partial"));
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
        let c0 = start_of(&tick(&s, &intent, &facts, &BUDGETS), 0);
        run(&mut facts, 0, 6);
        finish(&mut facts, 0, c0, Err(()));

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[1], StepState::Blocked(0));
        assert_eq!(t.closed, vec![(0, Outcome::Failed { step: 0 })]);
    }

    #[test]
    fn a_blocked_step_fails_a_request_that_reaches_it_but_not_its_producer() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        facts.sinks[0] = None;
        let t = tick(&s, &request(&[1], 5), &facts, &BUDGETS);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.closed, vec![(0, Outcome::Failed { step: 1 })]);
    }

    #[test]
    fn a_paused_step_never_starts_and_does_not_hold_its_request_open() {
        let s = chain();
        let facts = all_fresh(&s, 1);
        let mut intent = request(&[0], 5);
        intent.paused.insert(0);

        let t = tick(&s, &intent, &facts, &BUDGETS);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(t.states[0], StepState::Paused);
        assert_eq!(t.closed, vec![(0, Outcome::Done)]);
    }

    #[test]
    fn pausing_a_running_step_stops_it() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 0, 6);
        let mut intent = request(&[0], 5);
        intent.paused.insert(0);
        assert_eq!(tick(&s, &intent, &facts, &BUDGETS).stops, vec![0]);
    }

    #[test]
    fn a_running_step_no_open_request_wants_is_stopped() {
        let s = chain();
        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 0, 6);
        let t = tick(&s, &Intent::default(), &facts, &BUDGETS);
        assert_eq!(t.stops, vec![0]);
        assert_eq!(t.states[0], StepState::Running);
    }

    #[test]
    fn stopping_one_request_leaves_the_fan_in_to_the_other() {
        // gmail 0 → render 1 ┐
        //                     ├→ index 4
        // slack 2 → render 3 ┘
        let mut s = shape(&[&[], &[0], &[], &[2], &[1, 3]]);
        s.steps[4].class = Class::Index;
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
            paused: BTreeSet::new(),
        };
        assert!(tick(&s, &both, &facts, &BUDGETS).stops.is_empty());

        let slack_only = Intent {
            requests: vec![both.requests[1].clone()],
            paused: BTreeSet::new(),
        };
        let t = tick(&s, &slack_only, &facts, &BUDGETS);
        assert!(t.stops.is_empty(), "{t:?}");
        assert!(!started(&t).contains(&0), "gmail is nobody's any more");
        assert_eq!(t.states[0], StepState::Idle);
    }

    #[test]
    fn a_budget_holds_the_steps_past_it() {
        let s = shape(&[&[], &[], &[]]);
        let facts = all_fresh(&s, 1);
        let budgets = Budgets {
            network: 2,
            ..BUDGETS
        };
        let t = tick(&s, &request(&[0, 1, 2], 5), &facts, &budgets);
        assert_eq!(started(&t), vec![0, 1]);
        assert_eq!(
            t.states[2],
            StepState::Waiting(Wait::Budget(Class::Network))
        );
        assert!(t.closed.is_empty());
    }

    #[test]
    fn a_running_step_counts_against_its_budget() {
        let s = shape(&[&[], &[]]);
        let mut facts = all_fresh(&s, 1);
        run(&mut facts, 0, 3);
        let budgets = Budgets {
            network: 1,
            ..BUDGETS
        };
        let t = tick(&s, &request(&[0, 1], 5), &facts, &budgets);
        assert!(t.starts.is_empty(), "{t:?}");
        assert_eq!(
            t.states[1],
            StepState::Waiting(Wait::Budget(Class::Network))
        );
    }

    #[test]
    fn two_writers_of_one_sink_take_turns() {
        let mut s = shape(&[&[], &[], &[0]]);
        s.steps[1].writes = 0;
        s.sink_count = 3;
        let facts = all_fresh(&s, 1);
        let t = tick(&s, &request(&[0, 1], 5), &facts, &BUDGETS);
        assert_eq!(started(&t), vec![0]);
        assert_eq!(t.states[1], StepState::Waiting(Wait::Sink(0)));
    }

    #[test]
    fn a_changed_definition_makes_a_fresh_step_stale() {
        let mut s = chain();
        let facts = all_fresh(&s, 1);
        s.steps[1].fingerprint = "fp1-edited".into();
        let t = tick(&s, &request(&[1], 5), &facts, &BUDGETS);
        assert_eq!(started(&t), vec![1]);
    }

    #[test]
    fn a_step_outside_every_request_is_left_alone_however_stale() {
        let s = shape(&[&[], &[0], &[], &[2]]);
        let mut facts = all_fresh(&s, 1);
        facts.steps[3].last_success = None;
        let t = tick(&s, &request(&[0], 5), &facts, &BUDGETS);
        assert_eq!(started(&t), vec![0]);
        assert_eq!(t.states[3], StepState::Stale);
    }

    /// strum and serde spell these independently, and both will be read
    /// back: serde from the config, strum from the run store.
    #[test]
    fn class_as_str_matches_the_serde_spelling() {
        for &v in Class::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(Class::parse(v.as_str()), Some(v));
        }
    }
}
