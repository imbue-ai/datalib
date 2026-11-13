//! The counts after a Manage row's name. A red and a yellow number are
//! the error and warning rows the problems grid shows for that row: the
//! grid index's rows the step found, less the ones a person muted
//! (`problem_mutes/README.md`), counted by the one function that also
//! writes the grid's query, so a double-click opens exactly what was
//! counted. A grey number beside them is what is in the step's log
//! rather than in that grid: the passes of its last run that failed and
//! were run again, and what a step that writes no problem rows counted.
//! A group's is the sum of its steps'. The System row's count is the
//! config's own warnings.

use std::collections::HashMap;

use datalib_columns::{Chip, ChipKind};
use datalib_problem_mutes::{Counted, Counter, Tally};
use datalib_problems::{CountedBy, Severity, METRIC};
use datalib_runs::MetricRow;

/// What one step last reported of itself, from the run store.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProblemCounts {
    /// Its own `problems{severity=…}` count, which the Manage row draws
    /// only for a step whose rows the index does not hold.
    pub errors: i64,
    pub warnings: i64,
    /// The run that count comes from; empty when it never counted.
    pub run_id: String,
    /// Passes that failed and were run again in the last run that invoked
    /// the step, which reports only its last pass.
    pub failed_passes: i64,
    /// The run that count comes from.
    pub failed_passes_run_id: String,
}

/// `step id → counts`, from the newest [`METRIC`] samples per step and
/// the newest [`datalib_metrics::FAILED_PASSES`] ones. A step with only
/// one of the two severities reported is still counted — the other
/// reads as zero — but a label this build cannot name is skipped rather
/// than guessed at.
pub fn counts_by_step(
    problems: &[MetricRow],
    failed_passes: &[MetricRow],
) -> HashMap<String, ProblemCounts> {
    let mut out: HashMap<String, ProblemCounts> = HashMap::new();
    for m in failed_passes
        .iter()
        .filter(|m| m.name == datalib_metrics::FAILED_PASSES)
    {
        let entry = out.entry(m.step.clone()).or_default();
        entry.failed_passes = m.value;
        entry.failed_passes_run_id = m.run_id.clone();
    }
    for m in problems.iter().filter(|m| m.name == METRIC) {
        let Some(severity) = Severity::from_metric_labels(&m.labels) else {
            continue;
        };
        let entry = out.entry(m.step.clone()).or_default();
        match severity {
            Severity::Error => entry.errors = m.value,
            Severity::Warning => entry.warnings = m.value,
            Severity::Info => continue,
        }
        // Both series come from one run; either names it.
        entry.run_id = m.run_id.clone();
    }
    out
}

/// The index rows counted by whose they are; `None` before there is an
/// index, when nothing has been counted.
pub type IndexCounts = Option<HashMap<Counter, Counted>>;

/// Whose rows a step's count is, by its function: a source's download and
/// render, and the grid index's own. Every other step writes no rows.
fn counter_of(step: &str) -> Option<Counter> {
    let (group, function) = step.rsplit_once('/')?;
    let by = match function {
        "ingest" => CountedBy::Download,
        "render_markdown" => CountedBy::Render,
        "grid_index" => CountedBy::Index,
        _ => return None,
    };
    Some(datalib_problem_mutes::counter_of(group, by))
}

/// What a step's row draws, gathered from the index and the run store.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StepProblems {
    /// The index's rows this step found; `None` for a step that writes
    /// none, or before the index is there.
    pub rows: Option<Counted>,
    /// Whose rows they are, for the grid's query.
    pub counter: Option<Counter>,
    /// A step that writes no rows: what it last counted itself, and the
    /// run it is from. Only its log says what they are.
    pub unlisted: Option<(Tally, String)>,
    pub failed_passes: i64,
    pub failed_passes_run_id: String,
}

