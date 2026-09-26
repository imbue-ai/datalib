//! What a Manage row's Status column says: the loop's `state` for the
//! step, read from its record, while the step is doing something, and its
//! last outcome while it is at rest. Nothing here works out what the loop
//! is doing; the loop writes it (`datalib_dag::supervisor::record`).

use std::collections::{HashMap, HashSet, VecDeque};

use datalib_dag::supervisor::record::StepRecord;
use datalib_dag::supervisor::tick::StateKind;
use datalib_dag::{Diagnostic, Severity};

/// One row's status, in the shape the Status column draws. The rules
/// here fill `key`, `label`, `at`, `last_success_at` and `detail`; the
/// assembly adds a fraction and segments where a run is in flight.
pub type StatusView = datalib_columns::Status;

/// The word each status key stands for. `skipped_up_to_date` is the
/// runner's word; "Up to date" is what it means to someone looking at
/// a table. The two config statuses borrow no word from the runner's
/// vocabulary: "Failed" would claim it ran, and "Blocked" already means
/// an upstream step failed.
pub const STATUS_LABELS: &[(&str, &str)] = &[
    ("config_rejected", "Not loaded"),
    ("config_blocked", "Can\u{2019}t run"),
    ("running", "Running"),
    ("queued", "Queued"),
    ("off", "Off"),
    ("succeeded", "Succeeded"),
    ("skipped_up_to_date", "Up to date"),
    ("failed", "Failed"),
    ("blocked", "Blocked"),
    ("interrupted", "Interrupted"),
    ("stopped", "Stopped"),
    ("never_run", "Never run"),
];

pub fn status_label(key: &str) -> String {
    STATUS_LABELS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, l)| l.to_string())
        .unwrap_or_else(|| key.replace('_', " "))
}

pub fn view(key: &str, at: Option<String>, detail: Option<String>) -> StatusView {
    StatusView {
        key: key.to_string(),
        label: status_label(key),
        at,
        detail,
        ..Default::default()
    }
}

fn instant(stamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|d| d.timestamp_micros())
}

/// Instant order, with an absent stamp before any present one.
pub fn compare_stamps(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => instant(a).cmp(&instant(b)),
    }
}

/// A step and the ids it reads: the DAG's edges, as far as the Sync
/// button needs them. Applets carry no inputs and are not steps.
#[derive(Debug, Clone, PartialEq)]
pub struct StepEdges {
    pub id: String,
    pub inputs: Vec<String>,
}

/// The source steps a given step ultimately reads from: walk `inputs`
/// up until every branch reaches a step that declares none.
pub fn sources_feeding(steps: &[StepEdges], id: &str) -> Vec<String> {
    let by_id: HashMap<&str, &StepEdges> = steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([id.to_string()]);
    while let Some(at) = queue.pop_front() {
        if !seen.insert(at.clone()) {
            continue;
        }
        // An input naming no step at all is a config the loader would
        // refuse, but this runs against every entry as written.
        let Some(step) = by_id.get(at.as_str()) else {
            continue;
        };
        if step.inputs.is_empty() {
            if at != id {
                found.push(at);
            }
            continue;
        }
        queue.extend(step.inputs.iter().cloned());
    }
    found.sort();
    found
}

/// The sentence a dropped entry's status carries.
pub fn dropped_detail(d: &Diagnostic) -> String {
    match &d.help {
        Some(help) => format!("{} \u{2014} {help}", d.message),
        None => d.message.clone(),
    }
}

