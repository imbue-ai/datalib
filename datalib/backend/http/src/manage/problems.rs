//! The counts after a Manage row's name: how many errors and warnings a
//! step found, from the `problems{severity=…}` metrics it reported at
//! the end of its last run, and the passes of that run that failed and
//! were run again, from the runner's `failed_passes`, counted as
//! warnings. A red and a yellow number when there are any; nothing when
//! there are none, or when the step has never counted. A group's is the
//! sum of its steps', since each step counts only what it found itself.
//! A problem a person muted that the index still holds is taken off the
//! count of the step that found it (`problem_mutes/README.md`).
//! The System row's count is the config's own warnings.

use std::collections::HashMap;

use datalib_columns::{Chip, ChipKind};
use datalib_problem_mutes::Muted;
use datalib_problems::{CountedBy, Severity, METRIC};
use datalib_runs::MetricRow;

/// What one step last counted, and what the runner counted of it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProblemCounts {
    pub errors: i64,
    pub warnings: i64,
    /// The run the step's own count comes from; empty when it never counted.
    pub run_id: String,
    /// Passes that failed and were run again in the last run that invoked
    /// the step, which reports only its last pass.
    pub failed_passes: i64,
    /// The run that count comes from.
    pub failed_passes_run_id: String,
    /// Taken off `errors` and `warnings`: problems found and muted.
    pub muted: Muted,
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

/// Whose count a step keeps, by its function: a source's download and
/// render count its problems, the grid index its own; any other step
/// counts what nothing can mute.
fn counter_of(step: &str) -> Option<(Option<String>, CountedBy)> {
    let (group, function) = step.rsplit_once('/')?;
    match function {
        "ingest" => Some((Some(group.to_string()), CountedBy::Download)),
        "render_markdown" => Some((Some(group.to_string()), CountedBy::Render)),
        "grid_index" => Some((None, CountedBy::Index)),
        _ => None,
    }
}

/// Each step's count with the problems muted in it taken off. A step's
/// count and the index are read at different moments, so a mute can
/// outnumber what the step last counted; the count stops at zero.
pub fn take_off_mutes(
    counts: &mut HashMap<String, ProblemCounts>,
    muted: &HashMap<(Option<String>, CountedBy), Muted>,
) {
    for (step, c) in counts.iter_mut() {
        let Some(m) = counter_of(step).and_then(|key| muted.get(&key)) else {
            continue;
        };
        c.muted = Muted {
            errors: m.errors.min(c.errors),
            warnings: m.warnings.min(c.warnings),
        };
        c.errors -= c.muted.errors;
        c.warnings -= c.muted.warnings;
    }
}

/// A step's chips: bare numbers, the words on hover.
pub fn chips(counts: Option<&ProblemCounts>) -> Vec<Chip> {
    let Some(c) = counts else {
        return Vec::new();
    };
    draw(
        c.errors,
        c.warnings,
        c.muted,
        c.failed_passes,
        &format!("this step found, as of run {}", c.run_id),
        &format!("as of run {}", c.failed_passes_run_id),
    )
}

/// A group's chips: the sum over the steps under it that have counted.
pub fn group_chips(steps: &[Option<&ProblemCounts>]) -> Vec<Chip> {
    let counted: Vec<&ProblemCounts> = steps.iter().flatten().copied().collect();
    draw(
        counted.iter().map(|c| c.errors).sum(),
        counted.iter().map(|c| c.warnings).sum(),
        Muted {
            errors: counted.iter().map(|c| c.muted.errors).sum(),
            warnings: counted.iter().map(|c| c.muted.warnings).sum(),
        },
        counted.iter().map(|c| c.failed_passes).sum(),
        "its steps found, as of each one's last run",
        "as of each step's last run",
    )
}

