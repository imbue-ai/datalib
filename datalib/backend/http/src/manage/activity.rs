//! What the Activity cell says about a running step, from what the run
//! store holds for it: how much is queued ahead of it, what it has
//! counted so far and how fast, whether it has stopped advancing, and
//! how many warnings and errors it has logged — the USE questions.

use datalib_columns::{Chip, ChipKind};

use crate::DagStepProgress;

/// How long a running step may go without a metric moving before the
/// cell says so. A download that has not written a row in a minute is
/// either waiting on the network or stuck, and either is worth a glance.
const STALL_AFTER_SECS: i64 = 60;

fn grouped(n: i64) -> String {
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

fn format_rate(per_sec: f64) -> String {
    if per_sec >= 1000.0 {
        format!("{:.1}k/s", per_sec / 1000.0)
    } else if per_sec >= 10.0 {
        format!("{}/s", per_sec.round() as i64)
    } else {
        format!("{per_sec:.1}/s")
    }
}

fn format_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", (secs as f64 / 60.0).round() as i64)
    } else {
        format!("{:.1}h", secs as f64 / 3600.0)
    }
}

fn is_queued(name: &str) -> bool {
    name == "queued" || name.starts_with("queued{")
}

/// The chips in the order the cell draws them: `queued` first, because
/// it is the one that says whether the step is keeping up; then every
/// other series with its rate; then a stall, if any; then the
/// warn/error count when there is one.
pub fn chips(p: &DagStepProgress) -> Vec<Chip> {
    let mut out = Vec::new();
    // Every `queued` series: the step's own gauge, plus one per producer
    // (`queued{from=slack/ingest}`). One chip, summed; the breakdown is
    // on hover.
    let queued: Vec<(&String, &i64)> = p.metrics.iter().filter(|(n, _)| is_queued(n)).collect();
    if !queued.is_empty() {
        let total: i64 = queued.iter().map(|(_, v)| **v).sum();
        let breakdown = queued
            .iter()
            .map(|(name, v)| {
                match name
                    .strip_prefix("queued{from=")
                    .and_then(|s| s.strip_suffix('}'))
                {
                    Some(from) => format!("{} from {from}", grouped(**v)),
                    None => format!("{} of its own", grouped(**v)),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        out.push(Chip {
            kind: if total > 0 {
                ChipKind::Info
            } else {
                ChipKind::Idle
            },
            text: format!("{} queued", grouped(total)),
            title: if queued.len() > 1 {
                format!("Work still ahead of the step: {breakdown}")
            } else {
                "Work the step says is still ahead of it".to_string()
            },
        });
    }
    for (name, value) in &p.metrics {
        if is_queued(name) {
            continue;
        }
        let rate = p.rates.get(name).copied().filter(|r| *r > 0.0);
        let moving = rate
            .map(|r| format!(" · {}", format_rate(r)))
            .unwrap_or_default();
        out.push(Chip {
            kind: ChipKind::Metric,
            text: format!("{name} {}{moving}", grouped(*value)),
            title: format!(
                "{name} = {} so far this run{}",
                grouped(*value),
                rate.map(|r| format!(", moving at {}", format_rate(r)))
                    .unwrap_or_default()
            ),
        });
    }
    // Not advancing: a running step whose numbers have not moved for a
    // while. Whether it is still logging is the difference between
    // "busy but stuck" and "silent", and goes on the hover.
    if let Some(age) = p.progress_age_secs.filter(|a| *a >= STALL_AFTER_SECS) {
        let since = format_age(age);
        let title = match p.log_age_secs {
            Some(log_age) if log_age < STALL_AFTER_SECS => format!(
                "No metric has moved for {since}, but the step is still logging (last line {} ago) — busy, not advancing",
                format_age(log_age)
            ),
            None => format!("No metric has moved for {since}, and the step has logged nothing"),
            Some(log_age) => format!(
                "No metric has moved for {since}, and the step last logged {} ago — silent",
                format_age(log_age)
            ),
        };
        out.push(Chip {
            kind: ChipKind::Warning,
            text: format!("no progress {since}"),
            title,
        });
    }
    if p.errors > 0 {
        let plural = if p.errors == 1 { "" } else { "s" };
        out.push(Chip {
            kind: ChipKind::Error,
            text: format!("{} ⚠", grouped(p.errors)),
            title: format!(
                "{} warning{plural} or error{plural} logged this run — double-click Status to read them",
                p.errors
            ),
        });
    }
    out
}

/// How far along, when the step reported the plain done/queued pair.
pub fn fraction(p: &DagStepProgress) -> Option<f64> {
    let done = *p.metrics.get("done")?;
    let queued = *p.metrics.get("queued")?;
    if done + queued <= 0 {
        return None;
    }
    Some((done as f64 / (done + queued) as f64).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(metrics: &[(&str, i64)], rates: &[(&str, f64)]) -> DagStepProgress {
        DagStepProgress {
            msg: None,
            metrics: metrics.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            errors: 0,
            rates: rates.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            progress_age_secs: None,
            log_age_secs: None,
            updated_at_utc: String::new(),
        }
    }

    #[test]
    fn queued_leads_summed_then_each_series_with_its_rate() {
        let p = progress(
            &[("rows", 1234), ("queued", 5), ("queued{from=a/ingest}", 7)],
            &[("rows", 20.4)],
        );
        let texts: Vec<String> = chips(&p).into_iter().map(|c| c.text).collect();
        assert_eq!(texts, ["12 queued", "rows 1,234 · 20/s"]);
    }

    #[test]
    fn a_stall_and_errors_trail() {
        let mut p = progress(&[("rows", 3)], &[]);
        p.progress_age_secs = Some(90);
        p.log_age_secs = Some(5);
        p.errors = 2;
        let got = chips(&p);
        assert_eq!(got[1].text, "no progress 2m");
        assert!(got[1].title.contains("busy, not advancing"));
        assert_eq!(got[2].text, "2 ⚠");
        assert_eq!(got[2].kind, ChipKind::Error);
    }

    #[test]
    fn a_fraction_needs_both_numbers() {
        assert_eq!(
            fraction(&progress(&[("done", 3), ("queued", 1)], &[])),
            Some(0.75)
        );
        assert_eq!(fraction(&progress(&[("done", 3)], &[])), None);
        assert_eq!(
            fraction(&progress(&[("done", 0), ("queued", 0)], &[])),
            None
        );
    }
}
