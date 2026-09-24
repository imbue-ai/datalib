//! Seeded random walks over the scenarios the hand-written tests pick
//! one at a time. Each seed builds a root with a doltlite source `A`, a
//! file source `B` and a consumer `C` of `A`, and plays episodes: a person
//! syncs (after maybe pausing a source), each running source does a
//! random handful of things, and each ends in one of every way a step
//! ends — finishing, failing, being retried, crashing, being killed,
//! being stopped or paused mid-stall or mid-spin, ignoring the stop.
//! After every episode the invariant is checked: what was acknowledged is
//! on disk and on `main`, and nothing else is. A failure names its seed;
//! `HARNESS_SEED=<n>` replays that one alone.

use std::collections::BTreeSet;

use datalib_dag::supervisor::store::RequestOutcome;

use crate::harness::{reads, step, Harness};

const A: &str = "a/ingest";
const B: &str = "b/ingest";
const C: &str = "c/render";

/// xorshift64*: enough randomness to walk, and a seed replays exactly.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

/// How a running source's invocation ends.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ending {
    Ok,
    FailData,
    FailTransient,
    Crash,
    Kill,
    PauseStalled,
    PauseSpinning,
}

const ENDINGS: [Ending; 7] = [
    Ending::Ok,
    Ending::FailData,
    Ending::FailTransient,
    Ending::Crash,
    Ending::Kill,
    Ending::PauseStalled,
    Ending::PauseSpinning,
];

/// What the walk knows that the harness's model of files and rows does
/// not: `A`'s version as the runner computes it, and the one `C` last read.
struct Walk {
    h: Harness,
    rng: Rng,
    seed: u64,
    /// With a store, its head, moved by the schema commit and by every
    /// commit that changes something; without one, the tree's hash, taken
    /// at each success only.
    a_version: Option<String>,
    c_read: Option<String>,
    a_store_made: bool,
    a_commits: u32,
    next_file: u32,
}

pub async fn walk(seed: u64, episodes: u32, trail: std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let mut h = Harness::new(&[step(A), step(B), reads(C, &[A])]).await;
    h.trail = trail;
    let mut w = Walk {
        h,
        rng: Rng::new(seed),
        seed,
        a_version: None,
        c_read: None,
        a_store_made: false,
        a_commits: 0,
        next_file: 0,
    };
    for episode in 0..episodes {
        w.episode(episode).await;
    }
    w.h.finish().await;
}

impl Walk {
    fn ctx(&self, episode: u32) -> String {
        format!("seed {} episode {episode}", self.seed)
    }

    async fn episode(&mut self, episode: u32) {
        let ctx = self.ctx(episode);
        self.h.log(format!("── {ctx}"));
        // Which sources, and whether one sits this sync out paused.
        let roots: Vec<&str> = match self.rng.below(3) {
            0 => vec![A],
            1 => vec![B],
            _ => vec![A, B],
        };
        let paused_before: Option<&str> = if self.rng.chance(15) {
            Some(*self.rng.pick(&roots))
        } else {
            None
        };
        if let Some(p) = paused_before {
            self.h.pause(p).await;
        }
        // Stopping the whole request is its own ending, for every source
        // in it at once; the rest end one by one.
        let stop_request = self.rng.chance(20);
        let deaf: BTreeSet<&str> = roots
            .iter()
            .copied()
            .filter(|_| stop_request && self.rng.chance(30))
            .collect();

        let request = self.h.sync(&roots).await;
        let running: Vec<&str> = roots
            .iter()
            .copied()
            .filter(|s| Some(*s) != paused_before)
            .collect();
        for &s in &running {
            self.h.expect(s, "started").await;
        }
        // A paused `A` holds nothing back: `C`, in the request's scope,
        // reads what `A` last published, now.
        if roots.contains(&A) && paused_before == Some(A) {
            self.consumer(&ctx).await;
        }
        let mut order = running.clone();
        if self.rng.chance(50) {
            order.reverse();
        }

        let mut failed = false;
        let mut paused_mid = Vec::new();
        for &s in &order {
            self.work(s).await;
            if stop_request {
                // Leave it waiting on its next instruction, or stuck in a
                // stall or a spin, for the stop to reach.
                if deaf.contains(s) {
                    self.h.done(s, "on_stop ignore").await;
                }
                match self.rng.below(3) {
                    0 => {}
                    1 => {
                        self.h.done(s, "stall").await;
                    }
                    _ => {
                        self.h.done(s, "spin").await;
                    }
                }
                continue;
            }
            let ending = self.end(s, &ctx).await;
            failed |= matches!(ending, Ending::FailData | Ending::Crash | Ending::Kill);
            if matches!(ending, Ending::PauseStalled | Ending::PauseSpinning) {
                paused_mid.push(s);
            }
            if s == A {
                self.consumer(&ctx).await;
            }
        }

        if stop_request {
            self.h.stop(&request).await;
            for &s in &running {
                if deaf.contains(s) {
                    self.h.expect(s, "ignoring a stop").await;
                    // Killed at the grace, with nothing more to say.
                    self.no_invocation_of(s, &ctx).await;
                } else {
                    self.h.expect(s, "stopped").await;
                }
                self.h.forget_held(s);
            }
            assert_eq!(
                self.h.closed(&request).await,
                Some(RequestOutcome::Stopped),
                "{ctx}"
            );
        } else {
            let outcome = self.h.closed(&request).await;
            if paused_mid.is_empty() && paused_before.is_none() {
                let want = if failed {
                    RequestOutcome::Failed
                } else {
                    RequestOutcome::Done
                };
                assert_eq!(outcome, Some(want), "{ctx}");
            }
        }
        for s in [A, B, C] {
            self.no_invocation_of(s, &ctx).await;
        }
        self.h.check_files(A);
        self.h.check_files(B);
        self.h.check_rows(A).await;
        for s in paused_mid.into_iter().chain(paused_before) {
            self.h.resume(s).await;
        }
    }

