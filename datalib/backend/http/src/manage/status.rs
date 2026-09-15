//! What a Manage row's Status column says, and why. Pure functions over
//! the runner's record, the job queue and the config's edges; the
//! handler in `manage/mod.rs` feeds them one snapshot.

use std::collections::{HashMap, HashSet, VecDeque};

use app_schema::sync_jobs::SyncJobRow;
use datalib_dag::{Diagnostic, Severity};
use serde::Serialize;

use crate::{DagRunInfo, DagStepRun};

/// What the runner's record says about one step.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepRecord {
    pub last_run: Option<DagStepRun>,
    pub current_state: Option<String>,
}

/// The record as it applies to the run now in flight. `stale` is true
/// when the record describes a *previous* run, so its `current_state`
/// is about different work and is dropped. `last_run` is deliberately
/// untouched: that is the row's history, still correct, and blanking it
/// would send the row to "Never run", which ranks below Queued and so
/// is its own way of going backwards.
pub fn step_for_run(step: Option<&StepRecord>, stale: bool) -> Option<StepRecord> {
    let step = step?;
    if !stale {
        return Some(step.clone());
    }
    Some(StepRecord {
        last_run: step.last_run.clone(),
        current_state: None,
    })
}

/// The run a row should be judged against.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveRun {
    pub run_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub live: bool,
    /// True when this run was **not** reported by the runner — it was
    /// inferred from the queue because a job is running and the record
    /// has not caught up. Load-bearing: a synthesized run says the
    /// runner has written nothing about *this* run yet, so every
    /// per-step `current_state` in the record still belongs to the run
    /// before it. `step_for_run` takes this flag and drops that state.
    pub synthesized: bool,
}

impl EffectiveRun {
    fn reported(r: &DagRunInfo) -> Self {
        EffectiveRun {
            run_id: r.run_id.clone(),
            started_at: r.started_at.clone(),
            finished_at: r.finished_at.clone(),
            live: r.live,
            synthesized: false,
        }
    }

    pub fn in_flight(&self) -> bool {
        self.finished_at.is_none()
    }
}

pub fn effective_run(
    fetched: Option<&DagRunInfo>,
    live_job: Option<&SyncJobRow>,
) -> Option<EffectiveRun> {
    let Some(job) = live_job.filter(|j| j.state == "running") else {
        return fetched.map(EffectiveRun::reported);
    };
    // The job's id *is* the run id (the worker passes it as `--run-id`),
    // so a record for this job's run is the real thing even if the
    // queue and the record disagree on whether it has finished.
    if let Some(r) = fetched {
        if r.run_id == job.id || r.finished_at.is_none() {
            return Some(EffectiveRun::reported(r));
        }
    }
    let started = job
        .started_at_utc
        .clone()
        .unwrap_or_else(|| job.created_at_utc.clone());
    Some(EffectiveRun {
        run_id: job.id.clone(),
        started_at: started,
        finished_at: None,
        live: true,
        synthesized: true,
    })
}

/// How far through a run each status is. `config_rejected` and
/// `config_blocked` are deliberately absent: they are not points in a
/// run — they say the entry is not in the pipeline at all, news that
/// has to be able to arrive *mid-run* and move a row backwards from
/// `Succeeded`. "Never run" sits *below* Queued: it is the absence of
/// history, so seeing it after a sync was queued really is going
/// backwards.
pub fn status_rank(key: &str) -> Option<i32> {
    Some(match key {
        "never_run" => -1,
        "queued" => 0,
        "running" => 1,
        "succeeded" | "skipped_up_to_date" | "failed" | "blocked" | "incomplete"
        | "interrupted" => 2,
        _ => return None,
    })
}

/// One row's status, reduced to a vocabulary the Status column can draw.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusView {
    pub key: String,
    pub label: String,
    /// When this status was reached. Feeds the "Last synced" column, so
    /// the two can never disagree about which run they describe.
    pub at: Option<String>,
    pub detail: Option<String>,
}

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
    ("succeeded", "Succeeded"),
    ("skipped_up_to_date", "Up to date"),
    ("failed", "Failed"),
    ("blocked", "Blocked"),
    // Stopped on its budget with work left; the next sync resumes it.
    ("incomplete", "Incomplete"),
    ("interrupted", "Interrupted"),
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
    }
}

/// A floor under a row's status, holding it to the furthest it has got
/// within one run. Keyed by the claiming job, which exists from the
/// enqueue frame; when the job finishes the key changes, the floor
/// lifts, and the next sync is free to start at Queued again.
#[derive(Debug, Default)]
pub struct StatusFloor {
    seen: HashMap<String, (String, StatusView)>,
}

