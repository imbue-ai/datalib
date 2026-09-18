//! The Problems cell: how many errors and warnings a step's store holds,
//! from the `problems{severity=…}` metrics the step reported at the end
//! of its last run. Red and yellow when there are any, green when the
//! step counted and found none, and nothing at all when it has never
//! counted — a missing series is "unknown", never a false zero.

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

/// The chips the cell draws. `None` (no counts) draws nothing.
pub fn chips(counts: Option<&ProblemCounts>) -> Vec<Chip> {
    let Some(c) = counts else {
        return Vec::new();
    };
    let since = format!("as of run {}", c.run_id);
    if c.errors == 0 && c.warnings == 0 {
        return vec![Chip {
            kind: ChipKind::Ok,
            text: "0".into(),
            title: format!("No errors or warnings recorded, {since}"),
        }];
    }
    let mut out = Vec::new();
    if c.errors > 0 {
        out.push(Chip {
            kind: ChipKind::Error,
            text: format!("{} error{}", c.errors, if c.errors == 1 { "" } else { "s" }),
            title: format!(
                "{} record{} dropped, {since} \u{2014} double-click to see them",
                c.errors,
                if c.errors == 1 { "" } else { "s" }
            ),
        });
    }
    if c.warnings > 0 {
        out.push(Chip {
            kind: ChipKind::Warning,
            text: format!(
                "{} warning{}",
                c.warnings,
                if c.warnings == 1 { "" } else { "s" }
            ),
            title: format!(
                "{} record{} kept with something lost, {since} \u{2014} double-click to see them",
                c.warnings,
                if c.warnings == 1 { "" } else { "s" }
            ),
        });
    }
    out
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
                name: "rows_upserted".into(),
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

    /// Green zero only when the step counted; nothing when it never has.
    #[test]
    fn a_counted_zero_is_green_and_an_uncounted_step_draws_nothing() {
        assert!(chips(None).is_empty());
        let zero = chips(Some(&ProblemCounts {
            run_id: "r1".into(),
            ..Default::default()
        }));
        assert_eq!(zero.len(), 1);
        assert_eq!(zero[0].kind, ChipKind::Ok);
        assert_eq!(zero[0].text, "0");
        let some = chips(Some(&ProblemCounts {
            errors: 1,
            warnings: 2,
            run_id: "r1".into(),
        }));
        assert_eq!(
            some.iter()
                .map(|c| (c.kind, c.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (ChipKind::Error, "1 error"),
                (ChipKind::Warning, "2 warnings")
            ]
        );
    }
}
