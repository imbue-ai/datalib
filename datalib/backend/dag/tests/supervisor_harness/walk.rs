//! A seeded random walk over everything the scenarios do one at a time:
//! sources `a` and `b`, a consumer `c` of `a`, a fan-in `d` of both, and
//! syncs, stops, pauses,
//! resumes and every way a step can end, in any order. The invariants are
//! checked as it goes (one process per step, on every start) and after
//! each episode. `HARNESS_SEED=<n>` replays one walk; `HARNESS_SEEDS=<n>`
//! runs that many.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use datalib_dag::supervisor::store::RequestOutcome;

use crate::harness::{reads, source, Clocks, Harness, Seen};

const EPISODES: usize = 4;
const ACTIONS: usize = 12;
const STEPS: [&str; 4] = ["a", "b", "c", "d"];

/// splitmix64: enough randomness for a walk, and the same walk for a seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, of: &'a [T]) -> &'a T {
        &of[self.below(of.len())]
    }
}

/// What the walk knows of a puppet it may tell things to.
#[derive(Default)]
struct Puppet {
    /// The process now running, if it is reading instructions.
    listening: Option<i32>,
}

struct Walk {
    h: Harness,
    rng: Rng,
    puppets: BTreeMap<&'static str, Puppet>,
    open: Vec<String>,
    stopped: BTreeSet<String>,
    paused: BTreeSet<&'static str>,
    /// Per source: how many runs its requests and resumes allow.
    allowed: BTreeMap<&'static str, usize>,
    /// Per source: what the walk did that allows a run, for the message
    /// when a source runs more often than that.
    allowances: BTreeMap<&'static str, Vec<String>>,
    requests: Vec<String>,
    version: u32,
}

impl Walk {
    fn absorb(&mut self, step: &str, pid: i32, what: &str) {
        let Some(p) = self.puppets.get_mut(step) else {
            return;
        };
        if what.starts_with("started") {
            p.listening = Some(pid);
        } else if p.listening == Some(pid)
            && [
                "sigint",
                "did stall",
                "did spin",
                "did ok",
                "did fail",
                "did exit",
                "did crash",
                "did kill",
            ]
            .iter()
            .any(|end| what.starts_with(end))
        {
            p.listening = None;
        }
    }

    fn take_acks(&mut self) {
        for (step, pid, what) in self.h.take_acks() {
            self.absorb(&step, pid, &what);
        }
    }

    async fn sync(&mut self) {
        let roots: &[&str] = self.rng.pick(&[&["a"][..], &["b"], &["a", "b"], &["c"]]);
        for r in roots {
            if let Some(n) = self.allowed.get_mut(r) {
                *n += 1;
            }
            if let Some(log) = self.allowances.get_mut(r) {
                log.push(format!("sync {roots:?}"));
            }
        }
        // Nothing says the loop has taken a request on while an older one
        // for the same steps is open: a step's record names the oldest.
        let id = self.h.sync(roots).await;
        self.open.push(id.clone());
        self.requests.push(id);
    }

    async fn stop(&mut self) {
        if self.open.is_empty() {
            return;
        }
        let id = self.open.remove(self.rng.below(self.open.len()));
        self.stopped.insert(id.clone());
        self.h.stop(&id).await;
        self.h.closed(&id).await;
    }

    async fn pause(&mut self) {
        let step = *self.rng.pick(&STEPS);
        if !self.paused.insert(step) {
            return;
        }
        self.h.pause(step).await;
        let what = format!("{step} to read paused");
        self.h
            .until(&what, |s| {
                let st = s.record.steps.get(step)?;
                (st.paused_by.as_deref() == Some("person")).then_some(())
            })
            .await;
    }

    async fn resume(&mut self) {
        let Some(&step) = self
            .paused
            .iter()
            .nth(self.rng.below(self.paused.len().max(1)))
        else {
            return;
        };
        self.paused.remove(step);
        if let Some(n) = self.allowed.get_mut(step) {
            *n += 1;
        }
        if let Some(log) = self.allowances.get_mut(step) {
            log.push("resume".to_string());
        }
        self.h.resume(step).await;
        let what = format!("{step} to read resumed");
        self.h
            .until(&what, |s| {
                let paused = s
                    .record
                    .steps
                    .get(step)
                    .and_then(|st| st.paused_by.as_ref());
                paused.is_none().then_some(())
            })
            .await;
    }