/// What a step is doing now, or did last, and when it last succeeded.
/// A step the loader dropped says so and nothing else: a row still
/// reading "Up to date" from last week, for an entry no longer in the
/// graph, is what #209 was about. `run` is the busy period in flight.
pub fn step_status(
    step: Option<&StepRecord>,
    run: Option<&str>,
    dropped: Option<&Diagnostic>,
) -> StatusView {
    if let Some(d) = dropped {
        let key = if d.severity == Severity::Blocked {
            "config_blocked"
        } else {
            "config_rejected"
        };
        return view(key, None, Some(dropped_detail(d)));
    }
    let last = step.and_then(|s| s.last_run.as_ref());
    let detail = step.and_then(|s| s.state_detail.clone());
    let ended = last.map(|l| {
        l.finished_at
            .clone()
            .unwrap_or_else(|| l.started_at.clone())
    });
    // Fresh is wanted and up to date: done for this sync if it has run in
    // it, and otherwise waiting, since what it reads may still move.
    let ran_this_run =
        last.is_some_and(|l| l.finished_at.is_some() && Some(l.run_id.as_str()) == run);
    let now = match step.and_then(|s| s.state) {
        Some(StateKind::Running) => view("running", last.map(|l| l.started_at.clone()), detail),
        Some(StateKind::Waiting) => view("queued", ended, detail),
        Some(StateKind::Fresh) if !ran_this_run => view(
            "queued",
            ended,
            Some(
                "up to date so far; runs again if what it reads moves before the sync ends".into(),
            ),
        ),
        Some(StateKind::Off) => view("off", ended, detail),
        Some(StateKind::Blocked) => view("blocked", ended, detail),
        _ => match last {
            None => view("never_run", None, None),
            // Started, and the loop that started it is gone: the loop
            // writes an outcome for every step it sees end.
            Some(l) if l.status.is_empty() => view(
                "interrupted",
                ended,
                Some(format!(
                    "Started {} and never finished \u{2014} the loop running it stopped \
                     before it did.",
                    l.started_at
                )),
            ),
            Some(l) => view(&l.status, ended, finish_detail(&l.status)),
        },
    };
    StatusView {
        last_success_at: step.and_then(|s| s.last_success_at.clone()),
        ..now
    }
}

