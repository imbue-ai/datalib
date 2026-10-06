//! `GET /api/manage/groups/{id}/dashboard`: one group's sync as time
//! series, for the dashboard card. For one run — the newest its steps
//! took part in, or the one asked for — each step's state, every metric
//! series it reported (each already a running total, or the `queued`
//! gauge), its warning and error lines counted up over the run, and
//! what its tree weighed; and the group's folder's weight. The rows,
//! controls and status come from `/api/manage/rows`, as for the table.

use axum::extract::{Path, Query, State};
use axum::Json;
use datalib_columns::Sample;
use datalib_runs::RunRow;
use serde::{Deserialize, Serialize};

use crate::AppState;

/// The most points one series carries. A chart a few hundred pixels
/// wide draws no more than this, and a day-long run has thousands.
const MAX_POINTS: usize = 300;

/// How many of the group's runs the picker offers.
const RUNS_OFFERED: i64 = 20;

#[derive(Debug, Deserialize)]
pub struct DashboardParams {
    run: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Series {
    pub name: String,
    /// `k=v` pairs, as the run store keeps them; empty for none.
    pub labels: String,
    pub points: Vec<Sample>,
}

#[derive(Debug, Serialize)]
pub struct StepPanel {
    pub id: String,
    /// Whether the step took part in the run shown. A step that did not
    /// has nothing else set.
    pub in_run: bool,
    pub state: Option<String>,
    pub attempt: Option<i64>,
    pub started_at_utc: Option<String>,
    pub finished_at_utc: Option<String>,
    pub error: Option<String>,
    pub msg: Option<String>,
    pub series: Vec<Series>,
    /// `warn` and `error` lines so far, as running totals.
    pub warnings: Vec<Sample>,
    pub errors: Vec<Sample>,
    /// Bytes under the step's tree over the run.
    pub disk: Vec<Sample>,
}

#[derive(Debug, Serialize)]
pub struct Dashboard {
    pub group: String,
    /// The run shown; `None` when the group's steps have never run.
    pub run: Option<RunRow>,
    /// The shown run is still going.
    pub live: bool,
    /// The group's recent runs, newest first, for the picker.
    pub runs: Vec<RunRow>,
    /// The group's steps in config order.
    pub steps: Vec<StepPanel>,
    /// Bytes under the group's folder over the run.
    pub disk: Vec<Sample>,
}

pub async fn get_dashboard(
    State(s): State<AppState>,
    Path(group): Path<String>,
    Query(p): Query<DashboardParams>,
) -> Json<Dashboard> {
    let text = std::fs::read_to_string(s.config_path()).unwrap_or_default();
    let step_ids: Vec<String> = datalib_dag::written::entries_as_written(&text)
        .map(|w| {
            w.steps
                .into_iter()
                .filter(|st| st.group.as_deref() == Some(group.as_str()))
                .map(|st| st.id)
                .collect()
        })
        .unwrap_or_default();
    let runs = datalib_runs::runs_of_steps(&s.root, &step_ids, RUNS_OFFERED).await;
    let run = match p.run {
        Some(id) => datalib_runs::runs(&s.root, None, i64::MAX)
            .await
            .into_iter()
            .find(|r| r.run_id == id),
        None => runs.first().cloned(),
    };
    let Some(run) = run else {
        return Json(Dashboard {
            group,
            run: None,
            live: false,
            runs,
            steps: step_ids.into_iter().map(absent).collect(),
            disk: Vec::new(),
        });
    };
    let live = run.finished_at_utc.is_none() && s.sync.running();
    let until = run.finished_at_utc.clone().unwrap_or_else(crate::now_utc);
    let snap = datalib_runs::snapshot_of(&s.root, Some(&run.run_id)).await;
    let disk_of = |path: String| {
        let (repo, since, until) = (s.app.clone(), run.started_at_utc.clone(), until.clone());
        async move {
            let rows = repo
                .disk_usage_between(&path, &since, &until)
                .await
                .unwrap_or_default();
            thin(
                rows.into_iter()
                    .map(|r| Sample {
                        at: r.measured_at_utc,
                        value: r.bytes,
                    })
                    .collect(),
            )
        }
    };
    let mut steps = Vec::new();
    for id in step_ids {
        let Some(row) = snap.steps.iter().find(|r| r.step == id) else {
            steps.push(absent(id));
            continue;
        };
        let samples = datalib_runs::step_samples(&s.root, &run.run_id, &id).await;
        let lines = datalib_runs::step_problem_lines(&s.root, &run.run_id, &id).await;
        steps.push(StepPanel {
            in_run: true,
            state: Some(row.state.clone()),
            attempt: Some(row.attempt),
            started_at_utc: row.started_at_utc.clone(),
            finished_at_utc: row.finished_at_utc.clone(),
            error: row.error.clone(),
            msg: row.msg.clone(),
            series: series_of(samples),
            warnings: running_total(&lines, "warn"),
            errors: running_total(&lines, "error"),
            disk: disk_of(id.clone()).await,
            id,
        });
    }
    let disk = disk_of(group.clone()).await;
    Json(Dashboard {
        group,
        run: Some(run),
        live,
        runs,
        steps,
        disk,
    })
}

fn absent(id: String) -> StepPanel {
    StepPanel {
        id,
        in_run: false,
        state: None,
        attempt: None,
        started_at_utc: None,
        finished_at_utc: None,
        error: None,
        msg: None,
        series: Vec::new(),
        warnings: Vec::new(),
        errors: Vec::new(),
        disk: Vec::new(),
    }
}

/// The samples, ordered by series then time, split into one series each.
fn series_of(samples: Vec<datalib_runs::MetricSampleRow>) -> Vec<Series> {
    let mut out: Vec<Series> = Vec::new();
    for m in samples {
        let point = Sample {
            at: m.ts_utc,
            value: m.value,
        };
        match out.last_mut() {
            Some(s) if s.name == m.name && s.labels == m.labels => s.points.push(point),
            _ => out.push(Series {
                name: m.name,
                labels: m.labels,
                points: vec![point],
            }),
        }
    }
    for s in &mut out {
        s.points = thin(std::mem::take(&mut s.points));
    }
    out
}

/// The lines of one level, counted up: the nth line's stamp at n.
fn running_total(lines: &[(String, String)], level: &str) -> Vec<Sample> {
    thin(
        lines
            .iter()
            .filter(|(l, _)| l == level)
            .enumerate()
            .map(|(i, (_, at))| Sample {
                at: at.clone(),
                value: i as i64 + 1,
            })
            .collect(),
    )
}

/// At most [`MAX_POINTS`], keeping the first, the last, and the last of
/// every stride between. Every series here is a step function read by
/// carrying a value forward, so the last of a stretch is the one that
/// stands for it.
fn thin(points: Vec<Sample>) -> Vec<Sample> {
    if points.len() <= MAX_POINTS {
        return points;
    }
    let stride = points.len().div_ceil(MAX_POINTS - 1);
    let last = points.len() - 1;
    points
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i == 0 || *i == last || (i + 1) % stride == 0)
        .map(|(_, p)| p)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pts(n: usize) -> Vec<Sample> {
        (0..n)
            .map(|i| Sample {
                at: format!("{i:06}"),
                value: i as i64,
            })
            .collect()
    }