impl StatusFloor {
    pub fn hold(&mut self, id: &str, run: &str, next: StatusView) -> StatusView {
        // A status outside the vocabulary is passed through and
        // forgotten. Holding a row at a rank we cannot compare would be
        // worse than the flicker this exists to stop.
        let Some(next_rank) = status_rank(&next.key) else {
            self.seen.remove(id);
            return next;
        };
        if let Some((prev_run, prev)) = self.seen.get(id) {
            if prev_run == run && status_rank(&prev.key) > Some(next_rank) {
                return prev.clone();
            }
        }
        self.seen
            .insert(id.to_string(), (run.to_string(), next.clone()));
        next
    }
}

/// "a", "a and b", "a, b and c". A tooltip is prose, and a row waiting
/// on two steps should read like a sentence rather than an array.
fn list_of(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
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

/// Has this step already been reached by the run `job` started?
fn reached_since(last: Option<&DagStepRun>, job: &SyncJobRow) -> bool {
    let (Some(last), Some(since)) = (last, job.started_at_utc.as_deref()) else {
        return false;
    };
    let at = last.finished_at.as_deref().unwrap_or(&last.started_at);
    match (instant(at), instant(since)) {
        (Some(at), Some(since)) => at >= since,
        _ => false,
    }
}

/// A step and the ids it reads: the DAG's edges, as far as status needs
/// them. Applets carry no inputs and are not steps.
#[derive(Debug, Clone, PartialEq)]
pub struct StepEdges {
    pub id: String,
    pub inputs: Vec<String>,
}

/// step id → the ids of the steps that name it as an input. The DAG's
/// edges, read the direction the scheduler reads them.
pub fn dependents_of(steps: &[StepEdges]) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for s in steps {
        for input in &s.inputs {
            m.entry(input.clone()).or_default().push(s.id.clone());
        }
    }
    m
}

/// Every step a sync of `seeds` will consider: the seeds plus their
/// transitive dependents.
pub fn closure_of(dependents: &HashMap<String, Vec<String>>, seeds: &[String]) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut queue: VecDeque<String> = seeds.iter().cloned().collect();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(ds) = dependents.get(&id) {
            queue.extend(ds.iter().cloned());
        }
    }
    seen
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

/// The steps a queued step is actually waiting behind: its declared
/// inputs that have not finished in the run now in flight. A step that
/// is *running* has not, and is the most useful thing to name.
pub fn waiting_on(
    steps: &[StepEdges],
    id: &str,
    is_finished: impl Fn(&str) -> bool,
) -> Vec<String> {
    let Some(step) = steps.iter().find(|s| s.id == id) else {
        return Vec::new();
    };
    let mut out: Vec<String> = step
        .inputs
        .iter()
        .filter(|input| !is_finished(input))
        .cloned()
        .collect();
    out.sort();
    out
}

/// The worker splits `source_ids` on commas and passes each as its own
/// `--sync`; empty means the whole config.
pub fn job_seeds(job: &SyncJobRow) -> Vec<String> {
    job.source_ids
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// step id → the queued-or-running job that has claimed it.
pub fn claimed_by<'a>(
    steps: &[StepEdges],
    jobs: &'a [SyncJobRow],
) -> HashMap<String, &'a SyncJobRow> {
    let mut m: HashMap<String, &'a SyncJobRow> = HashMap::new();
    let dependents = dependents_of(steps);
    for job in jobs {
        if job.state != "pending" && job.state != "running" {
            continue;
        }
        let seeds = job_seeds(job);
        let scope: HashSet<String> = if seeds.is_empty() {
            steps.iter().map(|s| s.id.clone()).collect()
        } else {
            closure_of(&dependents, &seeds)
        };
        for id in scope {
            m.entry(id).or_insert(job);
        }
    }
    m
}

pub struct StatusArgs<'a> {
    /// The step being described. Passed separately from `step`, which
    /// is absent until a run has reached this row at least once.
    pub id: &'a str,
    pub step: Option<&'a StepRecord>,
    pub run: Option<&'a EffectiveRun>,
    pub claim: Option<&'a SyncJobRow>,
    /// The steps this one is queued behind, from `waiting_on`. Only read
    /// in the queued branch, where it turns "Queued" into a sentence.
    pub waiting_on: &'a [String],
    /// The loader's reason this entry is not in the pipeline, if it
    /// isn't. Outranks every other source.
    pub dropped: Option<&'a Diagnostic>,
}

/// The sentence a dropped entry's status carries.
pub fn dropped_detail(d: &Diagnostic) -> String {
    match &d.help {
        Some(help) => format!("{} \u{2014} {help}", d.message),
        None => d.message.clone(),
    }
}

