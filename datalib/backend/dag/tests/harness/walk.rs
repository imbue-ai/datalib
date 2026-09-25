//! Seeded random walks over what the hand-written scenarios pick one at a
//! time. Each seed builds a root with two sources, `A` and `B`, and a
//! consumer `C` of `A`, and plays episodes: a person syncs (after maybe
//! pausing a source), each running source does a random handful of things,
//! and each ends in one of every way a step ends — finishing (on a new
//! version or the same one), failing, being retried, crashing, being
//! killed, being paused mid-stall or mid-spin, or the whole request being
//! stopped with the source waiting, stalled or spinning, sometimes deaf to
//! it.
//!
//! After every ending the loop's bookkeeping is checked: the invocation
//! closed, the record says how it ended, `C` ran exactly when `A`'s version
//! moved and was handed the version the loop recorded, the request closed
//! as it should, and no step ever ran twice at once. A failure names its
//! seed and the end of its trail; `HARNESS_SEED=<n>` replays that one.

use std::collections::BTreeSet;

use datalib_dag::supervisor::store::RequestOutcome;

use crate::harness::{reads, step, Harness, DEADLINE};

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
    OkUnmoved,
    FailData,
    FailTransient,
    Crash,
    Kill,
    PauseStalled,
    PauseSpinning,
}

const ENDINGS: [Ending; 8] = [
    Ending::Ok,
    Ending::OkUnmoved,
    Ending::FailData,
    Ending::FailTransient,
    Ending::Crash,
    Ending::Kill,
    Ending::PauseStalled,
    Ending::PauseSpinning,
];

impl Ending {
    /// What the record's last run says after it, retries aside.
    fn status(self) -> &'static str {
        match self {
            Ending::Ok | Ending::OkUnmoved => "succeeded",
            Ending::FailData | Ending::FailTransient | Ending::Crash | Ending::Kill => "failed",
            Ending::PauseStalled | Ending::PauseSpinning => "stopped",
        }
    }
}

struct Walk {
    h: Harness,
    rng: Rng,
    seed: u64,
    /// The version `A` last reported on finishing, which is all the
    /// loop has for it: the puppet's failures report nothing.
    a_version: Option<String>,
    /// The version `C` was last handed.
    c_read: Option<String>,
    versions: u32,
    files: u32,
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
        versions: 0,
        files: 0,
    };
    for episode in 0..episodes {
        w.episode(episode).await;
    }
    w.h.finish().await;
}

impl Walk {
    async fn episode(&mut self, episode: u32) {
        let ctx = format!("seed {} episode {episode}", self.seed);
        self.h.log(format!("── {ctx}"));
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
            failed |= ending.status() == "failed";
            if ending.status() == "stopped" {
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
                } else {
                    self.h.expect(s, "stopped").await;
                }
                // Killed at the grace, if deaf, with nothing more to say.
                self.no_invocation_of(s, &ctx).await;
                self.status_is(s, "stopped", &ctx).await;
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
            assert_eq!(
                self.h.pending_acks(s),
                Vec::<String>::new(),
                "{ctx}: {s} said more"
            );
        }
        for s in paused_mid.into_iter().chain(paused_before) {
            self.h.resume(s).await;
        }
    }

    /// A random handful of what a running source does, none of which the
    /// loop is asked to judge but all of which it has to carry.
    async fn work(&mut self, s: &str) {
        for _ in 0..self.rng.below(5) {
            match self.rng.below(4) {
                0 => {
                    let n = self.rng.below(1000);
                    self.h.done(s, &format!("metric seen {n}")).await;
                }
                1 => {
                    let n = self.rng.below(200_000);
                    self.files += 1;
                    self.h.done(s, &format!("fill big{} {n}", self.files)).await;
                }
                2 => {
                    self.h.done(s, "log warn a warning").await;
                }
                _ => {
                    self.files += 1;
                    self.h
                        .done(s, &format!("write f{0} v{0}", self.files))
                        .await;
                }
            }
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
                Ending::Ok | Ending::OkUnmoved => {
                    let version = match (ending, self.last_version(s)) {
                        (Ending::OkUnmoved, Some(v)) => v,
                        _ => {
                            self.versions += 1;
                            format!("v{}", self.versions)
                        }
                    };
                    self.h.done(s, &format!("ok {version}")).await;
                    if s == A {
                        self.a_version = Some(version);
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
            if ending != Ending::FailTransient {
                self.no_invocation_of(s, ctx).await;
                self.status_is(s, ending.status(), ctx).await;
                return ending;
            }
            // Retried inside the same invocation, by a new process.
            attempt += 1;
            self.h.expect(s, &format!("started {attempt}")).await;
            self.work(s).await;
        }
    }

    /// The version a source last reported, as the puppet reports it.
    fn last_version(&self, s: &str) -> Option<String> {
        (s == A).then(|| self.a_version.clone()).flatten()
    }

    /// `C` runs after `A` ends exactly when `A`'s version moved since `C`
    /// last read it, and is handed the version the loop recorded.
    async fn consumer(&mut self, ctx: &str) {
        if self.a_version.is_none() || self.a_version == self.c_read {
            return;
        }
        self.h.expect(C, "started").await;
        let got = self.h.done(C, "reads").await;
        let handed = got
            .strip_prefix(&format!("reads {A}="))
            .unwrap_or_else(|| panic!("{ctx}: c said {got:?}"))
            .to_string();
        let a = self.a_version.clone().unwrap();
        assert!(handed.ends_with(&a), "{ctx}: c was handed {handed} for {a}");
        let want = handed.clone();
        self.h
            .until(
                &format!("{ctx}: the record to say a is at {handed}"),
                move |rec, _| {
                    (rec.steps.get(A)?.version.as_deref() == Some(want.as_str())).then_some(())
                },
            )
            .await;
        self.h.done(C, "ok").await;
        self.no_invocation_of(C, ctx).await;
        self.status_is(C, "succeeded", ctx).await;
        self.c_read = self.a_version.clone();
    }

    /// The loop writes a run's outcome the tick after it sees the process
    /// end, so this waits for one to be written, then compares it.
    async fn status_is(&mut self, s: &str, want: &str, ctx: &str) {
        let what = format!("{ctx}: {s}'s last run to say how it ended");
        let got = self
            .h
            .until(&what, |rec, _| {
                let status = &rec.steps.get(s)?.last_run.as_ref()?.status;
                (!status.is_empty()).then(|| status.clone())
            })
            .await;
        assert_eq!(got, want, "{ctx}: {s}'s last run");
    }

    /// Until the loop has seen `s`'s process end.
    async fn no_invocation_of(&mut self, s: &str, ctx: &str) {
        let what = format!("{ctx}: {s}'s invocation to close");
        let deadline = tokio::time::Instant::now() + DEADLINE;
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