    #[test]
    fn thinning_keeps_both_ends_and_stays_under_the_cap() {
        assert_eq!(thin(pts(10)).len(), 10);
        let t = thin(pts(10_000));
        assert!(t.len() <= MAX_POINTS, "{}", t.len());
        assert_eq!(t.first().unwrap().value, 0);
        assert_eq!(t.last().unwrap().value, 9_999);
    }

    #[test]
    fn warning_and_error_lines_count_up_separately() {
        let lines: Vec<(String, String)> = [("warn", "a"), ("error", "b"), ("warn", "c")]
            .iter()
            .map(|(l, t)| (l.to_string(), t.to_string()))
            .collect();
        let w = running_total(&lines, "warn");
        assert_eq!(
            w.iter()
                .map(|s| (s.at.as_str(), s.value))
                .collect::<Vec<_>>(),
            [("a", 1), ("c", 2)]
        );
        assert_eq!(running_total(&lines, "error").len(), 1);
    }

    #[test]
    fn samples_split_into_one_series_per_name_and_labels() {
        let m = |name: &str, labels: &str, ts: &str| datalib_runs::MetricSampleRow {
            name: name.into(),
            labels: labels.into(),
            ts_utc: ts.into(),
            value: 1,
            ..Default::default()
        };
        let s = series_of(vec![
            m("rows", "table=a", "1"),
            m("rows", "table=a", "2"),
            m("rows", "table=b", "1"),
            m("queued", "", "1"),
        ]);
        let shape: Vec<(&str, &str, usize)> = s
            .iter()
            .map(|x| (x.name.as_str(), x.labels.as_str(), x.points.len()))
            .collect();
        assert_eq!(
            shape,
            [
                ("rows", "table=a", 2),
                ("rows", "table=b", 1),
                ("queued", "", 1)
            ]
        );
    }
}
