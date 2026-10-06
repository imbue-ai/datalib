//! How a problem reaches the log: every writer that stores a row it made
//! (not a copy) hands it to [`note_recorded`], and the step logs the tally
//! once, at its end, through [`log_recorded`] — one line per kind of
//! problem, at the highest severity among its rows. The line carries the
//! count and the vocabulary, never a row's `sample` or key: those hold the
//! record's own contents and belong in the store, not the log.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::{Outcome, ProblemRow, Reason, Severity, Stage};

/// One kind of problem: every column of a row that comes from the code
/// rather than from the record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Kind {
    pub source_id: String,
    pub stage: &'static str,
    pub outcome: &'static str,
    pub reason: &'static str,
    pub rule: Option<String>,
    pub field: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    pub rows: usize,
    pub severity: Severity,
}

#[derive(Debug, Default)]
pub struct Tally(BTreeMap<Kind, Count>);

impl Tally {
    pub fn add<'a>(&mut self, rows: impl IntoIterator<Item = &'a ProblemRow>) {
        for row in rows {
            let kind = Kind {
                source_id: row.source_id.clone(),
                stage: Stage::as_str(row.stage),
                outcome: Outcome::as_str(row.outcome),
                reason: Reason::as_str(row.reason),
                rule: row.rule.clone(),
                field: row.field.clone(),
            };
            let count = self.0.entry(kind).or_insert(Count {
                rows: 0,
                severity: row.severity,
            });
            count.rows += 1;
            count.severity = louder(count.severity, row.severity);
        }
    }

    pub fn take(&mut self) -> Vec<(Kind, Count)> {
        std::mem::take(&mut self.0).into_iter().collect()
    }
}

fn louder(a: Severity, b: Severity) -> Severity {
    let rank = |s| match s {
        Severity::Error => 2,
        Severity::Warning => 1,
        Severity::Info => 0,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

static RECORDED: Mutex<Tally> = Mutex::new(Tally(BTreeMap::new()));

/// Rows this process just stored and made itself. A copy of another
/// step's rows (render taking the download's, the index taking every
/// source's) was already counted by the step that made them.
pub fn note_recorded<'a>(rows: impl IntoIterator<Item = &'a ProblemRow>) {
    RECORDED.lock().unwrap_or_else(|e| e.into_inner()).add(rows);
}

/// Logs what [`note_recorded`] has counted since the last call, and
/// forgets it.
pub fn log_recorded() {
    let lines = RECORDED.lock().unwrap_or_else(|e| e.into_inner()).take();
    for (k, c) in lines {
        let source = (!k.source_id.is_empty()).then_some(k.source_id.as_str());
        macro_rules! line {
            ($level:ident, $msg:literal) => {
                tracing::$level!(
                    event = "problems_recorded",
                    source,
                    stage = k.stage,
                    outcome = k.outcome,
                    reason = k.reason,
                    rule = k.rule.as_deref(),
                    field = k.field.as_deref(),
                    count = c.rows,
                    $msg
                )
            };
        }
        match c.severity {
            Severity::Error => line!(error, "records were lost; each is a problems row"),
            Severity::Warning => line!(
                warn,
                "records were degraded or left out; each is a problems row"
            ),
            Severity::Info => line!(debug, "findings were recorded; each is a problems row"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Problem, Scope};

    fn row(key: &str, problem: Problem) -> ProblemRow {
        ProblemRow::new(
            "tng-takeout",
            Stage::Fetch,
            Scope::Entity(key),
            None,
            Outcome::Dropped,
            problem,
            None,
        )
    }

    /// 321 saved places with no key were once 321 identical log lines,
    /// and a row's sample is the record's own text.
    #[test]
    fn one_line_per_kind_at_its_loudest_and_never_the_sample() {
        let mut t = Tally::default();
        let places: Vec<ProblemRow> = (0..321)
            .map(|i| {
                row(
                    &format!("skipped:places:{i}"),
                    Problem::field("google_maps_url", Reason::NoIdentity, "Quark's Bar"),
                )
            })
            .collect();
        t.add(&places);
        t.add(&[
            row(
                "skipped:watch:a",
                Problem::lossy("not_a_video", None, "/post/1").severity(Severity::Info),
            ),
            row(
                "skipped:watch:b",
                Problem::lossy("not_a_video", None, "/post/2").severity(Severity::Warning),
            ),
        ]);

        let lines = t.take();
        assert_eq!(lines.len(), 2, "{lines:?}");
        let by_reason = |r: &str| lines.iter().find(|(k, _)| k.reason == r).unwrap();
        let (place, n) = by_reason("no_identity");
        assert_eq!(
            (place.reason, place.field.as_deref(), n.rows, n.severity),
            ("no_identity", Some("google_maps_url"), 321, Severity::Error)
        );
        let (watch, n) = by_reason("deliberate_loss");
        assert_eq!(
            (watch.rule.as_deref(), n.rows, n.severity),
            (Some("not_a_video"), 2, Severity::Warning),
            "the loudest row sets the level"
        );
        assert!(!format!("{lines:?}").contains("Quark"), "{lines:?}");
        assert!(t.take().is_empty(), "taking empties the tally");
    }
}