/// `found` says whose the errors and warnings are; `ran`, when the
/// failed passes were counted. A muted problem draws nothing; the hover
/// says how many there are beside the ones counted.
fn draw(
    errors: i64,
    warnings: i64,
    muted: Muted,
    failed_passes: i64,
    found: &str,
    ran: &str,
) -> Vec<Chip> {
    let plural = |n: i64| if n == 1 { "" } else { "s" };
    let besides = |n: i64| {
        if n > 0 {
            format!(", and {n} more you muted")
        } else {
            String::new()
        }
    };
    let mut out = Vec::new();
    if errors > 0 {
        out.push(Chip {
            kind: ChipKind::Error,
            text: errors.to_string(),
            title: format!(
                "{errors} error{s} {found}{more} \u{2014} double-click to see them",
                s = plural(errors),
                more = besides(muted.errors)
            ),
        });
    }
    // A failed pass is not in the problems a double-click opens; its
    // error is in the step's log.
    let mut said = Vec::new();
    if warnings > 0 {
        said.push(format!(
            "{warnings} warning{s} {found}{more} \u{2014} double-click to see them",
            s = plural(warnings),
            more = besides(muted.warnings)
        ));
    }
    match failed_passes {
        0 => {}
        1 => said.push(format!(
            "1 pass failed and was run again, {ran} \u{2014} its error is in the log, a \
             double-click on the status away"
        )),
        n => said.push(format!(
            "{n} passes failed and were run again, {ran} \u{2014} their errors are in the \
             log, a double-click on the status away"
        )),
    }
    if !said.is_empty() {
        out.push(Chip {
            kind: ChipKind::Warning,
            text: (warnings + failed_passes).to_string(),
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
        assert_eq!(
            by["mail/render_markdown"],
            ProblemCounts {
                errors: 0,
                warnings: 0,
                run_id: "r8".into(),
                ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    /// A muted problem took the source off the Dashboard's Needs you
    /// only if it comes off the count the Needs you reads: the count of
    /// the step that found it, and no other.
    #[test]
    fn a_mute_comes_off_the_count_of_the_step_that_found_it() {
        let count = |errors, warnings| ProblemCounts {
            errors,
            warnings,
            run_id: "r1".into(),
            ..Default::default()
        };
        let mut counts: HashMap<String, ProblemCounts> = [
            ("enterprise/ingest", count(0, 1)),
            ("enterprise/render_markdown", count(2, 3)),
            ("defiant/render_markdown", count(1, 0)),
            ("unified_document_index/grid_index", count(1, 0)),
            ("enterprise/keyword_index", count(4, 0)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let muted: HashMap<(Option<String>, CountedBy), Muted> = [
            (
                (Some("enterprise".to_string()), CountedBy::Download),
                Muted {
                    errors: 0,
                    warnings: 1,
                },
            ),
            (
                (Some("enterprise".to_string()), CountedBy::Render),
                Muted {
                    errors: 2,
                    warnings: 5,
                },
            ),
            (
                (None, CountedBy::Index),
                Muted {
                    errors: 1,
                    warnings: 0,
                },
            ),
        ]
        .into_iter()
        .collect();
        take_off_mutes(&mut counts, &muted);
        let left = |step: &str| (counts[step].errors, counts[step].warnings);
        assert_eq!(left("enterprise/ingest"), (0, 0));
        assert_eq!(left("enterprise/render_markdown"), (0, 0), "stops at zero");
        assert_eq!(left("defiant/render_markdown"), (1, 0), "another source's");
        assert_eq!(left("unified_document_index/grid_index"), (0, 0));
        assert_eq!(
            left("enterprise/keyword_index"),
            (4, 0),
            "counts nothing muted"
        );
        assert!(chips(counts.get("enterprise/render_markdown")).is_empty());
        assert!(group_chips(&[
            counts.get("enterprise/ingest"),
            counts.get("enterprise/render_markdown")
        ])
        .is_empty());

        let mut one_muted: HashMap<String, ProblemCounts> =
            [("enterprise/render_markdown".to_string(), count(3, 0))].into();
        take_off_mutes(&mut one_muted, &muted);
        let drawn = chips(one_muted.get("enterprise/render_markdown"));
        assert_eq!(drawn[0].text, "1");
        assert!(
            drawn[0].title.contains("and 2 more you muted"),
            "{}",
            drawn[0].title
        );
    }

    /// A pass that failed and was run again left the row clean: the run
    /// reports only its last pass, and the runner never writes a step's
    /// `problems`. The runner's own count of such passes is a warning on
    /// the row, said on hover with the run it is from, and a group sums
    /// it with the rest.
    #[test]
    fn a_failed_pass_that_ran_again_is_a_warning_on_its_row() {
        let failed = |step: &str, value: i64, run: &str| MetricRow {
            name: datalib_metrics::FAILED_PASSES.into(),
            ..sample(step, "", value, run)
        };
        let by = counts_by_step(
            &[sample("evals/score", "severity=warning", 2, "r8")],
            &[
                failed("evals/score", 1, "r9"),
                failed("evals/ingest", 0, "r9"),
                failed("evals/render", 3, "r9"),
            ],
        );
        let score = chips(by.get("evals/score"));
        assert_eq!(
            score
                .iter()
                .map(|c| (c.kind, c.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(ChipKind::Warning, "3")]
        );
        assert_eq!(
            score[0].title,
            "2 warnings this step found, as of run r8 \u{2014} double-click to see them\n\
             1 pass failed and was run again, as of run r9 \u{2014} its error is in the log, a \
             double-click on the status away"
        );
        assert!(
            chips(by.get("evals/ingest")).is_empty(),
            "no failed pass and nothing found: nothing drawn"
        );
        assert!(
            chips(by.get("evals/render"))[0]
                .title
                .starts_with("3 passes failed and were run again, as of run r9"),
            "{:?}",
            chips(by.get("evals/render"))
        );
        let group = group_chips(&[
            by.get("evals/ingest"),
            by.get("evals/score"),
            by.get("evals/render"),
        ]);
        assert_eq!(
            group
                .iter()
                .map(|c| (c.kind, c.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(ChipKind::Warning, "6")]
        );
    }
}