    /// A random handful of what a source does while it runs.
    async fn work(&mut self, s: &str) {
        for _ in 0..self.rng.below(5) {
            match (s, self.rng.below(4)) {
                (A, 0) => {
                    let ops = self.ops();
                    let ack = self.h.done(A, &format!("batch commit {ops}")).await;
                    self.store_opened();
                    self.committed(&ack);
                }
                (A, 1) => {
                    let ops = self.ops();
                    self.h.done(A, &format!("batch hold {ops}")).await;
                    self.store_opened();
                }
                (A, 2) => {
                    let ack = self.h.done(A, "commit").await;
                    self.committed(&ack);
                }
                (_, 3) => {
                    let n = self.rng.below(1000) as i64;
                    self.h.done(s, &format!("metric seen {n}")).await;
                }
                _ => {
                    self.next_file += 1;
                    let f = self.next_file;
                    if self.rng.chance(20) {
                        let n = self.rng.below(200_000);
                        self.h.done(s, &format!("fill big{f} {n}")).await;
                    } else {
                        self.h.done(s, &format!("write f{f} v{f}")).await;
                    }
                }
            }
        }
    }

    fn ops(&mut self) -> String {
        (0..1 + self.rng.below(4))
            .map(|_| {
                let id = self.rng.below(6);
                if self.rng.chance(25) {
                    format!("del:k{id}")
                } else {
                    format!("put:k{id}:{}", self.rng.below(100))
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The first open of a store commits its schema, which is a version
    /// like any other: `C` has something to read from then on.
    fn store_opened(&mut self) {
        if !std::mem::replace(&mut self.a_store_made, true) {
            self.a_head_moved();
        }
    }

    fn committed(&mut self, ack: &str) {
        if ack != "committed -" {
            self.a_head_moved();
        }
    }

    fn a_head_moved(&mut self) {
        self.a_commits += 1;
        self.a_version = Some(format!("store:{}", self.a_commits));
    }

    /// A success with no store is versioned by hashing what the tree holds.
    fn a_succeeded(&mut self) {
        if !self.a_store_made {
            let files = self.h.files.get(A).cloned().unwrap_or_default();
            self.a_version = Some(format!("files:{files:?}"));
        }
    }

    /// End a running source's invocation, retries included.
    async fn end(&mut self, s: &str, ctx: &str) -> Ending {
        let mut attempt = 1;
        loop {
            let mut ending = *self.rng.pick(&ENDINGS);
            // The third attempt is the last the policy allows.
            if ending == Ending::FailTransient && attempt == 3 {
                ending = Ending::FailData;
            }
            match ending {
                Ending::Ok => {
                    self.h.done(s, "ok").await;
                    if s == A {
                        self.a_succeeded();
                    }
                }
                Ending::FailData => {
                    self.h.done(s, "fail data").await;
                }
                Ending::FailTransient => {
                    self.h.done(s, "fail transient").await;
                }
                Ending::Crash => {
                    self.h.done(s, "crash").await;
                }
                Ending::Kill => {
                    self.h.done(s, "kill").await;
                }
                Ending::PauseStalled | Ending::PauseSpinning => {
                    let how = if ending == Ending::PauseStalled {
                        "stall"
                    } else {
                        "spin"
                    };
                    self.h.done(s, how).await;
                    self.h.pause(s).await;
                    self.h.expect(s, "stopped").await;
                }
            }
            // Whatever it held uncommitted went with its process.
            self.h.forget_held(s);
            if ending != Ending::FailTransient {
                self.no_invocation_of(s, ctx).await;
                return ending;
            }
            // Retried inside the same invocation, by a new process.
            attempt += 1;
            self.h.expect(s, &format!("started {attempt}")).await;
            self.work(s).await;
        }
    }

    /// `C` runs after `A` ends if what it reads moved since it last ran;
    /// it reads `A` at the version it was started against, which is all
    /// `A` has committed.
    async fn consumer(&mut self, ctx: &str) {
        if self.a_version.is_none() || self.a_version == self.c_read {
            return;
        }
        self.h.expect(C, "started").await;
        let want = self.h.rows.get(A).map_or(0, |r| r.len());
        let got = self.h.done(C, "count").await;
        assert_eq!(
            got,
            format!("count {A}={want}"),
            "{ctx}: C read the wrong version"
        );
        self.h.done(C, "ok").await;
        self.c_read = self.a_version.clone();
    }

    /// Until the loop has seen `s`'s process end: an ack is written just
    /// before the exit it announces.
    async fn no_invocation_of(&mut self, s: &str, ctx: &str) {
        let what = format!("{ctx}: {s}'s invocation to close");
        let deadline = tokio::time::Instant::now() + crate::harness::DEADLINE;
        loop {
            let running = self.h.store.running_invocations().await.unwrap();
            if !running.iter().any(|r| r.step == s) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what}: not within the deadline"
            );
            self.h.next_commit(&what).await;
        }
    }
}
