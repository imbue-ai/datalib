//! What a step is told about its inputs as it starts: which moved since its
//! last success and which last ended without succeeding
//! (`DATALIB_DAG_CHANGED_INPUTS`, `DATALIB_DAG_FAILED_INPUTS`). Pure; the
//! loop hands over what it knows (`round.rs`, `ctx_for`).

use super::tick::{Consumed, SinkIx, StepFacts};
use crate::run_state::RunState;

/// The producers whose version differs from the one the step read at its
/// last success. Every one of them when there is no success under the
/// step's current definition to compare with, so an empty answer can only
/// mean nothing moved.
pub fn changed(reads: &[SinkIx], last_success: Option<&Consumed>, now: &Consumed) -> Vec<SinkIx> {
    match last_success {
        Some(last) if last.fingerprint == now.fingerprint => reads
            .iter()
            .copied()
            .filter(|p| last.reads.get(p) != now.reads.get(p))
            .collect(),
        _ => reads.to_vec(),
    }
}

/// Whether a producer's most recent run ended without succeeding: it
/// failed, gave up `incomplete`, was stopped, or was blocked. One running
/// now has not ended. What this busy period saw of it wins over
/// `recorded`, how its last run ended in the record, which a producer the
/// period has not run still has from an earlier one.
pub fn ended_short(producer: &StepFacts, recorded: Option<RunState>) -> bool {
    if producer.running.is_some() {
        return false;
    }
    match &producer.last_attempt {
        Some(a) => a.failed,
        None => recorded.is_some_and(|s| s.is_terminal() && !s.is_ok()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::supervisor::tick::{Attempt, Running, Seq};

    fn consumed(fingerprint: &str, reads: &[(SinkIx, &str)]) -> Consumed {
        Consumed {
            fingerprint: fingerprint.into(),
            reads: reads
                .iter()
                .map(|&(p, v)| (p, Some(v.to_string())))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    /// A first run used to be told nothing changed, and a step that read
    /// an empty list as "nothing to do" did nothing on its first run.
    #[test]
    fn a_first_run_is_told_every_input_changed() {
        let now = consumed("fp", &[(0, "a1"), (1, "b1")]);
        assert_eq!(changed(&[0, 1], None, &now), [0, 1]);
    }

    #[test]
    fn a_changed_definition_is_told_every_input_changed() {
        let last = consumed("fp-old", &[(0, "a1"), (1, "b1")]);
        let now = consumed("fp", &[(0, "a1"), (1, "b1")]);
        assert_eq!(changed(&[0, 1], Some(&last), &now), [0, 1]);
    }

    #[test]
    fn a_rerun_is_told_only_what_moved() {
        let last = consumed("fp", &[(0, "a1"), (1, "b1")]);
        let now = consumed("fp", &[(0, "a2"), (1, "b1")]);
        assert_eq!(changed(&[0, 1], Some(&last), &now), [0]);
        assert!(changed(&[0, 1], Some(&now), &now).is_empty());
    }

    fn attempt(failed: bool, stopped: bool) -> Attempt {
        Attempt {
            started: Seq(1),
            failed,
            stopped,
            consumed: consumed("fp", &[]),
        }
    }

    #[test]
    fn a_producer_that_failed_or_stopped_this_period_ended_short() {
        let mut f = StepFacts {
            last_attempt: Some(attempt(true, false)),
            ..Default::default()
        };
        assert!(
            ended_short(&f, Some(RunState::Succeeded)),
            "this period wins"
        );
        f.last_attempt = Some(attempt(true, true));
        assert!(ended_short(&f, None), "a stop is not a success");
        f.last_attempt = Some(attempt(false, false));
        assert!(!ended_short(&f, Some(RunState::Failed)), "this period wins");
    }

    #[test]
    fn a_producer_this_period_has_not_run_is_read_from_the_record() {
        let f = StepFacts::default();
        for (recorded, short) in [
            (Some(RunState::Failed), true),
            (Some(RunState::Stopped), true),
            (Some(RunState::Blocked), true),
            (Some(RunState::Succeeded), false),
            (Some(RunState::SkippedUpToDate), false),
            (Some(RunState::Running), false),
            (None, false),
        ] {
            assert_eq!(ended_short(&f, recorded), short, "{recorded:?}");
        }
    }

    /// A streaming producer's consumer reads its seals while it runs; an
    /// earlier run's failure says nothing about those.
    #[test]
    fn a_running_producer_has_not_ended() {
        let f = StepFacts {
            running: Some(Running { started: Seq(2) }),
            last_attempt: Some(attempt(true, false)),
            ..Default::default()
        };
        assert!(!ended_short(&f, Some(RunState::Failed)));
    }
}