/// What a step is doing right now, or did last.
///
/// `not_selected` appears nowhere. It is a fact about a *run* ("this
/// one didn't ask for me"), not about the step; the runner no longer
/// records it as a `last_run`, and `GET /api/dag` drops the ones
/// already on disk.
pub fn step_status(args: StatusArgs<'_>) -> StatusView {
    // A step the config loader dropped is not going to run, whatever
    // the runner's record still remembers. This has to outrank
    // everything else: a row still reading "Up to date" from last
    // week, for an entry no longer in the graph, is exactly the
    // failure #209 is about. `at` is None for the same reason.
    if let Some(d) = args.dropped {
        let key = if d.severity == Severity::Blocked {
            "config_blocked"
        } else {
            "config_rejected"
        };
        return view(key, None, Some(dropped_detail(d)));
    }
    let last = args.step.and_then(|s| s.last_run.as_ref());
    let run_in_flight = args.run.filter(|r| r.in_flight());
    // Only a run still in flight says anything about now. A closed
    // record's states are last run's history, and `last_run` tells it
    // better.
    let current = run_in_flight.and_then(|_| args.step.and_then(|s| s.current_state.as_deref()));

    let died = |at: &str| {
        view(
            "interrupted",
            Some(at.to_string()),
            Some(format!(
                "Started {at} and never finished \u{2014} no runner holds this root now, \
                 so it was killed or crashed."
            )),
        )
    };

    if current == Some("running") {
        let run = run_in_flight.expect("current is read only under a run in flight");
        let at = last
            .map(|l| l.started_at.as_str())
            .unwrap_or(&run.started_at);
        return if run.live {
            view("running", Some(at.to_string()), None)
        } else {
            died(at)
        };
    }

    // Claimed, and the runner hasn't reached it. `current` being set at
    // all means it has — including `not_selected`, which is the runner
    // saying this row is out of scope after all.
    if let Some(claim) = args
        .claim
        .filter(|c| current.is_none() && !reached_since(last, c))
    {
        // "the sync of pdfs/raw" reads badly on pdfs/raw's own row, which
        // is the row most likely to be read. Name the sync only when it
        // is some *other* row's.
        let seeds = job_seeds(claim);
        let sync = match claim.source_ids.as_deref().filter(|s| !s.is_empty()) {
            None => "a sync of everything".to_string(),
            Some(_) if seeds.len() == 1 && seeds[0] == args.id => "this sync".to_string(),
            Some(ids) => format!("the sync of {ids}"),
        };
        // Upstream steps first, because that is the specific answer;
        // the job itself is the fallback.
        let detail = if !args.waiting_on.is_empty() {
            format!(
                "Waiting for {} to finish, in {sync}.",
                list_of(args.waiting_on)
            )
        } else if claim.state == "pending" {
            format!("Waiting for {sync} to start.")
        } else {
            format!("Waiting its turn in {sync}.")
        };
        let at = last.and_then(|l| l.finished_at.clone().or_else(|| Some(l.started_at.clone())));
        return view("queued", at, Some(detail));
    }

    let Some(last) = last else {
        return view("never_run", None, None);
    };
    // An open record with no status is a step that was dispatched and
    // never finished — the run it belonged to is gone by now, or the
    // branch above would have caught it.
    if last.status.is_empty() {
        return died(&last.started_at);
    }
    view(
        &last.status,
        Some(
            last.finished_at
                .clone()
                .unwrap_or_else(|| last.started_at.clone()),
        ),
        last.error.clone(),
    )
}

#[cfg(test)]
mod tests {
    //! The Status column's state machine. The timeline cases replay an
    //! ordered series of snapshots and assert properties of the whole
    //! run — above all that it never goes *backwards*, which is the one
    //! property a snapshot test cannot express.
    use super::*;
    use crate::DagRunInfo;

