//! `GET /api/requests/{id}`: one sync request, and where each step it
//! covers stands for it. An app that reads only the raw stores waits on
//! its roots here rather than on the request, which stays open until the
//! render and index steps after them have run.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use datalib_dag::supervisor::record::{Record, StepRecord};
use datalib_dag::supervisor::store::RequestRow;
use datalib_dag::supervisor::tick::StateKind;
use serde::Serialize;
use strum::{EnumString, IntoStaticStr, VariantArray};

use crate::{
    internal, live_progress, mailbox, supervisor, AppState, DagStepProgress, DagStepRun, Refusal,
    RequestView,
};

/// Where a step stands for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, EnumString, IntoStaticStr, VariantArray)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RequestStepPhase {
    /// It has work left for the request and is not running: waiting its
    /// turn, or the loop has not taken the request on yet.
    Pending,
    Running,
    /// Nothing left for the request: it ran, failed, was up to date, or
    /// is turned off. `state` and `last_run` say which.
    Done,
}

impl RequestStepPhase {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Serialize)]
pub struct RequestStep {
    pub id: String,
    /// One of the steps the request was opened on.
    pub root: bool,
    pub phase: RequestStepPhase,
    /// What the loop's last tick made of it (`StateKind`), whichever
    /// request it was for.
    pub state: Option<&'static str>,
    pub state_detail: Option<String>,
    /// Its newest run. A step done without running for this request keeps
    /// an older one.
    pub last_run: Option<DagStepRun>,
    /// What it has reported in the loop's run in flight, or the one that
    /// finished last.
    pub progress: Option<DagStepProgress>,
}

#[derive(Debug, Serialize)]
pub struct RequestProgress {
    #[serde(flatten)]
    pub request: RequestView,
    /// The roots, then what reads them, in the order they run.
    pub steps: Vec<RequestStep>,
}

/// A step's record names the open requests it has work left in, and the
/// loop closes a request only after saving a record in which no step
/// names it. So until one does, the loop has not taken it on; once one
/// does, a step that does not is done with it. In the moment between
/// that last save and the close, every step reads pending, never done
/// too early.
pub fn phase(request: &RequestRow, record: &Record, step: Option<&StepRecord>) -> RequestStepPhase {
    let names = |s: &StepRecord| s.requests.contains(&request.id);
    if request.closed.is_some() {
        return RequestStepPhase::Done;
    }
    match step.filter(|s| names(s)) {
        Some(s) if s.state == Some(StateKind::Running) => RequestStepPhase::Running,
        Some(_) => RequestStepPhase::Pending,
        None if record.steps.values().any(names) => RequestStepPhase::Done,
        None => RequestStepPhase::Pending,
    }
}

/// The roots, then every step of `graph` downstream of them in
/// topological order. A root the config no longer names is still listed:
/// the loop may be holding the request for a config that has it.
fn scope(graph: &datalib_dag::Graph, roots: &[String]) -> Vec<(String, bool)> {
    let known: Vec<usize> = roots
        .iter()
        .filter_map(|r| graph.by_id.get(r).copied())
        .collect();
    let reached = graph.downstream_of(&known);
    let mut steps: Vec<(String, bool)> = roots
        .iter()
        .filter(|r| !graph.by_id.contains_key(*r))
        .map(|r| (r.clone(), true))
        .collect();
    steps.extend(graph.topo.iter().filter(|&&i| reached[i]).map(|&i| {
        let id = graph.steps[i].id.clone();
        let root = roots.contains(&id);
        (id, root)
    }));
    steps
}

/// `GET /api/requests/{id}` — the request, and each step it covers.
pub async fn get(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RequestProgress>, Refusal> {
    let store = mailbox(&s).await?;
    let Some(request) = store.request(&id).await.map_err(internal)? else {
        return Err((StatusCode::NOT_FOUND, format!("no request {id}")));
    };
    let checked = supervisor::load_config(&s.root).map_err(|e| (StatusCode::CONFLICT, e))?;
    let record = store.load_record().await.map_err(internal)?;
    let mut progress = live_progress(
        &s.root,
        record.current_run.as_ref().map(|r| r.run_id.as_str()),
    )
    .await;
    let steps = scope(&checked.graph, &request.roots)
        .into_iter()
        .map(|(step, root)| {
            let rec = record.steps.get(&step);
            RequestStep {
                root,
                phase: phase(&request, &record, rec),
                state: rec.and_then(|r| r.state).map(StateKind::as_str),
                state_detail: rec.and_then(|r| r.state_detail.clone()),
                last_run: rec.and_then(|r| r.last_run.as_ref()).map(DagStepRun::from),
                progress: progress.remove(&step),
                id: step,
            }
        })
        .collect();
    Ok(Json(RequestProgress {
        request: RequestView::from(request),
        steps,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_dag::supervisor::store::RequestOutcome;

    fn request(closed: Option<Option<RequestOutcome>>) -> RequestRow {
        RequestRow {
            id: "r".into(),
            roots: vec!["a/ingest".into()],
            opened_by: "ui".into(),
            stop_requested_by: None,
            closed,
            failed_step: None,
        }
    }

    fn step(state: StateKind, requests: &[&str]) -> StepRecord {
        StepRecord {
            state: Some(state),
            requests: requests.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        }
    }

    fn record(steps: &[(&str, StepRecord)]) -> Record {
        Record {
            steps: steps
                .iter()
                .map(|(id, s)| (id.to_string(), s.clone()))
                .collect(),
            current_run: None,
        }
    }

    /// The download finished while the index after it still works for the
    /// request: the download is done, the index is not.
    #[test]
    fn a_root_is_done_while_what_reads_it_still_works() {
        let r = request(None);
        let rec = record(&[
            ("a/ingest", step(StateKind::Fresh, &[])),
            ("index/all", step(StateKind::Running, &["r"])),
        ]);
        let of = |id: &str| phase(&r, &rec, rec.steps.get(id));
        assert_eq!(of("a/ingest"), RequestStepPhase::Done);
        assert_eq!(of("index/all"), RequestStepPhase::Running);
    }

    /// Running for another request is not running for this one, and a
    /// request no step names yet has not been taken on.
    #[test]
    fn nothing_reads_done_before_the_loop_takes_the_request_on() {
        let r = request(None);
        let rec = record(&[("a/ingest", step(StateKind::Running, &["other"]))]);
        assert_eq!(
            phase(&r, &rec, rec.steps.get("a/ingest")),
            RequestStepPhase::Pending
        );
        assert_eq!(phase(&r, &rec, None), RequestStepPhase::Pending);
    }

    #[test]
    fn every_step_of_a_closed_request_is_done() {
        let r = request(Some(Some(RequestOutcome::Done)));
        let rec = record(&[("a/ingest", step(StateKind::Running, &["r"]))]);
        assert_eq!(
            phase(&r, &rec, rec.steps.get("a/ingest")),
            RequestStepPhase::Done
        );
    }

    #[test]
    fn phase_names_agree_between_strum_and_serde() {
        for &p in RequestStepPhase::VARIANTS {
            assert_eq!(
                serde_json::to_string(&p).unwrap(),
                format!("\"{}\"", p.as_str())
            );
        }
    }
}
