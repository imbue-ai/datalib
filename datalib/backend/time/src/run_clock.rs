//! A run's clock: the instant the run began, which `--now` may pin, moved
//! on by the time that has passed since. Every stamp that says when a run
//! began or ended is read from it, so a run pinned to 2369 ends in 2369.

use crate::IsoOffsetTimestamp;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct RunClock {
    began_at: IsoOffsetTimestamp,
    began: Instant,
}

impl RunClock {
    /// A clock that reads `began_at` now and runs on from it.
    pub fn starting_at(began_at: IsoOffsetTimestamp) -> Self {
        Self {
            began_at,
            began: Instant::now(),
        }
    }

    pub fn now(&self) -> IsoOffsetTimestamp {
        let elapsed = i64::try_from(self.began.elapsed().as_micros()).unwrap_or(i64::MAX);
        self.began_at.bump_micros(elapsed)
    }
}

/// When a run that began at `began_at` and was left open by a process
/// now gone ended, as best anyone can say: now, on the wall clock, but
/// never before it began, which a run pinned ahead of the clock would.
pub fn closed_now(began_at: &str) -> IsoOffsetTimestamp {
    let now = IsoOffsetTimestamp::now_local();
    match crate::parse_strict(began_at) {
        Ok(began) if began.inner() > now.inner() => began,
        _ => now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run pinned far from the wall clock ends on its own clock: the
    /// demo's bake, pinned to 2369, recorded an end in the present.
    #[test]
    fn a_pinned_clock_runs_on_from_its_pin() {
        let pin = crate::parse_strict("2369-04-15T00:00:00+00:00").unwrap();
        let clock = RunClock::starting_at(pin.clone());
        let later = clock.now();
        assert!(later.inner() >= pin.inner());
        assert!(later.inner() < pin.bump_micros(60_000_000).inner());
        assert_eq!(later.offset_secs(), 0);
    }

    #[test]
    fn a_run_left_open_is_not_closed_before_it_began() {
        let ahead = "2369-04-15T00:00:00+00:00";
        assert_eq!(closed_now(ahead).to_rfc3339(), ahead);
        let behind = "2001-01-01T00:00:00+00:00";
        assert!(closed_now(behind).inner() > crate::parse_strict(behind).unwrap().inner());
    }
}