impl StepProblems {
    pub fn of(step: &str, reported: Option<&ProblemCounts>, index: &IndexCounts) -> Self {
        let counter = counter_of(step);
        let rows = match (&counter, index) {
            (Some(c), Some(index)) => Some(index.get(c).copied().unwrap_or_default()),
            _ => None,
        };
        let unlisted = match (&counter, reported) {
            (None, Some(r)) if !r.run_id.is_empty() => Some((
                Tally {
                    errors: r.errors,
                    warnings: r.warnings,
                },
                r.run_id.clone(),
            )),
            _ => None,
        };
        StepProblems {
            rows,
            counter,
            unlisted,
            failed_passes: reported.map_or(0, |r| r.failed_passes),
            failed_passes_run_id: reported
                .map(|r| r.failed_passes_run_id.clone())
                .unwrap_or_default(),
        }
    }

    fn shown(&self) -> Tally {
        self.rows.map(|r| r.shown).unwrap_or_default()
    }
}

/// The problems grid's query for what a step's red and yellow numbers
/// count; `None` when they count nothing.
pub fn query(step: &StepProblems) -> Option<String> {
    let counter = step.counter.as_ref()?;
    (!step.shown().is_empty()).then(|| datalib_problem_mutes::query(counter))
}

/// The same for a group: the rows its steps' numbers count together.
pub fn group_query(steps: &[StepProblems]) -> Option<String> {
    if steps.iter().all(|s| s.shown().is_empty()) {
        return None;
    }
    let counters: Vec<Counter> = steps.iter().filter_map(|s| s.counter.clone()).collect();
    datalib_problem_mutes::group_query(&counters)
}

/// A step's chips: bare numbers, the words on hover.
pub fn chips(step: &StepProblems) -> Vec<Chip> {
    draw(&Drawn {
        shown: step.shown(),
        muted: step.rows.map(|r| r.muted).unwrap_or_default(),
        unlisted: step.unlisted.as_ref().map(|(t, _)| *t).unwrap_or_default(),
        unlisted_from: step
            .unlisted
            .as_ref()
            .map(|(_, run)| format!("as of run {run}"))
            .unwrap_or_default(),
        failed_passes: step.failed_passes,
        ran: format!("as of run {}", step.failed_passes_run_id),
    })
}

/// A group's chips: the sum over the steps under it.
pub fn group_chips(steps: &[StepProblems]) -> Vec<Chip> {
    let sum = |f: fn(&StepProblems) -> Tally| {
        steps.iter().map(f).fold(Tally::default(), |a, b| Tally {
            errors: a.errors + b.errors,
            warnings: a.warnings + b.warnings,
        })
    };
    draw(&Drawn {
        shown: sum(StepProblems::shown),
        muted: sum(|s| s.rows.map(|r| r.muted).unwrap_or_default()),
        unlisted: sum(|s| s.unlisted.as_ref().map(|(t, _)| *t).unwrap_or_default()),
        unlisted_from: "as of each step's last run".into(),
        failed_passes: steps.iter().map(|s| s.failed_passes).sum(),
        ran: "as of each step's last run".into(),
    })
}

struct Drawn {
    shown: Tally,
    muted: Tally,
    unlisted: Tally,
    unlisted_from: String,
    failed_passes: i64,
    ran: String,
}

