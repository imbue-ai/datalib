//! The Queue and ETA cells: how much work is ahead of a step, from the
//! `queued` gauges, and when that work is done at the pace work has come
//! off the queue lately. The pace is what came *off*, not how the queue
//! changed: a queue the runner keeps for a consumer climbs seal by seal
//! and falls to nothing when a pass ends, and its net change reads as
//! growing at every peak. A group sums its steps' queues and waits on
//! the slowest of them.

use std::collections::BTreeMap;

use datalib_columns::Quantity;
use datalib_metrics::{DEQUEUED, DONE, QUEUED};
use datalib_runs::MetricSampleRow;
use serde::Serialize;

use crate::{secs_between, series_key, DagStepProgress};

/// How long a running step may go without a metric moving before its
/// ETA says it has stalled rather than guessing.
const STALL_AFTER_SECS: i64 = 60;

/// The shortest look at the queue an estimate is drawn from. Less than
/// this and one page arriving swings it by an order of magnitude.
const MEASURE_SECS: f64 = 15.0;

const STALLED: &str = "stalled";
const GROWING: &str = "growing";
const FLAT: &str = "flat";
const MEASURING: &str = "measuring";

pub(super) fn grouped(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// The same words the ETA cell draws, for the hovers that explain it.
fn duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs} sec")
    } else if secs < 3600 {
        format!("{} min", (secs as f64 / 60.0).round() as i64)
    } else {
        format!("{:.1} h", secs as f64 / 3600.0)
    }
}

/// Whether a series key (`name` or `name{labels}`) is of this name.
fn is_named(key: &str, name: &str) -> bool {
    key.strip_prefix(name)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('{'))
}

fn is_queued(key: &str) -> bool {
    is_named(key, QUEUED)
}

/// How fast a step's queues have been worked off lately.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct QueueDrain {
    /// Work that came off the queues over `secs`.
    pub taken: i64,
    pub secs: f64,
    /// What the queues held when that stretch began.
    pub queued_then: i64,
    /// Read off running totals of what came off — `dequeued_total` beside
    /// the runner's queues, `done_total` beside the step's own — rather than summed
    /// from the drops the queue's samples recorded.
    pub counted: bool,
    /// Nothing came off in the recent window, so the stretch reaches back
    /// to when the step started: a consumer in a pass longer than the
    /// window takes its work off only when the pass ends.
    pub since_start: bool,
}