    /// A two-source graph with a shared fan-in, which is the shape every
    /// real config has.
    fn steps() -> Vec<StepEdges> {
        let mk = |id: &str, inputs: &[&str]| StepEdges {
            id: id.into(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
        };
        vec![
            mk("a/ingest", &[]),
            mk("a/render_markdown", &["a/ingest"]),
            mk("b/ingest", &[]),
            mk("b/render_markdown", &["b/ingest"]),
            mk(
                "unified_index/grid_index",
                &["a/render_markdown", "b/render_markdown"],
            ),
        ]
    }

    const JOB_START: &str = "2026-09-01T10:00:00+02:00";
    const RUN_START: &str = "2026-09-01T10:00:01+02:00";
    const A_DONE: &str = "2026-09-01T10:00:09+02:00";
    const RUN_END: &str = "2026-09-01T10:00:20+02:00";
    const YESTERDAY: &str = "2026-08-31T09:00:00+02:00";

    fn job(state: &str, started: bool) -> SyncJobRow {
        SyncJobRow {
            id: "job-1".into(),
            source_ids: Some("a/ingest".into()),
            kind: "all".into(),
            parent_job_id: None,
            state: state.into(),
            created_at_utc: JOB_START.into(),
            started_at_utc: started.then(|| JOB_START.to_string()),
            finished_at_utc: None,
            tz_offset: None,
            error: None,
            pid: None,
            progress_pct: None,
            progress_msg: None,
        }
    }

    fn last_run(run_id: &str, started: &str, finished: Option<&str>, status: &str) -> DagStepRun {
        DagStepRun {
            run_id: run_id.into(),
            started_at: started.into(),
            finished_at: finished.map(str::to_string),
            status: status.into(),
            attempts: u32::from(finished.is_some()),
            error: None,
        }
    }

    fn rec(current: Option<&str>, last: Option<DagStepRun>) -> StepRecord {
        StepRecord {
            last_run: last,
            current_state: current.map(str::to_string),
        }
    }

    fn run(run_id: &str, started: &str, finished: Option<&str>, live: bool) -> DagRunInfo {
        DagRunInfo {
            run_id: run_id.into(),
            started_at: started.into(),
            finished_at: finished.map(str::to_string),
            live,
        }
    }

    fn live_run() -> DagRunInfo {
        run(RUN_START, RUN_START, None, true)
    }

    /// One frame of what the grid holds: the queue and the runner's
    /// record, exactly the two things it reads.
    struct Frame {
        jobs: Vec<SyncJobRow>,
        run: Option<DagRunInfo>,
        dag: HashMap<String, StepRecord>,
    }

    fn status_in(frame: &Frame, id: &str) -> StatusView {
        let run = frame.run.as_ref().map(EffectiveRun::reported);
        let claims = claimed_by(&steps(), &frame.jobs);
        step_status(StatusArgs {
            id,
            step: frame.dag.get(id),
            run: run.as_ref(),
            claim: claims.get(id).copied(),
            waiting_on: &[],
            dropped: None,
        })
    }

    fn dag(entries: &[(&str, StepRecord)]) -> HashMap<String, StepRecord> {
        entries
            .iter()
            .map(|(id, r)| (id.to_string(), r.clone()))
            .collect()
    }

    #[test]
    fn a_step_nothing_has_ever_touched_has_never_run() {
        let f = Frame {
            jobs: vec![],
            run: None,
            dag: dag(&[]),
        };
        let s = status_in(&f, "a/ingest");
        assert_eq!(s.key, "never_run");
        assert_eq!(s.at, None);
    }

    /// Nothing is running yet — the worker has not even claimed the
    /// job. This is the window that used to show nothing at all.
    #[test]
    fn a_pending_job_queues_the_step_it_names_and_everything_downstream() {
        let jobs = [job("pending", false)];
        let claims = claimed_by(&steps(), &jobs);
        let mut keys: Vec<&String> = claims.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["a/ingest", "a/render_markdown", "unified_index/grid_index"]
        );
        assert!(!claims.contains_key("b/ingest"));
        assert!(!claims.contains_key("b/render_markdown"));
    }

    #[test]
    fn a_job_naming_no_source_claims_every_step() {
        let mut j = job("running", true);
        j.source_ids = None;
        let jobs = [j];
        assert_eq!(claimed_by(&steps(), &jobs).len(), 5);
    }

    #[test]
    fn a_run_in_flight_with_a_dead_runner_reads_as_interrupted() {
        let f = Frame {
            jobs: vec![],
            run: Some(run(RUN_START, RUN_START, None, false)),
            dag: dag(&[("a/ingest", rec(Some("running"), None))]),
        };
        let s = status_in(&f, "a/ingest");
        assert_eq!(s.key, "interrupted");
        assert!(s.detail.unwrap().contains("killed or crashed"));
    }

    /// The runner walks every step to publish output versions, so a
    /// subset sync reaches this one and reports it out of scope. That
    /// is a fact about the run; the row goes on showing the step's own
    /// history.
    #[test]
    fn a_not_selected_current_state_falls_through_to_what_the_step_last_did() {
        let f = Frame {
            jobs: vec![],
            run: Some(live_run()),
            dag: dag(&[(
                "b/ingest",
                rec(
                    Some("not_selected"),
                    Some(last_run("r", YESTERDAY, Some(YESTERDAY), "succeeded")),
                ),
            )]),
        };
        let s = status_in(&f, "b/ingest");
        assert_eq!(s.key, "succeeded");
        assert_eq!(s.at.as_deref(), Some(YESTERDAY));
    }

    #[test]
    fn names_the_source_steps_a_fan_in_would_be_carried_by() {
        assert_eq!(
            sources_feeding(&steps(), "unified_index/grid_index"),
            ["a/ingest", "b/ingest"]
        );
        assert_eq!(sources_feeding(&steps(), "a/render_markdown"), ["a/ingest"]);
        // A source step is not fed by anything, including itself.
        assert!(sources_feeding(&steps(), "a/ingest").is_empty());
    }

