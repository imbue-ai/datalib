//! The Documents cell: how many documents a step's store holds, from
//! the `documents` metric it reported at its last checkpoint or at the
//! end of its last run. The cell draws the number and nothing else, so
//! all that is needed here is the join — but a step that has never
//! reported the series must be absent rather than zero, which is the
//! one thing worth a test.

use std::collections::HashMap;

use datalib_runs::MetricRow;

/// `step id → documents`, from the newest [`datalib_metrics::DOCUMENTS`]
/// sample per step. Only render steps report it, so most steps are
/// absent and their cell is blank.
pub fn by_step(latest: &[MetricRow]) -> HashMap<String, i64> {
    latest
        .iter()
        .filter(|m| m.name == datalib_metrics::DOCUMENTS)
        .map(|m| (m.step.clone(), m.value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(step: &str, name: &str, value: i64) -> MetricRow {
        MetricRow {
            run_id: "r1".into(),
            step: step.into(),
            name: name.into(),
            value,
            ..Default::default()
        }
    }

    /// A counted zero is a zero; a step that never counted is absent,
    /// so the column draws nothing rather than claiming an empty store.
    #[test]
    fn only_the_documents_series_counts_and_a_zero_survives_it() {
        let by = by_step(&[
            sample("slack/render_markdown", datalib_metrics::DOCUMENTS, 1204),
            sample("mail/render_markdown", datalib_metrics::DOCUMENTS, 0),
            sample("mail/ingest", "rows_upserted", 90_000),
        ]);
        assert_eq!(by.get("slack/render_markdown"), Some(&1204));
        assert_eq!(by.get("mail/render_markdown"), Some(&0));
        assert_eq!(by.get("mail/ingest"), None, "another series is not a count");
    }
}