/// A step's drain from its queue series' samples (oldest first within a
/// series, from [`datalib_runs::QUEUE_WINDOW`] back plus the one before),
/// its current values (`name{labels}` → value), when it started, and
/// `now`. `None` for a step with no queue, or nothing to measure from.
pub fn queue_drain(
    samples: &[&MetricSampleRow],
    current: &BTreeMap<String, i64>,
    started: Option<&str>,
    now: &str,
) -> Option<QueueDrain> {
    let queues: Vec<&String> = current.keys().filter(|k| is_queued(k)).collect();
    if queues.is_empty() {
        return None;
    }
    let mut by_series: BTreeMap<String, Vec<(&str, i64)>> = BTreeMap::new();
    for m in samples {
        by_series
            .entry(series_key(&m.name, &m.labels))
            .or_default()
            .push((m.ts_utc.as_str(), m.value));
    }
    let value_at = |key: &str, t: &str| -> i64 {
        by_series
            .get(key)
            .and_then(|pts| pts.iter().rev().find(|(ts, _)| *ts <= t))
            .map_or(0, |(_, v)| *v)
    };
    let (window_start, _) = datalib_time::parse_strict(now)
        .ok()?
        .bump_micros(-(datalib_runs::QUEUE_WINDOW.as_micros() as i64))
        .to_utc_and_offset();
    let opened = started.or_else(|| samples.iter().map(|m| m.ts_utc.as_str()).min())?;
    let start = window_start.as_str().max(opened);
    let queued_then = queues.iter().map(|k| value_at(k, start)).sum();
    let secs = secs_between(start, now)?.max(0.0);

    // `done_total` counts the step's own bar, so it is the pace only of the
    // step's own queue, and says nothing about one the runner keeps.
    let has_own_queue = current.contains_key(QUEUED);
    let counters: Vec<&String> = current
        .keys()
        .filter(|k| is_named(k, DEQUEUED) || (has_own_queue && k.as_str() == DONE))
        .collect();
    if !counters.is_empty() {
        let taken_since =
            |t: &str| -> i64 { counters.iter().map(|k| current[*k] - value_at(k, t)).sum() };
        let taken = taken_since(start);
        if taken <= 0 {
            if let Some(from) = started.filter(|s| *s < start) {
                return Some(QueueDrain {
                    taken: taken_since(from),
                    secs: secs_between(from, now)?.max(0.0),
                    queued_then,
                    counted: true,
                    since_start: true,
                });
            }
        }
        return Some(QueueDrain {
            taken,
            secs,
            queued_then,
            counted: true,
            since_start: false,
        });
    }
    // A step that reports only its queue: add up every fall in it.
    let taken = queues
        .iter()
        .map(|k| {
            let later = by_series
                .get(k.as_str())
                .into_iter()
                .flatten()
                .filter(|(ts, _)| *ts > start)
                .map(|(_, v)| *v);
            let path: Vec<i64> = std::iter::once(value_at(k, start))
                .chain(later)
                .chain(std::iter::once(current[*k]))
                .collect();
            path.windows(2).map(|w| (w[0] - w[1]).max(0)).sum::<i64>()
        })
        .sum();
    Some(QueueDrain {
        taken,
        secs,
        queued_then,
        counted: false,
        since_start: false,
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cells {
    pub queue: Quantity,
    pub eta: Quantity,
}

fn blank(unit: &str) -> Quantity {
    Quantity {
        unit: unit.into(),
        ..Default::default()
    }
}

fn noted(note: &str, detail: String) -> Quantity {
    Quantity {
        value: None,
        unit: "seconds".into(),
        note: Some(note.into()),
        detail: Some(detail),
    }
}

/// A step's two cells from what it has reported this run. A step that
/// reports no `queued` series has neither; one that has finished keeps
/// its queue only while something is still in it.
pub fn step_cells(p: Option<&DagStepProgress>, running: bool) -> Cells {
    let Some(p) = p else {
        return Cells {
            queue: blank("count"),
            eta: blank("seconds"),
        };
    };
    let queued: Vec<(&String, i64)> = p
        .metrics
        .iter()
        .filter(|(n, _)| is_queued(n))
        .map(|(n, v)| (n, *v))
        .collect();
    let total: i64 = queued.iter().map(|(_, v)| v).sum();
    if queued.is_empty() || (!running && total == 0) {
        return Cells {
            queue: blank("count"),
            eta: blank("seconds"),
        };
    }
    let queue = Quantity {
        value: Some(total),
        unit: "count".into(),
        note: None,
        detail: Some(queue_detail(&queued)),
    };
    let eta = if running {
        eta(total, p.queue_drain, p.progress_age_secs, p.log_age_secs)
    } else {
        blank("seconds")
    };
    Cells { queue, eta }
}

fn queue_detail(queued: &[(&String, i64)]) -> String {
    if queued.len() == 1 {
        return "Work the step says is still ahead of it.".into();
    }
    let parts = queued
        .iter()
        .map(|(name, v)| {
            match name
                .strip_prefix("queued{from=")
                .and_then(|s| s.strip_suffix('}'))
            {
                Some(from) => format!("{} from {from}", grouped(*v)),
                None => format!("{} of its own", grouped(*v)),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("Work still ahead of the step: {parts}.")
}

fn eta(
    total: i64,
    drain: Option<QueueDrain>,
    progress_age: Option<i64>,
    log_age: Option<i64>,
) -> Quantity {
    if total == 0 {
        return blank("seconds");
    }
    if let Some(age) = progress_age.filter(|a| *a >= STALL_AFTER_SECS) {
        let since = duration(age);
        let why = match log_age {
            Some(l) if l < STALL_AFTER_SECS => format!(
                "but it is still logging (last line {} ago) \u{2014} busy, not advancing",
                duration(l)
            ),
            Some(l) => format!("and it last logged {} ago \u{2014} silent", duration(l)),
            None => "and it has logged nothing".into(),
        };
        return noted(
            STALLED,
            format!(
                "No metric has moved for {since}, {why}. Double-click for the step's dashboard."
            ),
        );
    }
    let Some(d) = drain.filter(|d| d.secs >= MEASURE_SECS) else {
        return noted(
            MEASURING,
            format!(
                "The estimate is the pace work comes off the queue, and it takes {} seconds of \
                 watching.",
                MEASURE_SECS as i64
            ),
        );
    };
    let over = if d.since_start {
        format!(
            "since the step started {} ago",
            duration(d.secs.round() as i64)
        )
    } else {
        format!("over the last {}", duration(d.secs.round() as i64))
    };
    if d.taken <= 0 {
        if d.counted {
            return noted(
                MEASURING,
                format!(
                    "{} queued, and none of it has come off yet this run; the estimate starts \
                     with the first that does.",
                    grouped(total)
                ),
            );
        }
        let (note, how) = if total > d.queued_then {
            (
                GROWING,
                format!("grew by {}", grouped(total - d.queued_then)),
            )
        } else {
            (FLAT, "did not move".to_string())
        };
        return noted(
            note,
            format!(
                "{} queued. Nothing came off the queue {over} \u{2014} it {how} \u{2014} so there \
                 is no pace to finish at yet.",
                grouped(total)
            ),
        );
    }
    let per_sec = d.taken as f64 / d.secs;
    let secs = (total as f64 / per_sec).ceil() as i64;
    Quantity {
        value: Some(secs),
        unit: "seconds".into(),
        note: None,
        detail: Some(format!(
            "{} queued; {} came off it {over}. At that pace it is empty in about {}.",
            grouped(total),
            grouped(d.taken),
            duration(secs)
        )),
    }
}

/// A group's cells from its steps', each with the label it goes by.
/// The queue is the sum. The ETA waits on the slowest step, and a stall
/// anywhere outranks every estimate: a stuck step is the one thing the
/// group's figure must not hide.
pub fn group_cells(children: &[(&str, &Cells)]) -> Cells {
    let with_queue: Vec<(&str, i64)> = children
        .iter()
        .filter_map(|(l, c)| c.queue.value.map(|v| (*l, v)))
        .collect();
    let queue = if with_queue.is_empty() {
        blank("count")
    } else {
        Quantity {
            value: Some(with_queue.iter().map(|(_, v)| v).sum()),
            unit: "count".into(),
            note: None,
            detail: Some(
                with_queue
                    .iter()
                    .map(|(l, v)| format!("{l}: {}", grouped(*v)))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        }
    };
    let said = |(l, c): &(&str, &Cells)| {
        c.eta
            .detail
            .as_ref()
            .map(|d| format!("{l}: {d}"))
            .unwrap_or_default()
    };
    let noted_as = |note: &str| {
        children
            .iter()
            .find(|(_, c)| c.eta.note.as_deref() == Some(note))
    };
    let slowest = children
        .iter()
        .filter(|(_, c)| c.eta.value.is_some())
        .max_by_key(|(_, c)| c.eta.value);
    let eta = if let Some(stalled) = noted_as(STALLED) {
        noted(STALLED, said(stalled))
    } else if let Some(slowest) = slowest {
        Quantity {
            detail: Some(said(slowest)),
            ..slowest.1.eta.clone()
        }
    } else if let Some(c) = [GROWING, MEASURING, FLAT].into_iter().find_map(noted_as) {
        Quantity {
            detail: Some(said(c)),
            ..c.1.eta.clone()
        }
    } else {
        blank("seconds")
    };
    Cells { queue, eta }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(metrics: &[(&str, i64)], drain: Option<(i64, f64)>) -> DagStepProgress {
        DagStepProgress {
            msg: None,
            metrics: metrics.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            errors: 0,
            progress_age_secs: Some(0),
            log_age_secs: Some(0),
            queue_drain: drain.map(|(taken, secs)| QueueDrain {
                taken,
                secs,
                queued_then: 0,
                counted: true,
                since_start: false,
            }),
            updated_at_utc: String::new(),
        }
    }

    const NOW: &str = "2026-09-30T10:05:00.000000+00:00";
    const STARTED: &str = "2026-09-30T10:00:00.000000+00:00";

    /// `mm:ss` past 10:00, as the store stamps it.
    fn at(mm_ss: &str) -> String {
        format!("2026-09-30T10:{mm_ss}.000000+00:00")
    }

    fn sample(name: &str, labels: &str, mm_ss: &str, value: i64) -> MetricSampleRow {
        MetricSampleRow {
            name: name.into(),
            labels: labels.into(),
            ts_utc: at(mm_ss),
            value,
            ..Default::default()
        }
    }

    /// A consumer's queue as the runner keeps it: seals pile on, and a
    /// pass takes the whole pile off at once. 40 came off in the last two
    /// minutes (at 03:40), and 60 are on it now.
    fn sawtooth() -> Vec<MetricSampleRow> {
        let from = "from=a/ingest";
        vec![
            sample("queued", from, "02:30", 10),
            sample("dequeued_total", from, "02:30", 0),
            sample("queued", from, "03:20", 40),
            sample("queued", from, "03:40", 0),
            sample("dequeued_total", from, "03:40", 40),
            sample("queued", from, "04:30", 60),
        ]
    }

    fn current(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn a_sawtooth_is_paced_by_what_came_off_it_not_by_its_net_change() {
        let samples = sawtooth();
        let refs: Vec<&MetricSampleRow> = samples.iter().collect();
        let now = current(&[
            ("queued{from=a/ingest}", 60),
            ("dequeued_total{from=a/ingest}", 40),
        ]);
        let d = queue_drain(&refs, &now, Some(STARTED), NOW).unwrap();
        // The window opens at 03:00, when 10 were queued; the queue has
        // grown since, which a net reading calls "growing".
        assert_eq!((d.taken, d.secs, d.queued_then), (40, 120.0, 10));
        assert!(d.counted);
        let c = step_cells(
            Some(&DagStepProgress {
                queue_drain: Some(d),
                ..progress(&[("queued{from=a/ingest}", 60)], None)
            }),
            true,
        );
        // 40 in 120s is 1 every 3s; 60 queued is 180s.
        assert_eq!(c.eta.value, Some(180));

        // A step that reports only the queue: the fall at 03:40 is summed
        // from its samples, and reads the same.
        let queue_only: Vec<&MetricSampleRow> =
            samples.iter().filter(|m| m.name == "queued").collect();
        let d = queue_drain(
            &queue_only,
            &current(&[("queued{from=a/ingest}", 60)]),
            Some(STARTED),
            NOW,
        )
        .unwrap();
        assert_eq!((d.taken, d.counted), (40, false));
    }

    /// A pass longer than the window takes nothing off inside it; the
    /// pace then comes from the whole of the step's run.
    #[test]
    fn a_window_with_nothing_taken_off_reaches_back_to_the_start() {
        let samples = [
            sample("dequeued_total", "from=a", "00:30", 0),
            sample("dequeued_total", "from=a", "01:00", 100),
            sample("queued", "from=a", "01:00", 0),
            sample("queued", "from=a", "02:00", 50),
        ];
        let refs: Vec<&MetricSampleRow> = samples.iter().collect();
        let now = current(&[("queued{from=a}", 50), ("dequeued_total{from=a}", 100)]);
        let d = queue_drain(&refs, &now, Some(STARTED), NOW).unwrap();
        assert_eq!((d.taken, d.secs, d.since_start), (100, 300.0, true));
    }

    /// `done_total` counts the step's own bar, so it paces the step's own
    /// queue — and is not taken for the pace of a queue the runner keeps.
    #[test]
    fn done_paces_only_the_steps_own_queue() {
        let samples = [
            sample("done_total", "", "02:00", 10),
            sample("queued", "from=a", "02:00", 5),
        ];
        let refs: Vec<&MetricSampleRow> = samples.iter().collect();
        let own = queue_drain(
            &refs,
            &current(&[("queued", 5), ("done_total", 70)]),
            Some(STARTED),
            NOW,
        )
        .unwrap();
        assert_eq!((own.taken, own.counted), (60, true));
        let theirs = queue_drain(
            &refs,
            &current(&[("queued{from=a}", 5), ("done_total", 70)]),
            Some(STARTED),
            NOW,
        )
        .unwrap();
        assert!(!theirs.counted);
    }

    #[test]
    fn queue_sums_every_queued_series_and_ignores_the_rest() {
        let p = progress(
            &[
                ("rows", 1234),
                ("queued", 5),
                ("queued{from=a/ingest}", 7),
                ("queued_bytes", 9),
            ],
            None,
        );
        let c = step_cells(Some(&p), true);
        assert_eq!(c.queue.value, Some(12));
        assert!(c.queue.detail.unwrap().contains("7 from a/ingest"));
    }

    #[test]
    fn nothing_taken_off_yet_gets_a_word_not_a_figure() {
        let none_yet = step_cells(Some(&progress(&[("queued", 50)], Some((0, 60.0)))), true);
        assert_eq!(none_yet.eta.note.as_deref(), Some(MEASURING));
        let young = step_cells(Some(&progress(&[("queued", 50)], Some((10, 5.0)))), true);
        assert_eq!(young.eta.note.as_deref(), Some(MEASURING));
        let mut drops_only = progress(&[("queued", 50)], Some((0, 60.0)));
        drops_only.queue_drain = drops_only.queue_drain.map(|d| QueueDrain {
            counted: false,
            queued_then: 20,
            ..d
        });
        assert_eq!(
            step_cells(Some(&drops_only), true).eta.note.as_deref(),
            Some(GROWING)
        );
    }

    #[test]
    fn a_stall_outranks_the_estimate() {
        let mut p = progress(&[("queued", 300)], Some((300, 60.0)));
        p.progress_age_secs = Some(90);
        p.log_age_secs = Some(5);
        let c = step_cells(Some(&p), true);
        assert_eq!(c.eta.note.as_deref(), Some(STALLED));
        assert!(c.eta.detail.unwrap().contains("busy, not advancing"));
    }

    #[test]
    fn a_finished_step_shows_nothing_once_its_queue_is_empty() {
        let p = progress(&[("queued", 0)], Some((10, 60.0)));
        assert_eq!(
            step_cells(Some(&p), false),
            Cells {
                queue: blank("count"),
                eta: blank("seconds"),
            }
        );
        // Work waiting on a step that has not started is still worth
        // showing, with no estimate.
        let waiting = step_cells(Some(&progress(&[("queued{from=a}", 4)], None)), false);
        assert_eq!(waiting.queue.value, Some(4));
        assert_eq!(waiting.eta, blank("seconds"));
    }

    #[test]
    fn a_group_sums_queues_and_waits_on_its_slowest_step() {
        let fast = step_cells(Some(&progress(&[("queued", 10)], Some((60, 60.0)))), true);
        let slow = step_cells(Some(&progress(&[("queued", 100)], Some((10, 60.0)))), true);
        let g = group_cells(&[("Ingest", &fast), ("Render", &slow)]);
        assert_eq!(g.queue.value, Some(110));
        assert_eq!(g.eta.value, slow.eta.value);
        assert!(g.eta.detail.unwrap().starts_with("Render: "));

        let mut stuck = progress(&[("queued", 1)], None);
        stuck.progress_age_secs = Some(300);
        let stuck = step_cells(Some(&stuck), true);
        let g = group_cells(&[("Ingest", &stuck), ("Render", &slow)]);
        assert_eq!(g.eta.note.as_deref(), Some(STALLED));
    }
}