/// Red and yellow are rows a double-click lists; grey is what only the
/// log says.
fn draw(d: &Drawn) -> Vec<Chip> {
    let plural = |n: i64| if n == 1 { "" } else { "s" };
    let besides = |n: i64| {
        if n > 0 {
            format!(", and {n} more you muted")
        } else {
            String::new()
        }
    };
    let mut out = Vec::new();
    for (n, muted, kind, noun) in [
        (d.shown.errors, d.muted.errors, ChipKind::Error, "error"),
        (
            d.shown.warnings,
            d.muted.warnings,
            ChipKind::Warning,
            "warning",
        ),
    ] {
        if n > 0 {
            out.push(Chip {
                kind,
                text: n.to_string(),
                title: format!(
                    "{n} {noun}{s} in the problems list{more} \u{2014} double-click to see them",
                    s = plural(n),
                    more = besides(muted)
                ),
            });
        }
    }
    let mut said = Vec::new();
    let unlisted = d.unlisted.errors + d.unlisted.warnings;
    if unlisted > 0 {
        said.push(format!(
            "{unlisted} problem{s} this step counted, {from}, that it lists nowhere but its log",
            s = plural(unlisted),
            from = d.unlisted_from
        ));
    }
    match d.failed_passes {
        0 => {}
        1 => said.push(format!("1 pass failed and was run again, {}", d.ran)),
        n => said.push(format!("{n} passes failed and were run again, {}", d.ran)),
    }
    if !said.is_empty() {
        said.push("Each is in the log, a double-click on the status away.".into());
        out.push(Chip {
            kind: ChipKind::Metric,
            text: (unlisted + d.failed_passes).to_string(),
            title: said.join("\n"),
        });
    }
    out
}

