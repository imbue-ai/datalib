//! What the app calls a run: one request, or one job, with the
//! invocations started for it; or one run an older build recorded per
//! busy period of the loop, shown as it was recorded. Requests and jobs
//! are rows of `system/supervisor.sqlite`; what their invocations did is
//! in the run store.

use std::path::Path;

use datalib_dag::supervisor::host::{job_reason, request_reason};
use datalib_dag::supervisor::store::{JobRow, RequestRow, Store};
use serde::Serialize;

/// One entry of the Runs list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunView {
    /// The request's or job's id, or the old run's.
    pub run_id: String,
    /// UTC.
    pub started_at_utc: String,
    /// UTC; `None` while it is open.
    pub finished_at_utc: Option<String>,
    pub tz_offset: Option<String>,
    /// `sync` for a request; the job's kind (`reset`, `compact`, …) for a
    /// job; an old run's own word (`sync`, `migrate`, `shrink`).
    pub kind: Option<String>,
    /// A sentence for a person: who asked, and for what.
    pub reason: Option<String>,
    /// Who opened it; `None` for an old run.
    pub by: Option<String>,
    /// How a closed request ended (`done`, `failed`, `stopped`), or a
    /// closed job (`done`, `failed`); `None` while open, and for an old
    /// run.
    pub outcome: Option<String>,
    /// Whether it is a run an older build recorded, which has steps but
    /// no invocations.
    pub old: bool,
}

/// The request word a run of the app's is a sync for.
const SYNC: &str = "sync";

fn utc(stamp: &str) -> (String, Option<String>) {
    let split = datalib_time::split_stamp(stamp);
    (split.utc, split.tz_offset)
}

impl From<&RequestRow> for RunView {
    fn from(r: &RequestRow) -> Self {
        let (started_at_utc, tz_offset) = utc(&r.opened_at);
        RunView {
            run_id: r.id.clone(),
            started_at_utc,
            finished_at_utc: r.closed_at.as_deref().map(|c| utc(c).0),
            tz_offset,
            kind: Some(SYNC.into()),
            reason: Some(request_reason(r)),
            by: Some(r.opened_by.clone()),
            outcome: r
                .closed
                .map(|o| o.map_or("closed", |o| o.as_str()).to_string()),
            old: false,
        }
    }
}

impl From<&JobRow> for RunView {
    fn from(j: &JobRow) -> Self {
        let (started_at_utc, tz_offset) = utc(&j.opened_at);
        RunView {
            run_id: j.id.clone(),
            started_at_utc,
            finished_at_utc: j.closed_at.as_deref().map(|c| utc(c).0),
            tz_offset,
            kind: j.kind.map(|k| k.as_str().to_string()),
            reason: Some(job_reason(j)),
            by: Some(j.opened_by.clone()),
            outcome: j
                .closed
                .as_ref()
                .map(|e| if e.is_some() { "failed" } else { "done" }.to_string()),
            old: false,
        }
    }
}

impl From<datalib_runs::RunRow> for RunView {
    fn from(r: datalib_runs::RunRow) -> Self {
        RunView {
            run_id: r.run_id,
            started_at_utc: r.started_at_utc,
            finished_at_utc: r.finished_at_utc,
            tz_offset: r.tz_offset,
            kind: r.kind,
            reason: r.reason,
            by: None,
            outcome: None,
            old: true,
        }
    }
}

/// The request or job `id` names, or the old run; `None` for an id none
/// of them has.
pub async fn one(root: &Path, store: &Store, id: &str) -> anyhow::Result<Option<RunView>> {
    if let Some(r) = store.request(id).await? {
        return Ok(Some(RunView::from(&r)));
    }
    if let Some(j) = store.job(id).await? {
        return Ok(Some(RunView::from(&j)));
    }
    Ok(datalib_runs::runs(root, None, i64::MAX)
        .await?
        .into_iter()
        .find(|r| r.run_id == id)
        .map(RunView::from))
}

/// The newest runs, newest first, at most `limit`: every request and job
/// when `steps` is `None`; else those an invocation of one of `steps`
/// served, and the old runs one of them took part in.
pub async fn list(
    root: &Path,
    store: &Store,
    steps: Option<&[String]>,
    limit: i64,
) -> anyhow::Result<Vec<RunView>> {
    let mut out: Vec<RunView> = Vec::new();
    match steps {
        None => {
            let n = u32::try_from(limit).unwrap_or(u32::MAX);
            out.extend(store.recent_requests(n).await?.iter().map(RunView::from));
            out.extend(store.recent_jobs(n).await?.iter().map(RunView::from));
            out.extend(
                datalib_runs::runs(root, None, limit)
                    .await?
                    .into_iter()
                    .map(RunView::from),
            );
        }
        Some(steps) => {
            for (id, _) in datalib_runs::requests_of_steps(root, Some(steps), limit).await? {
                if let Some(view) = one_new(store, &id).await? {
                    out.push(view);
                }
            }
            out.extend(
                datalib_runs::runs_of_steps(root, steps, limit)
                    .await?
                    .into_iter()
                    .map(RunView::from),
            );
        }
    }
    out.sort_by(|a, b| {
        b.started_at_utc
            .cmp(&a.started_at_utc)
            .then_with(|| b.run_id.cmp(&a.run_id))
    });
    out.truncate(limit.max(0) as usize);
    Ok(out)
}

async fn one_new(store: &Store, id: &str) -> anyhow::Result<Option<RunView>> {
    if let Some(r) = store.request(id).await? {
        return Ok(Some(RunView::from(&r)));
    }
    Ok(store.job(id).await?.as_ref().map(RunView::from))
}