/// What the hover says about a step that ended badly. Not the error
/// itself: that is a log line, and the log is where a person reads it
/// with everything that led up to it — a double-click on the cell
/// opens the log there.
fn finish_detail(status: &str) -> Option<String> {
    match status {
        "failed" => Some("double-click to open the log at the error".into()),
        "stopped" => Some("double-click to open the log where it stopped".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_dag::supervisor::record::LastRun;

    const STARTED: &str = "2026-09-01T10:00:01+02:00";
    const ENDED: &str = "2026-09-01T10:00:09+02:00";
    const YESTERDAY: &str = "2026-08-31T09:00:00+02:00";

    fn succeeded_yesterday() -> StepRecord {
        StepRecord {
            last_run: Some(LastRun {
                run_id: "r0".into(),
                started_at: YESTERDAY.into(),
                finished_at: Some(YESTERDAY.into()),
                status: "succeeded".into(),
                attempts: 1,
                error: None,
            }),
            last_success_at: Some(YESTERDAY.into()),
            ..Default::default()
        }
    }

    fn at(state: StateKind, detail: Option<&str>) -> StepRecord {
        StepRecord {
            state: Some(state),
            state_detail: detail.map(str::to_string),
            ..succeeded_yesterday()
        }
    }

    /// While the loop is doing something with a step, the row says what,
    /// in the loop's own words; the last success stays the history it is.
    #[test]
    fn a_step_in_hand_reads_as_the_loop_says() {
        let waiting = step_status(
            Some(&at(StateKind::Waiting, Some("waiting for a/ingest"))),
            None,
            None,
        );
        assert_eq!(waiting.key, "queued");
        assert_eq!(waiting.detail.as_deref(), Some("waiting for a/ingest"));
        assert_eq!(waiting.last_success_at.as_deref(), Some(YESTERDAY));

        let mut running = at(StateKind::Running, None);
        running.last_run.as_mut().unwrap().started_at = STARTED.into();
        let running = step_status(Some(&running), None, None);
        assert_eq!(
            (running.key.as_str(), running.at.as_deref()),
            ("running", Some(STARTED))
        );

        // Up to date so far is not done: what it reads may still move.
        // Having run in this sync, it is.
        assert_eq!(
            step_status(Some(&at(StateKind::Fresh, None)), Some("r0"), None).key,
            "succeeded"
        );
        assert_eq!(
            step_status(Some(&at(StateKind::Fresh, None)), None, None).key,
            "queued"
        );

        let turned_off = step_status(
            Some(&at(StateKind::Off, Some("turned off by claude"))),
            None,
            None,
        );
        assert_eq!(turned_off.label, "Off");
        assert_eq!(turned_off.detail.as_deref(), Some("turned off by claude"));
    }

    /// At rest — idle, stale, failed — the row is the last outcome and
    /// when it came, which says more than "idle".
    #[test]
    fn a_step_at_rest_reads_as_its_last_outcome() {
        for state in [StateKind::Idle, StateKind::Stale] {
            let v = step_status(Some(&at(state, None)), None, None);
            assert_eq!(
                (v.key.as_str(), v.at.as_deref()),
                ("succeeded", Some(YESTERDAY))
            );
        }
        let mut failed = at(StateKind::Failed, None);
        let last = failed.last_run.as_mut().unwrap();
        last.status = "failed".into();
        last.finished_at = Some(ENDED.into());
        let v = step_status(Some(&failed), None, None);
        assert_eq!((v.key.as_str(), v.at.as_deref()), ("failed", Some(ENDED)));
        assert_eq!(v.last_success_at.as_deref(), Some(YESTERDAY));

        assert_eq!(step_status(None, None, None).key, "never_run");
    }

    /// A step started by a loop that then died has an outcome nobody
    /// wrote. Once another loop has settled the record it is at rest, and
    /// says it was interrupted.
    #[test]
    fn a_step_a_dead_loop_left_reads_interrupted() {
        let mut rec = at(StateKind::Stale, None);
        let last = rec.last_run.as_mut().unwrap();
        last.status = String::new();
        last.finished_at = None;
        assert_eq!(step_status(Some(&rec), None, None).key, "interrupted");
    }

    fn diag(severity: Severity) -> Diagnostic {
        Diagnostic {
            severity,
            message: "unknown type".into(),
            help: Some("fix the type".into()),
            entry: None,
            span: None,
            line: None,
            column: None,
        }
    }

    #[test]
    fn a_dropped_entry_outranks_whatever_the_record_remembers() {
        let rec = at(StateKind::Running, None);
        let v = step_status(Some(&rec), None, Some(&diag(Severity::Rejected)));
        assert_eq!(v.key, "config_rejected");
        assert_eq!(v.at, None);
        assert_eq!(v.last_success_at, None);
        assert_eq!(
            v.detail.as_deref(),
            Some("unknown type \u{2014} fix the type")
        );
        let blocked = step_status(Some(&rec), None, Some(&diag(Severity::Blocked)));
        assert_eq!(blocked.key, "config_blocked");
    }

    #[test]
    fn names_the_source_steps_a_fan_in_would_be_carried_by() {
        let mk = |id: &str, inputs: &[&str]| StepEdges {
            id: id.into(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
        };
        let steps = [
            mk("a/ingest", &[]),
            mk("a/render_markdown", &["a/ingest"]),
            mk("b/ingest", &[]),
            mk("b/render_markdown", &["b/ingest"]),
            mk(
                "unified_index/grid_index",
                &["a/render_markdown", "b/render_markdown"],
            ),
        ];
        assert_eq!(
            sources_feeding(&steps, "unified_index/grid_index"),
            ["a/ingest", "b/ingest"]
        );
        assert_eq!(sources_feeding(&steps, "a/render_markdown"), ["a/ingest"]);
        assert!(sources_feeding(&steps, "a/ingest").is_empty());
    }
}