    /// The sequence of frames a grid really sees across one "Sync
    /// a/ingest", including the two places the queue and the runner's
    /// record disagree.
    fn timeline() -> Vec<(&'static str, Frame)> {
        let done = || {
            rec(
                Some("succeeded"),
                Some(last_run("r", RUN_START, Some(A_DONE), "succeeded")),
            )
        };
        let mut finished_job = job("running", true);
        finished_job.state = "done".into();
        finished_job.finished_at_utc = Some(RUN_END.into());
        vec![
            (
                "clicked: the job exists, the worker has not claimed it, and the record is still last run's",
                Frame {
                    jobs: vec![job("pending", false)],
                    run: Some(run("older", YESTERDAY, Some(YESTERDAY), false)),
                    dag: dag(&[]),
                },
            ),
            (
                "worker claimed the job; the runner has opened a record but reached nothing",
                Frame {
                    jobs: vec![job("running", true)],
                    run: Some(live_run()),
                    dag: dag(&[]),
                },
            ),
            (
                "the step is dispatched and running",
                Frame {
                    jobs: vec![job("running", true)],
                    run: Some(live_run()),
                    dag: dag(&[(
                        "a/ingest",
                        rec(Some("running"), Some(last_run("r", RUN_START, None, ""))),
                    )]),
                },
            ),
            (
                "the step finished, the run is still going",
                Frame {
                    jobs: vec![job("running", true)],
                    run: Some(live_run()),
                    dag: dag(&[("a/ingest", done())]),
                },
            ),
            (
                "the run closed, but the queue is one poll behind and still says running",
                Frame {
                    jobs: vec![job("running", true)],
                    run: Some(run(RUN_START, RUN_START, Some(RUN_END), false)),
                    dag: dag(&[("a/ingest", done())]),
                },
            ),
            (
                "settled: the queue has caught up",
                Frame {
                    jobs: vec![finished_job],
                    run: Some(run(RUN_START, RUN_START, Some(RUN_END), false)),
                    dag: dag(&[("a/ingest", done())]),
                },
            ),
        ]
    }

    #[test]
    fn the_sequence_a_sync_actually_produces() {
        let seen: Vec<(&str, StatusView)> = timeline()
            .iter()
            .map(|(note, f)| (*note, status_in(f, "a/ingest")))
            .collect();
        // Something is happening from the very first frame.
        assert_eq!(seen[0].1.key, "queued");
        assert_eq!(seen[1].1.key, "queued");
        assert_eq!(seen[2].1.key, "running");
        let last = &seen[seen.len() - 1].1;
        assert_eq!(last.key, "succeeded");
        assert_eq!(last.at.as_deref(), Some(A_DONE));
        // Never backwards — including frame 4, where reading the queue
        // alone would send a finished step back to "Queued".
        for w in seen.windows(2) {
            assert!(
                status_rank(&w[1].1.key) >= status_rank(&w[0].1.key),
                "went backwards at ({}): {} -> {}",
                w[1].0,
                w[0].1.key,
                w[1].1.key
            );
        }
    }

    /// The Stop button's face is `claimed_by`, and it must not linger
    /// once the queue settles, or the row can never be run again.
    #[test]
    fn holds_the_stop_button_for_exactly_as_long_as_the_job_is_live() {
        let claimed: Vec<bool> = timeline()
            .iter()
            .map(|(_, f)| claimed_by(&steps(), &f.jobs).contains_key("a/ingest"))
            .collect();
        assert_eq!(claimed, [true, true, true, true, true, false]);
    }

    /// b's rows are outside this sync, and nothing in the sequence may
    /// give them a status or a timestamp.
    #[test]
    fn leaves_the_other_sources_chain_alone_throughout() {
        for (note, f) in timeline() {
            let s = status_in(&f, "b/ingest");
            assert_eq!(s.key, "never_run", "b/ingest changed at: {note}");
            assert_eq!(s.at, None, "b/ingest got a timestamp at: {note}");
        }
    }

    /// The live sequence: a job frame arrives, then the runner's record
    /// catches up. The job's id is the run id, which is what lets the
    /// server tell "the record is about this run" from "still about the
    /// last".
    mod live_sequence {
        use super::*;

        fn stale_poll() -> DagRunInfo {
            run("yesterday", YESTERDAY, Some(YESTERDAY), false)
        }
        fn this_run() -> DagRunInfo {
            run("job-1", RUN_START, None, true)
        }

        /// Compose one frame the way the handler does, then read the row.
        fn read(state: &str, fetched: &DagRunInfo, step: Option<&StepRecord>) -> StatusView {
            let active = state == "pending" || state == "running";
            let j = job(state, active);
            let run = effective_run(Some(fetched), (state == "running").then_some(&j));
            let jobs = [j.clone()];
            let claims = claimed_by(&steps(), &jobs);
            let step = step_for_run(step, run.as_ref().is_some_and(|r| r.synthesized));
            step_status(StatusArgs {
                id: "a/ingest",
                step: step.as_ref(),
                run: run.as_ref(),
                claim: claims.get("a/ingest").copied(),
                waiting_on: &[],
                dropped: None,
            })
        }

        #[test]
        fn says_queued_from_the_enqueue_frame_before_any_runner_exists() {
            assert_eq!(read("pending", &stale_poll(), None).key, "queued");
        }

        /// The stale record says Succeeded. That is yesterday's answer,
        /// and `step_for_run` drops it.
        #[test]
        fn says_queued_while_the_record_is_still_about_yesterday() {
            let s = rec(Some("succeeded"), None);
            assert_eq!(read("running", &stale_poll(), Some(&s)).key, "queued");
        }

