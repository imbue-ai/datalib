//! What a write to a time-series store has to do, decided from the rows
//! alone: which time series keep which key, and which rows and chunks to
//! write, remove or leave. Writing only what differs keeps a render that
//! appended a day to one time series to one chunk written.

use std::collections::{BTreeMap, HashMap};

use datalib_time_series_schema::{TimeSeriesChunkRow, TimeSeriesRow};

/// `wanted` under the keys they have in `held`, matched by
/// `time_series_id`; a new one gets a key past every held one.
pub fn assign_keys(held: &[TimeSeriesRow], wanted: &[TimeSeriesRow]) -> Vec<TimeSeriesRow> {
    let by_id: HashMap<&str, i64> = held
        .iter()
        .map(|r| (r.time_series_id.as_str(), r.ts_key))
        .collect();
    let mut next_key = held.iter().map(|r| r.ts_key).max().unwrap_or(0) + 1;
    wanted
        .iter()
        .map(|w| {
            let ts_key = by_id
                .get(w.time_series_id.as_str())
                .copied()
                .unwrap_or_else(|| {
                    next_key += 1;
                    next_key - 1
                });
            TimeSeriesRow {
                ts_key,
                ..w.clone()
            }
        })
        .collect()
}

#[derive(Debug, PartialEq)]
pub struct Diff<T> {
    /// New, or different from what is held under the same key.
    pub write: Vec<T>,
    /// Held keys nothing wanted has.
    pub remove: Vec<i64>,
    pub unchanged: usize,
}

fn diff<T: PartialEq + Clone>(held: &[T], wanted: &[T], key: impl Fn(&T) -> i64) -> Diff<T> {
    let held_by: BTreeMap<i64, &T> = held.iter().map(|h| (key(h), h)).collect();
    let mut out = Diff {
        write: Vec::new(),
        remove: Vec::new(),
        unchanged: 0,
    };
    for w in wanted {
        match held_by.get(&key(w)) {
            Some(h) if *h == w => out.unchanged += 1,
            _ => out.write.push(w.clone()),
        }
    }
    let wanted_keys: std::collections::BTreeSet<i64> = wanted.iter().map(&key).collect();
    out.remove = held_by
        .into_keys()
        .filter(|k| !wanted_keys.contains(k))
        .collect();
    out
}

/// One source's rows, by `ts_key`.
pub fn rows(held: &[TimeSeriesRow], wanted: &[TimeSeriesRow]) -> Diff<TimeSeriesRow> {
    diff(held, wanted, |r| r.ts_key)
}

/// One time series' chunks, by `first_t`.
pub fn chunks(
    held: &[TimeSeriesChunkRow],
    wanted: &[TimeSeriesChunkRow],
) -> Diff<TimeSeriesChunkRow> {
    diff(held, wanted, |c| c.first_t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_time_series_schema::{TimeKind, ValueType};

    fn row(ts_key: i64, id: &str) -> TimeSeriesRow {
        TimeSeriesRow {
            source_id: "g".into(),
            ts_key,
            time_series_id: id.into(),
            about_uuid: None,
            name: id.into(),
            human_name: id.into(),
            unit: "1".into(),
            value_type: ValueType::Float,
            time_kind: TimeKind::Day,
            point_count: 1,
            first_t: Some(1),
            last_t: Some(1),
        }
    }

    #[test]
    fn a_time_series_keeps_its_key_and_a_new_one_gets_one_past_every_held() {
        let held = [row(4, "hr"), row(2, "steps")];
        let keyed = assign_keys(&held, &[row(0, "sleep"), row(0, "hr"), row(0, "vo2")]);
        let keys: Vec<(&str, i64)> = keyed
            .iter()
            .map(|r| (r.time_series_id.as_str(), r.ts_key))
            .collect();
        assert_eq!(keys, [("sleep", 5), ("hr", 4), ("vo2", 6)]);
    }

    #[test]
    fn only_what_differs_is_written_and_what_is_gone_is_removed() {
        let held = [row(1, "hr"), row(2, "steps"), row(3, "vo2")];
        let mut relabelled = row(2, "steps");
        relabelled.human_name = "Steps".into();
        let d = rows(&held, &[row(1, "hr"), relabelled.clone(), row(4, "new")]);
        assert_eq!(d.write, vec![relabelled, row(4, "new")]);
        assert_eq!(d.remove, vec![3]);
        assert_eq!(d.unchanged, 1);
    }
}