    /// Tell a puppet that is reading instructions to do something, and
    /// wait for it to say it did, or to be stopped first.
    async fn tell(&mut self) {
        self.take_acks();
        let listening: Vec<(&'static str, i32)> = self
            .puppets
            .iter()
            .filter_map(|(s, p)| Some((*s, p.listening?)))
            .collect();
        if listening.is_empty() {
            return;
        }
        let (step, pid) = *self.rng.pick(&listening);
        self.version += 1;
        let v = self.version;
        let mut choices = vec![
            format!("ok v{v}"),
            "fail transient".to_string(),
            "fail data".to_string(),
            "exit 3".to_string(),
            "crash".to_string(),
            "kill".to_string(),
            "stall".to_string(),
            "on_stop ignore".to_string(),
            "reads".to_string(),
            format!("metric rows {v}"),
        ];
        if step == "a" || step == "b" {
            choices.extend(["streams".to_string(), format!("seal s{v} {v}")]);
        }
        let instruction = self.rng.pick(&choices).clone();
        self.h.tell(step, &instruction);
        let want = if instruction == "reads" {
            "reads".to_string()
        } else {
            format!("did {instruction}")
        };
        let what = format!("{step} [{pid}] to do {instruction:?} or be stopped");
        let (w, p) = self
            .h
            .wait(&what, |s| match s {
                Seen::Ack {
                    step: st,
                    pid: p,
                    what: w,
                } if st == step && *p == pid && (w.starts_with(&want) || w == "sigint") => {
                    Some((w.clone(), *p))
                }
                _ => None,
            })
            .await;
        self.absorb(step, p, &w);
    }

    /// Stop everything and wait for the loop to be at rest; then check.
    async fn quiesce(&mut self) {
        for id in std::mem::take(&mut self.open) {
            self.stopped.insert(id.clone());
            self.h.stop(&id).await;
        }
        for step in std::mem::take(&mut self.paused) {
            self.h.resume(step).await;
        }
        let requests = self.requests.clone();
        self.h
            .until("every request closed and every invocation ended", |s| {
                let closed = requests.iter().all(|id| s.outcome(id).is_some());
                let ended = s.invocations.iter().all(|(_, end)| end.is_some());
                (closed && ended).then_some(())
            })
            .await;
        let state = self.h.state().await;
        for id in &self.requests {
            let outcome = state.outcome(id).flatten();
            if outcome == Some(RequestOutcome::Stopped) && !self.stopped.contains(id) {
                self.h
                    .fail(&format!("{id} closed as stopped, and nobody stopped it"));
            }
        }
        for (step, allowed) in &self.allowed {
            let ran = state.started(step);
            if ran > *allowed {
                let runs: Vec<String> = state
                    .invocations
                    .iter()
                    .filter(|(row, _)| row.step == *step)
                    .map(|(row, end)| match end {
                        Some(e) => format!(
                            "{} {} (attempts {}, exit {:?}, signal {:?})",
                            row.started_at_utc, e.outcome, e.attempts, e.exit_code, e.signal
                        ),
                        None => format!("{} still open", row.started_at_utc),
                    })
                    .collect();
                self.h.fail(&format!(
                    "{step} ran {ran} times; its syncs and resumes allow {allowed}\n\
                     its runs:\n  {}\nwhat allowed them:\n  {}",
                    runs.join("\n  "),
                    self.allowances[step].join("\n  ")
                ));
            }
        }
        // Every invocation has ended, so every process it ran has.
        let live = self.h.live_puppets();
        if !live.is_empty() {
            self.h
                .fail(&format!("puppets outlived their invocations: {live:?}"));
        }
        self.take_acks();
        for p in self.puppets.values_mut() {
            p.listening = None;
        }
    }
}

async fn walk(seed: u64) {
    let clocks = Clocks {
        stop_grace: Duration::from_millis(20),
        backoff: Duration::ZERO,
        ..Clocks::default()
    };
    let steps = [
        source("a"),
        source("b"),
        reads("c", &["a"]),
        reads("d", &["a", "b"]),
    ];
    let mut h = Harness::with(&steps, clocks).await;
    h.context = format!("walk seed {seed} (replay: HARNESS_SEED={seed}): ");
    let mut w = Walk {
        h,
        rng: Rng(seed),
        puppets: STEPS.iter().map(|s| (*s, Puppet::default())).collect(),
        open: Vec::new(),
        stopped: BTreeSet::new(),
        paused: BTreeSet::new(),
        allowed: [("a", 0), ("b", 0)].into_iter().collect(),
        allowances: [("a", Vec::new()), ("b", Vec::new())].into_iter().collect(),
        requests: Vec::new(),
        version: 0,
    };
    for episode in 0..EPISODES {
        w.h.say(format!("walk: episode {episode}"));
        for _ in 0..ACTIONS {
            match w.rng.below(10) {
                0..=2 => w.sync().await,
                3 => w.stop().await,
                4 => w.pause().await,
                5 => w.resume().await,
                _ => w.tell().await,
            }
        }
        w.quiesce().await;
    }
    w.h.finish().await;
}

fn seeds() -> Vec<u64> {
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
    match (var("HARNESS_SEED"), var("HARNESS_SEEDS")) {
        (Some(seed), _) => vec![seed],
        (None, n) => (1..=n.unwrap_or(32)).collect(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn random_walks_keep_every_invariant() {
    // As many walks at once as the machine has cores: more is a test of
    // the scheduler under overload, where a writer descheduled between its
    // commit and its announcement for two backstops reads as a miss.
    let at_once = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut set = tokio::task::JoinSet::new();
    for seed in seeds() {
        if set.len() == at_once {
            joined(set.join_next().await);
        }
        set.spawn(walk(seed));
    }
    while let Some(done) = set.join_next().await {
        joined(Some(done));
    }
}

fn joined(done: Option<Result<(), tokio::task::JoinError>>) {
    if let Some(Err(e)) = done {
        std::panic::resume_unwind(e.into_panic());
    }
}