        #[test]
        fn says_running_once_the_record_for_this_run_lands() {
            let s = rec(Some("running"), None);
            assert_eq!(read("running", &this_run(), Some(&s)).key, "running");
        }

        /// The record for this run is the truth even while the queue
        /// row still says the job is running after the runner closed it.
        #[test]
        fn recognises_the_record_by_the_jobs_id_whatever_the_queue_says() {
            let closed = run("job-1", RUN_START, Some(RUN_END), false);
            let got = effective_run(Some(&closed), Some(&job("running", true))).unwrap();
            assert_eq!(got, EffectiveRun::reported(&closed));
            assert!(!got.synthesized);
        }

        #[test]
        fn never_goes_backwards_across_the_live_sequence() {
            let running = rec(Some("running"), None);
            let seq: Vec<String> = [
                read("pending", &stale_poll(), None),
                read(
                    "running",
                    &stale_poll(),
                    Some(&rec(Some("succeeded"), None)),
                ),
                read("running", &this_run(), Some(&running)),
                read("running", &this_run(), Some(&running)),
            ]
            .into_iter()
            .map(|s| s.key)
            .collect();
            assert_eq!(seq, ["queued", "queued", "running", "running"]);
        }

        /// `effective_run` may only override a stale record on evidence.
        #[test]
        fn does_not_invent_a_live_run_when_nothing_is_running() {
            let stale = stale_poll();
            let expected = Some(EffectiveRun::reported(&stale));
            assert_eq!(effective_run(Some(&stale), None), expected);
            assert_eq!(
                effective_run(Some(&stale), Some(&job("pending", false))),
                expected
            );
        }

        #[test]
        fn keeps_last_run_while_dropping_a_stale_current_state() {
            let step = rec(
                Some("succeeded"),
                Some(last_run(
                    "yesterday",
                    YESTERDAY,
                    Some(YESTERDAY),
                    "succeeded",
                )),
            );
            let fresh = step_for_run(Some(&step), true).unwrap();
            assert_eq!(fresh.current_state, None);
            assert_eq!(fresh.last_run.as_ref().unwrap().status, "succeeded");
            assert_eq!(step_for_run(Some(&step), false).unwrap(), step);
        }
    }

    /// "Queued" on its own says a row will run without saying what it
    /// is behind — and a render step waiting on its download is a
    /// different situation from a download waiting for the worker.
    mod queued_detail {
        use super::*;

        #[test]
        fn names_the_direct_inputs_that_have_not_finished() {
            let none = |_: &str| false;
            assert_eq!(
                waiting_on(&steps(), "unified_index/grid_index", none),
                ["a/render_markdown", "b/render_markdown"]
            );
            // Only the direct ones; the transitive set is the rest of
            // the pipeline.
            assert_eq!(
                waiting_on(&steps(), "a/render_markdown", none),
                ["a/ingest"]
            );
        }

        #[test]
        fn drops_inputs_that_already_finished_this_run() {
            let done = |id: &str| id == "a/render_markdown";
            assert_eq!(
                waiting_on(&steps(), "unified_index/grid_index", done),
                ["b/render_markdown"]
            );
            assert!(waiting_on(&steps(), "unified_index/grid_index", |_| true).is_empty());
        }

        #[test]
        fn a_source_step_waits_on_nothing_upstream() {
            assert!(waiting_on(&steps(), "a/ingest", |_| false).is_empty());
        }

        fn detail(id: &str, blockers: &[&str], state: &str) -> String {
            let j = job(state, state != "pending");
            let jobs = [j];
            let claims = claimed_by(&steps(), &jobs);
            let blockers: Vec<String> = blockers.iter().map(|s| s.to_string()).collect();
            step_status(StatusArgs {
                id,
                step: None,
                run: None,
                claim: claims.get(id).copied(),
                waiting_on: &blockers,
                dropped: None,
            })
            .detail
            .unwrap()
        }

        #[test]
        fn says_what_it_is_behind_when_something_upstream_is_outstanding() {
            assert_eq!(
                detail("a/render_markdown", &["a/ingest"], "running"),
                "Waiting for a/ingest to finish, in the sync of a/ingest."
            );
        }

        #[test]
        fn reads_as_a_sentence_with_more_than_one_blocker() {
            assert_eq!(
                detail(
                    "unified_index/grid_index",
                    &["a/render_markdown", "b/render_markdown"],
                    "running"
                ),
                "Waiting for a/render_markdown and b/render_markdown to finish, in the sync of a/ingest."
            );
        }

        /// On `a/ingest`'s own row "the sync of a/ingest" says nothing —
        /// it *is* that row. Named only when it is someone else's.
        #[test]
        fn distinguishes_a_job_not_yet_started_from_one_already_going() {
            assert_eq!(
                detail("a/ingest", &[], "pending"),
                "Waiting for this sync to start."
            );
            assert_eq!(
                detail("a/ingest", &[], "running"),
                "Waiting its turn in this sync."
            );
        }
    }

