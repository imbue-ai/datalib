//! The Items cell: what a source holds, counted in the things a person
//! would count — messages, readings, events — as its render step last
//! reported them, with the last few days of runs behind the number so
//! a source that is growing looks it. How many documents those items
//! sit in goes to the hover.

use std::collections::HashMap;
use std::time::Duration;

use datalib_columns::{Sample, Timeseries};
use datalib_runs::{MetricRow, MetricSampleRow};

use super::queue::grouped;

/// How far back the sparkline reaches. Days rather than the Size
/// column's minutes: items move once a sync, and the question is
/// whether the last few syncs brought anything. The run store's own
/// retention is the other limit — a window it has pruned draws flat.
pub const WINDOW: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// `step id → its Items cell`, from the newest [`datalib_metrics::ITEMS`]
/// and [`datalib_metrics::DOCUMENTS`] per step and the samples of the
/// items series since the window opened. A step that has never reported
/// items is absent, so its cell is blank rather than a false zero.
pub fn by_step(
    items: &[MetricRow],
    documents: &[MetricRow],
    history: &[MetricSampleRow],
) -> HashMap<String, Timeseries> {
    let documents: HashMap<&str, i64> = documents
        .iter()
        .map(|m| (m.step.as_str(), m.value))
        .collect();
    items
        .iter()
        .map(|m| {
            let samples = history
                .iter()
                .filter(|s| s.step == m.step && s.labels == m.labels)
                .map(|s| Sample {
                    at: s.ts_utc.clone(),
                    value: s.value,
                })
                .collect();
            // A file tree or a photo library counts a table rather than
            // documents, and "in 0 documents" would read as a fault.
            let detail = match documents.get(m.step.as_str()) {
                Some(&d) if d > 0 => {
                    format!("{} in {}", count(m.value, "item"), count(d, "document"))
                }
                _ => count(m.value, "item"),
            };
            let cell = Timeseries {
                value: Some(m.value),
                unit: "items".into(),
                samples,
                detail: Some(detail),
                window_secs: WINDOW.as_secs(),
            };
            (m.step.clone(), cell)
        })
        .collect()
}

fn count(n: i64, noun: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{} {noun}{s}", grouped(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest(step: &str, name: &str, value: i64) -> MetricRow {
        MetricRow {
            run_id: "r2".into(),
            step: step.into(),
            name: name.into(),
            value,
            ..Default::default()
        }
    }

    fn sample(step: &str, at: &str, value: i64) -> MetricSampleRow {
        MetricSampleRow {
            step: step.into(),
            name: datalib_metrics::ITEMS.into(),
            ts_utc: at.into(),
            value,
            ..Default::default()
        }
    }

    /// Each step gets its own series and its own document count; a
    /// counted zero is a zero; a step that never counted items is
    /// absent even when it counted documents, so the cell stays blank
    /// until its first render under a build that counts them.
    #[test]
    fn each_step_draws_its_own_series_and_an_uncounted_one_is_absent() {
        let by = by_step(
            &[
                latest("slack/render_markdown", datalib_metrics::ITEMS, 48_210),
                latest("mail/render_markdown", datalib_metrics::ITEMS, 0),
                latest("photos/render_markdown", datalib_metrics::ITEMS, 4),
            ],
            &[
                latest("slack/render_markdown", datalib_metrics::DOCUMENTS, 1204),
                latest("old/render_markdown", datalib_metrics::DOCUMENTS, 7),
                latest("photos/render_markdown", datalib_metrics::DOCUMENTS, 0),
            ],
            &[
                sample("slack/render_markdown", "2026-09-01T00:00:00+00:00", 40_000),
                sample("mail/render_markdown", "2026-09-01T00:00:00+00:00", 0),
                sample("slack/render_markdown", "2026-09-02T00:00:00+00:00", 48_210),
            ],
        );
        let slack = &by["slack/render_markdown"];
        assert_eq!(slack.value, Some(48_210));
        let values: Vec<i64> = slack.samples.iter().map(|s| s.value).collect();
        assert_eq!(values, [40_000, 48_210]);
        assert_eq!(
            slack.detail.as_deref(),
            Some("48,210 items in 1,204 documents")
        );
        assert_eq!(slack.window_secs, WINDOW.as_secs());

        let mail = &by["mail/render_markdown"];
        assert_eq!(mail.value, Some(0));
        assert_eq!(mail.detail.as_deref(), Some("0 items"));
        let photos = &by["photos/render_markdown"];
        assert_eq!(
            photos.detail.as_deref(),
            Some("4 items"),
            "no documents to name"
        );

        assert!(!by.contains_key("old/render_markdown"));
    }
}
