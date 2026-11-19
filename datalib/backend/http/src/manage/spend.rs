//! The Modal spend cell: what a step's Modal Sandboxes cost at list
//! price, summed over the runs the run store keeps, with the total
//! climbing run by run behind it. Only an `embed` step that sent chunks
//! to Modal reports it (`datalib_metrics::MODAL_COST`); every other row
//! is blank, and the column hides while every row is.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use datalib_columns::{Sample, Timeseries};
use datalib_runs::MetricRow;

use super::queue::grouped;

/// How far back the sparkline reaches: a week of runs. The figure itself
/// covers every run the store keeps, which may be fewer.
pub const WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The unit the UI formats as dollars.
pub const UNIT: &str = "micro_usd";

/// One run's Modal figures for one step, summed over GPUs.
#[derive(Default)]
struct Run {
    run_id: String,
    at: String,
    micro_usd: i64,
    sandboxes: i64,
    seconds: i64,
    gpus: Vec<String>,
}

/// `step id → its Modal spend cell`, from every run's value of
/// [`datalib_metrics::MODAL_COST`], [`datalib_metrics::MODAL_SANDBOXES`]
/// and [`datalib_metrics::MODAL_SANDBOX_SECONDS`] (each a running total
/// within its run, so its value is what the run came to). A step that
/// never sent anything to Modal is absent, so its cell is blank.
pub fn by_step(
    cost: &[MetricRow],
    sandboxes: &[MetricRow],
    seconds: &[MetricRow],
) -> HashMap<String, Timeseries> {
    let mut runs: BTreeMap<&str, Vec<Run>> = BTreeMap::new();
    for m in cost {
        let step = runs.entry(m.step.as_str()).or_default();
        let run = match step.iter_mut().find(|r| r.run_id == m.run_id) {
            Some(r) => r,
            None => {
                step.push(Run {
                    run_id: m.run_id.clone(),
                    ..Default::default()
                });
                step.last_mut().unwrap()
            }
        };
        run.micro_usd += m.value;
        run.at = run.at.clone().max(m.updated_at_utc.clone());
        if let Some(gpu) = m.labels.strip_prefix("gpu=") {
            if !run.gpus.iter().any(|g| g == gpu) {
                run.gpus.push(gpu.to_string());
            }
        }
    }
    for (rows, add) in [
        (
            sandboxes,
            (|r: &mut Run, v| r.sandboxes += v) as fn(&mut Run, i64),
        ),
        (seconds, |r: &mut Run, v| r.seconds += v),
    ] {
        for m in rows {
            if let Some(run) = runs
                .get_mut(m.step.as_str())
                .and_then(|runs| runs.iter_mut().find(|r| r.run_id == m.run_id))
            {
                add(run, m.value);
            }
        }
    }
    runs.into_iter()
        .map(|(step, mut runs)| {
            runs.sort_by(|a, b| a.at.cmp(&b.at));
            let mut total = 0;
            let samples = runs
                .iter()
                .map(|r| {
                    total += r.micro_usd;
                    Sample {
                        at: r.at.clone(),
                        value: total,
                    }
                })
                .collect();
            let cell = Timeseries {
                value: Some(total),
                unit: UNIT.into(),
                samples,
                window_secs: WINDOW.as_secs(),
                detail: Some(detail(total, &runs)),
            };
            (step.to_string(), cell)
        })
        .collect()
}

fn detail(total: i64, runs: &[Run]) -> String {
    let n = runs.len();
    let mut out = format!(
        "about {} at Modal's list price over the {} the run store keeps{}",
        dollars(total),
        if n == 1 {
            "1 run".to_string()
        } else {
            format!("{n} runs")
        },
        runs.first()
            .map(|r| format!(", since {}", short_day(&r.at)))
            .unwrap_or_default(),
    );
    if let Some(last) = runs.last() {
        let gpu = match last.gpus.as_slice() {
            [one] => format!(" {one}"),
            _ => String::new(),
        };
        let s = if last.sandboxes == 1 { "" } else { "es" };
        out.push_str(&format!(
            ". The latest: {}, {}{gpu} Sandbox{s}, {} of Sandbox time",
            dollars(last.micro_usd),
            grouped(last.sandboxes),
            hours(last.seconds),
        ));
    }
    out.push_str(". An estimate that errs high: each Sandbox is priced as if it used a core and 2 GiB beside its GPU.");
    out
}

/// Millionths of a dollar as dollars, to the cent, or past it below one.
fn dollars(micro: i64) -> String {
    let d = micro as f64 / 1e6;
    if d > 0.0 && d < 0.01 {
        format!("${d:.4}")
    } else {
        format!("${d:.2}")
    }
}

fn hours(secs: i64) -> String {
    match secs {
        0..=99 => format!("{secs} s"),
        100..=5999 => format!("{} min", secs / 60),
        _ => format!("{} h {} min", secs / 3600, secs / 60 % 60),
    }
}

/// `2026-11-18T17:54:22+00:00` → `2026-11-18`.
fn short_day(at: &str) -> &str {
    at.get(..10).unwrap_or(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(run: &str, step: &str, name: &str, labels: &str, at: &str, value: i64) -> MetricRow {
        MetricRow {
            run_id: run.into(),
            step: step.into(),
            name: name.into(),
            labels: labels.into(),
            value,
            updated_at_utc: at.into(),
            ..Default::default()
        }
    }

    const EMBED: &str = "ten-forward/embed";

    /// Each run's spend adds to the last, oldest first, whichever GPUs
    /// it ran on; the hover names the latest run's Sandboxes; a step that
    /// never used Modal has no cell.
    #[test]
    fn spend_adds_up_run_by_run_and_names_the_latest() {
        let c = datalib_metrics::MODAL_COST;
        let by = by_step(
            &[
                row(
                    "r2",
                    EMBED,
                    c,
                    "gpu=T4",
                    "2026-11-18T18:10:00+00:00",
                    8_065_018,
                ),
                row(
                    "r1",
                    EMBED,
                    c,
                    "gpu=T4",
                    "2026-11-18T15:20:00+00:00",
                    30_000,
                ),
                row(
                    "r1",
                    EMBED,
                    c,
                    "gpu=A10G",
                    "2026-11-18T15:21:00+00:00",
                    20_000,
                ),
            ],
            &[
                row("r2", EMBED, "", "gpu=T4", "", 121),
                row("r1", EMBED, "", "gpu=T4", "", 2),
                row("r1", EMBED, "", "gpu=A10G", "", 1),
            ],
            &[row("r2", EMBED, "", "gpu=T4", "", 33_000)],
        );
        let cell = &by[EMBED];
        assert_eq!(cell.value, Some(8_115_018));
        assert_eq!(cell.unit, UNIT);
        let totals: Vec<i64> = cell.samples.iter().map(|s| s.value).collect();
        assert_eq!(totals, [50_000, 8_115_018]);
        assert_eq!(
            cell.detail.as_deref(),
            Some(
                "about $8.12 at Modal's list price over the 2 runs the run store keeps, \
                 since 2026-11-18. The latest: $8.07, 121 T4 Sandboxes, 9 h 10 min of \
                 Sandbox time. An estimate that errs high: each Sandbox is priced as if \
                 it used a core and 2 GiB beside its GPU."
            )
        );
        assert!(!by.contains_key("sickbay/embed"));
    }

    #[test]
    fn dollars_keep_a_fraction_of_a_cent() {
        assert_eq!(dollars(0), "$0.00");
        assert_eq!(dollars(1_234), "$0.0012");
        assert_eq!(dollars(6_708_161), "$6.71");
    }
}