    /// The frame `manager2-sync.spec.ts` caught intermittently as
    /// `went backwards: ["Queued","Succeeded","Running","Succeeded"]`.
    mod second_sync {
        use super::*;

        fn previous() -> DagRunInfo {
            run(YESTERDAY, YESTERDAY, Some(YESTERDAY), false)
        }
        fn this() -> DagRunInfo {
            run("job-1", RUN_START, None, true)
        }
        fn already_succeeded() -> StepRecord {
            rec(
                Some("succeeded"),
                Some(last_run(YESTERDAY, YESTERDAY, Some(YESTERDAY), "succeeded")),
            )
        }

        /// The handler's own composition, per row: pick the run to
        /// judge against, then drop what the record says about a run it
        /// has not caught up with.
        fn paint(jobs: &[SyncJobRow], fetched: &DagRunInfo, step: &StepRecord) -> StatusView {
            let live = jobs.iter().find(|j| j.state == "running");
            let run = effective_run(Some(fetched), live);
            let step = step_for_run(Some(step), run.as_ref().is_some_and(|r| r.synthesized));
            let claims = claimed_by(&steps(), jobs);
            step_status(StatusArgs {
                id: "a/ingest",
                step: step.as_ref(),
                run: run.as_ref(),
                claim: claims.get("a/ingest").copied(),
                waiting_on: &[],
                dropped: None,
            })
        }

        #[test]
        fn is_queued_the_moment_the_job_is() {
            let got = paint(&[job("pending", false)], &previous(), &already_succeeded());
            assert_eq!(got.key, "queued");
        }

        /// The failing frame: the job is running; the record has not
        /// been rewritten yet, so everything it says is about yesterday.
        #[test]
        fn stays_queued_once_the_worker_starts_it_before_the_record_catches_up() {
            let got = paint(&[job("running", true)], &previous(), &already_succeeded());
            assert_eq!(got.key, "queued");
        }

        #[test]
        fn reaches_running_when_the_record_for_this_run_lands() {
            let mut running = already_succeeded();
            running.current_state = Some("running".into());
            assert_eq!(
                paint(&[job("running", true)], &this(), &running).key,
                "running"
            );
        }

        #[test]
        fn never_goes_backwards_across_the_whole_re_sync() {
            let mut running = already_succeeded();
            running.current_state = Some("running".into());
            let landed = rec(
                Some("succeeded"),
                Some(last_run("job-1", RUN_START, Some(A_DONE), "succeeded")),
            );
            let closed = EffectiveRun {
                finished_at: Some(RUN_END.into()),
                live: false,
                ..EffectiveRun::reported(&this())
            };
            let seen: Vec<String> = vec![
                paint(&[job("pending", false)], &previous(), &already_succeeded()),
                paint(&[job("running", true)], &previous(), &already_succeeded()),
                paint(&[job("running", true)], &this(), &running),
                step_status(StatusArgs {
                    id: "a/ingest",
                    step: Some(&landed),
                    run: Some(&closed),
                    claim: None,
                    waiting_on: &[],
                    dropped: None,
                }),
            ]
            .into_iter()
            .map(|s| s.key)
            .collect();
            for w in seen.windows(2) {
                assert!(
                    status_rank(&w[1]) >= status_rank(&w[0]),
                    "went backwards: {seen:?}"
                );
            }
            assert_eq!(seen.last().unwrap(), "succeeded");
        }
    }

    /// `step_status` describes one snapshot, and the snapshot is
    /// genuinely ambiguous when neither the queue nor the record says
    /// anything about this step in the run now in flight. The floor is
    /// the missing input.
    mod floor {
        use super::*;

        fn v(key: &str) -> StatusView {
            StatusView {
                key: key.into(),
                label: key.into(),
                at: None,
                detail: None,
            }
        }

        #[test]
        fn holds_a_row_that_has_been_running_against_a_lapse_back_to_queued() {
            let mut hold = StatusFloor::default();
            assert_eq!(hold.hold("a/ingest", "job-1", v("queued")).key, "queued");
            assert_eq!(hold.hold("a/ingest", "job-1", v("running")).key, "running");
            // The frame the e2e caught: claim still running, no
            // `current_state` from either source.
            assert_eq!(hold.hold("a/ingest", "job-1", v("queued")).key, "running");
            assert_eq!(
                hold.hold("a/ingest", "job-1", v("succeeded")).key,
                "succeeded"
            );
        }

        /// A new job is a new key, so the floor lifts. Without this a
        /// row could never be seen queued twice.
        #[test]
        fn lets_the_next_sync_of_the_same_row_start_at_queued_again() {
            let mut hold = StatusFloor::default();
            hold.hold("a/ingest", "job-1", v("running"));
            hold.hold("a/ingest", "job-1", v("succeeded"));
            assert_eq!(hold.hold("a/ingest", "job-2", v("queued")).key, "queued");
        }