/// The System row's chip: the config's warnings, each in words on
/// hover. A warning drops nothing, so no entry's row says it; this is
/// where the app shows what `datalib-dag --check` would print.
pub fn config_warning_chips(diagnostics: &[datalib_dag::Diagnostic]) -> Vec<Chip> {
    let warnings: Vec<String> = diagnostics
        .iter()
        .filter(|d| d.severity == datalib_dag::Severity::Warning)
        .map(datalib_dag::Diagnostic::describe)
        .collect();
    if warnings.is_empty() {
        return Vec::new();
    }
    let n = warnings.len();
    let s = if n == 1 { "" } else { "s" };
    vec![Chip {
        kind: ChipKind::Warning,
        text: n.to_string(),
        title: format!(
            "{n} config warning{s} \u{2014} double-click to open the config:\n{}",
            warnings.join("\n")
        ),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(step: &str, labels: &str, value: i64, run: &str) -> MetricRow {
        MetricRow {
            run_id: run.into(),
            step: step.into(),
            name: METRIC.into(),
            labels: labels.into(),
            value,
            ..Default::default()
        }
    }

    fn kinds(chips: &[Chip]) -> Vec<(ChipKind, &str)> {
        chips.iter().map(|c| (c.kind, c.text.as_str())).collect()
    }

    fn tally(errors: i64, warnings: i64) -> Tally {
        Tally { errors, warnings }
    }

    fn index(rows: &[(Counter, Counted)]) -> IndexCounts {
        Some(rows.iter().cloned().collect())
    }

    #[test]
    fn counts_come_from_the_labelled_series_and_nothing_else() {
        let latest = vec![
            sample("slack/render_markdown", "severity=error", 3, "r9"),
            sample("slack/render_markdown", "severity=warning", 12, "r9"),
            sample("slack/render_markdown", "severity=loud", 99, "r9"),
            sample("mail/render_markdown", "severity=warning", 0, "r8"),
            MetricRow {
                name: "rows_upserted_total".into(),
                ..sample("mail/ingest", "", 500, "r8")
            },
        ];
        let by = counts_by_step(&latest, &[]);
        assert_eq!(
            by["slack/render_markdown"],
            ProblemCounts {
                errors: 3,
                warnings: 12,
                run_id: "r9".into(),
                ..Default::default()
            }
        );
        assert_eq!(by["mail/render_markdown"].run_id, "r8");
        assert!(
            !by.contains_key("mail/ingest"),
            "another series is not a count"
        );
    }

    /// A step whose rows the index holds is drawn from those rows and not
    /// from what it reported: the number is the rows a double-click lists,
    /// and nothing before there is an index.
    #[test]
    fn a_step_the_index_holds_rows_of_counts_its_rows_not_its_report() {
        let reported = ProblemCounts {
            errors: 5,
            warnings: 5,
            run_id: "r1".into(),
            ..Default::default()
        };
        let render = datalib_problem_mutes::counter_of("enterprise", CountedBy::Render);
        let held = index(&[(
            render.clone(),
            Counted {
                shown: tally(1, 12),
                muted: tally(2, 0),
            },
        )]);
        let step = StepProblems::of("enterprise/render_markdown", Some(&reported), &held);
        let drawn = chips(&step);
        assert_eq!(
            kinds(&drawn),
            [(ChipKind::Error, "1"), (ChipKind::Warning, "12")]
        );
        assert_eq!(
            drawn[0].title,
            "1 error in the problems list, and 2 more you muted \u{2014} double-click to see them"
        );
        assert_eq!(
            query(&step),
            Some(datalib_problem_mutes::query(&render)),
            "the query is the counter's"
        );

        let clean = StepProblems::of("enterprise/render_markdown", Some(&reported), &index(&[]));
        assert!(chips(&clean).is_empty(), "no rows: nothing drawn");
        assert_eq!(query(&clean), None);
        let no_index = StepProblems::of("enterprise/render_markdown", Some(&reported), &None);
        assert!(chips(&no_index).is_empty(), "nothing counted yet");
    }

    /// A step that writes no rows keeps its own count, grey, since a
    /// double-click on it could list nothing; and so does a pass that
    /// failed and was run again. Neither is in the yellow number.
    #[test]
    fn what_only_the_log_holds_is_grey_and_apart_from_the_rows() {
        let failed = |step: &str, value: i64, run: &str| MetricRow {
            name: datalib_metrics::FAILED_PASSES.into(),
            ..sample(step, "", value, run)
        };
        let by = counts_by_step(
            &[sample(
                "garmin/render_time_series",
                "severity=warning",
                2,
                "r8",
            )],
            &[
                failed("garmin/render_time_series", 1, "r9"),
                failed("garmin/ingest", 0, "r9"),
                failed("garmin/render_markdown", 3, "r9"),
            ],
        );
        let held = index(&[(
            datalib_problem_mutes::counter_of("garmin", CountedBy::Render),
            Counted {
                shown: tally(0, 1),
                muted: tally(0, 0),
            },
        )]);
        let step = |id: &str| StepProblems::of(id, by.get(id), &held);

        let series = chips(&step("garmin/render_time_series"));
        assert_eq!(kinds(&series), [(ChipKind::Metric, "3")]);
        assert_eq!(
            series[0].title,
            "2 problems this step counted, as of run r8, that it lists nowhere but its log\n\
             1 pass failed and was run again, as of run r9\n\
             Each is in the log, a double-click on the status away."
        );
        assert_eq!(query(&step("garmin/render_time_series")), None);
        assert!(chips(&step("garmin/ingest")).is_empty());
        assert_eq!(
            kinds(&chips(&step("garmin/render_markdown"))),
            [(ChipKind::Warning, "1"), (ChipKind::Metric, "3")]
        );

        let steps: Vec<StepProblems> = [
            "garmin/ingest",
            "garmin/render_markdown",
            "garmin/render_time_series",
        ]
        .into_iter()
        .map(step)
        .collect();
        assert_eq!(
            kinds(&group_chips(&steps)),
            [(ChipKind::Warning, "1"), (ChipKind::Metric, "6")]
        );
        assert_eq!(
            group_query(&steps).as_deref(),
            Some("source:garmin -doc:render_store -severity:info muted:no")
        );
    }

    /// The index's own rows are the grid index step's, whatever source
    /// they name, and its group opens them alone.
    #[test]
    fn the_index_counts_its_own_rows() {
        let held = index(&[(
            (None, CountedBy::Index),
            Counted {
                shown: tally(1, 0),
                muted: tally(0, 0),
            },
        )]);
        let steps = [
            StepProblems::of("unified_document_index/grid_index", None, &held),
            StepProblems::of("unified_document_index/qmd_aggregator", None, &held),
        ];
        assert_eq!(kinds(&group_chips(&steps)), [(ChipKind::Error, "1")]);
        assert_eq!(
            group_query(&steps).as_deref(),
            Some("scope:entity doc:render_store -severity:info muted:no")
        );
    }
}
