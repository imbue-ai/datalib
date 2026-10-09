//! The counts after a Manage row's name: how many errors and warnings a
//! step found, from the `problems{severity=…}` metrics it reported at
//! the end of its last run. A red and a yellow number when there are
//! any; nothing when there are none, or when the step has never
//! counted. A group's is the sum of its steps', since each step counts
//! only what it found itself. The System row's count is the config's
//! own warnings.

use std::collections::HashMap;

use datalib_columns::{Chip, ChipKind};
use datalib_problems::{Severity, METRIC};
use datalib_runs::MetricRow;

/// What one step last counted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProblemCounts {
    pub errors: i64,
    pub warnings: i64,
    /// The run the count comes from.
    pub run_id: String,
}

/// `step id → counts`, from the newest [`METRIC`] samples per step. A
/// step with only one of the two series reported is still counted —
/// the other reads as zero — but a label this build cannot name is
/// skipped rather than guessed at.
pub fn counts_by_step(latest: &[MetricRow]) -> HashMap<String, ProblemCounts> {
    let mut out: HashMap<String, ProblemCounts> = HashMap::new();
    for m in latest.iter().filter(|m| m.name == METRIC) {
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

/// A step's chips: bare numbers, the words on hover.
pub fn chips(counts: Option<&ProblemCounts>) -> Vec<Chip> {
    let Some(c) = counts else {
        return Vec::new();
    };
    draw(
        c.errors,
        c.warnings,
        &format!("this step found, as of run {}", c.run_id),
    )
}

/// A group's chips: the sum over the steps under it that have counted.
pub fn group_chips(steps: &[Option<&ProblemCounts>]) -> Vec<Chip> {
    let counted: Vec<&ProblemCounts> = steps.iter().flatten().copied().collect();
    draw(
        counted.iter().map(|c| c.errors).sum(),
        counted.iter().map(|c| c.warnings).sum(),
        "its steps found, as of each one's last run",
    )
}

fn draw(errors: i64, warnings: i64, whose: &str) -> Vec<Chip> {
    let plural = |n: i64| if n == 1 { "" } else { "s" };
    let mut out = Vec::new();
    if errors > 0 {
        out.push(Chip {
            kind: ChipKind::Error,
            text: errors.to_string(),
            title: format!(
                "{errors} error{s} {whose} \u{2014} double-click to see them",
                s = plural(errors)
            ),
        });
    }
    if warnings > 0 {
        out.push(Chip {
            kind: ChipKind::Warning,
            text: warnings.to_string(),
            title: format!(
                "{warnings} warning{s} {whose} \u{2014} double-click to see them",
                s = plural(warnings)
            ),
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
        let by = counts_by_step(&latest);
        assert_eq!(
            by["slack/render_markdown"],
            ProblemCounts {
                errors: 3,
                warnings: 12,
                run_id: "r9".into()
            }
        );
        assert_eq!(
            by["mail/render_markdown"],
            ProblemCounts {
                errors: 0,
                warnings: 0,
                run_id: "r8".into()
            }
        );
        assert!(
            !by.contains_key("mail/ingest"),
            "another series is not a count"
        );
    }

    /// Numbers only, red before yellow, and nothing at all for a clean
    /// count — a green zero after every name was noise.
    #[test]
    fn counts_draw_as_bare_numbers_and_a_clean_or_missing_count_draws_nothing() {
        assert!(chips(None).is_empty());
        assert!(chips(Some(&ProblemCounts {
            run_id: "r1".into(),
            ..Default::default()
        }))
        .is_empty());
        let some = chips(Some(&ProblemCounts {
            errors: 1,
            warnings: 12,
            run_id: "r1".into(),
        }));
        assert_eq!(
            some.iter()
                .map(|c| (c.kind, c.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(ChipKind::Error, "1"), (ChipKind::Warning, "12")]
        );
        assert!(some[0].title.starts_with("1 error this step found"));
        let warnings_only = chips(Some(&ProblemCounts {
            warnings: 3,
            run_id: "r1".into(),
            ..Default::default()
        }));
        assert_eq!(
            warnings_only
                .iter()
                .map(|c| (c.kind, c.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(ChipKind::Warning, "3")]
        );
    }

    /// A group's count is its steps' summed: each counts only what it
    /// found, so the download's warning and render's error are two, and
    /// a step that never counted adds nothing.
    #[test]
    fn a_group_counts_the_sum_of_its_steps() {
        let ingest = ProblemCounts {
            warnings: 1,
            run_id: "r1".into(),
            ..Default::default()
        };
        let render = ProblemCounts {
            errors: 2,
            warnings: 1,
            run_id: "r2".into(),
        };
        let drawn = group_chips(&[Some(&ingest), Some(&render), None]);
        assert_eq!(
            drawn
                .iter()
                .map(|c| (c.kind, c.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(ChipKind::Error, "2"), (ChipKind::Warning, "2")]
        );
        assert!(group_chips(&[None, None]).is_empty());
    }
}