        #[test]
        fn keeps_rows_apart() {
            let mut hold = StatusFloor::default();
            hold.hold("a/ingest", "job-1", v("running"));
            assert_eq!(hold.hold("b/ingest", "job-1", v("queued")).key, "queued");
        }

        /// The tooltip and the timestamp travel with the status, or a
        /// held frame would describe itself with the wrong sentence.
        #[test]
        fn returns_the_held_view_whole_not_just_its_rank() {
            let mut hold = StatusFloor::default();
            let running = StatusView {
                key: "running".into(),
                label: "Running".into(),
                at: Some(RUN_START.into()),
                detail: Some("downloading 3/10".into()),
            };
            hold.hold("a/ingest", "job-1", running.clone());
            assert_eq!(hold.hold("a/ingest", "job-1", v("queued")), running);
        }

        /// A vocabulary this table has not met is exactly when holding
        /// a row would be worst: we would be pinning it at a rank we
        /// invented.
        #[test]
        fn passes_through_a_status_it_cannot_rank_rather_than_freezing_on_it() {
            let mut hold = StatusFloor::default();
            hold.hold("a/ingest", "job-1", v("running"));
            assert_eq!(
                hold.hold("a/ingest", "job-1", v("something_new")).key,
                "something_new"
            );
            assert_eq!(hold.hold("a/ingest", "job-1", v("queued")).key, "queued");
        }

        /// Total over the run vocabulary on purpose: an unranked run
        /// status silently opts out of the floor. The two config
        /// statuses must stay unranked — a rank would let the floor pin
        /// a row at a status describing a config that no longer exists.
        #[test]
        fn ranks_every_run_status_and_deliberately_ranks_neither_config_status() {
            for (key, _) in STATUS_LABELS {
                let is_config = *key == "config_rejected" || *key == "config_blocked";
                assert_eq!(status_rank(key).is_none(), is_config, "{key}");
            }
        }

        /// A config edit is the case where going backwards is the
        /// truth: the entry left the pipeline, so last run's `Succeeded`
        /// is no longer a fact about it.
        #[test]
        fn lets_a_dropped_entry_override_a_status_it_already_showed() {
            let mut hold = StatusFloor::default();
            hold.hold("a/ingest", "job-1", v("succeeded"));
            assert_eq!(
                hold.hold("a/ingest", "job-1", v("config_rejected")).key,
                "config_rejected"
            );
            // …and the row's memory is cleared with it.
            assert_eq!(hold.hold("a/ingest", "job-1", v("queued")).key, "queued");
        }
    }

    mod dropped_entry {
        use super::*;
        use datalib_dag::EntryRef;

        fn diag(severity: Severity, message: &str, help: Option<&str>) -> Diagnostic {
            let mut d = Diagnostic {
                severity,
                entry: Some(EntryRef::step_id("slack/ingest")),
                message: message.into(),
                help: None,
                span: None,
                line: Some(7),
                column: Some(1),
            };
            d.help = help.map(str::to_string);
            d
        }

        fn healthy() -> StepRecord {
            rec(
                None,
                Some(last_run(
                    "r1",
                    "2026-09-01T10:00:00+00:00",
                    Some("2026-09-01T10:01:00+00:00"),
                    "succeeded",
                )),
            )
        }

        fn status(step: Option<&StepRecord>, dropped: Option<&Diagnostic>) -> StatusView {
            step_status(StatusArgs {
                id: "slack/ingest",
                step,
                run: None,
                claim: None,
                waiting_on: &[],
                dropped,
            })
        }

        /// The failure #209 is about: a row that reads "Up to date"
        /// from a run taken under a config that no longer loads this
        /// step.
        #[test]
        fn outranks_whatever_the_runners_record_still_remembers() {
            let step = healthy();
            assert_eq!(status(Some(&step), None).key, "succeeded");
            let d = diag(Severity::Rejected, "unknown field `title`", None);
            let now = status(Some(&step), Some(&d));
            assert_eq!(now.key, "config_rejected");
            assert!(now.detail.unwrap().contains("title"));
            // No timestamp: it would describe a run this config never
            // took part in.
            assert_eq!(now.at, None);
        }

        /// `blocked` and `rejected` drop the entry alike and say
        /// different things — the fix for one is at this entry, for the
        /// other it is somewhere else.
        #[test]
        fn distinguishes_a_broken_entry_from_one_broken_by_another() {
            let d = diag(
                Severity::Blocked,
                "input \"slack/x\" names no declared step",
                Some("fix that entry"),
            );
            let got = status(None, Some(&d));
            assert_eq!(got.key, "config_blocked");
            assert!(got.detail.unwrap().contains("fix that entry"));
        }

        #[test]
        fn is_not_triggered_by_a_status_the_loader_still_ran() {
            assert_eq!(status(None, None).key, "never_run");
        }
    }
}
