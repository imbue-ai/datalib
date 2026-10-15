//! What a write to a time-series store has to do, decided from the rows
//! alone: which time series to replace, which to drop, which to leave.
//! A time series is replaced only when its points hash moved, because
//! replacing costs time in proportion to its points
//! (`hack/doltlite_time_series_cost/`).

use std::collections::{BTreeSet, HashMap};

use datalib_time_series_schema::TimeSeriesRow;

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Rows to write, each under the `ts_key` it keeps or is given; its
    /// points are replaced whole.
    pub replace: Vec<TimeSeriesRow>,
    /// Keys whose row and points go.
    pub remove: Vec<i64>,
    pub unchanged: usize,
}

/// `held` is what the store has now. `sources` is the scope of this write:
/// a held time series from one of them that `wanted` does not name is
/// removed, and a held time series from any other source is left alone.
pub fn plan(held: &[TimeSeriesRow], wanted: &[TimeSeriesRow], sources: &BTreeSet<&str>) -> Plan {
    let by_id: HashMap<&str, &TimeSeriesRow> = held
        .iter()
        .map(|r| (r.time_series_id.as_str(), r))
        .collect();
    let mut next_key = held.iter().map(|r| r.ts_key).max().unwrap_or(0) + 1;
    let mut out = Plan::default();
    let mut kept: BTreeSet<&str> = BTreeSet::new();
    for w in wanted {
        kept.insert(&w.time_series_id);
        match by_id.get(w.time_series_id.as_str()) {
            Some(h) if same_row(h, w) => out.unchanged += 1,
            Some(h) => out.replace.push(TimeSeriesRow {
                ts_key: h.ts_key,
                ..w.clone()
            }),
            None => {
                out.replace.push(TimeSeriesRow {
                    ts_key: next_key,
                    ..w.clone()
                });
                next_key += 1;
            }
        }
    }
    out.remove = held
        .iter()
        .filter(|h| sources.contains(h.source_id.as_str()))
        .filter(|h| !kept.contains(h.time_series_id.as_str()))
        .map(|h| h.ts_key)
        .collect();
    out
}

/// Equal in everything but the key, which is each store's own.
fn same_row(held: &TimeSeriesRow, wanted: &TimeSeriesRow) -> bool {
    held == &TimeSeriesRow {
        ts_key: held.ts_key,
        ..wanted.clone()
    }
}

/// Whether the points of `row` have to be written: a new time series or
/// one whose points hash moved. A row whose label alone changed is
/// rewritten without touching its points.
pub fn points_moved(held: &[TimeSeriesRow], row: &TimeSeriesRow) -> bool {
    held.iter()
        .find(|h| h.time_series_id == row.time_series_id)
        .is_none_or(|h| h.points_hash != row.points_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_time_series_schema::TimeKind;

    fn row(ts_key: i64, id: &str, source: &str, hash: &str) -> TimeSeriesRow {
        TimeSeriesRow {
            ts_key,
            time_series_id: id.into(),
            source_id: source.into(),
            entity: "riker".into(),
            metric: id.into(),
            label: id.into(),
            unit: String::new(),
            time_kind: TimeKind::Day,
            points_hash: hash.into(),
            point_count: 1,
            first_t: Some(1),
            last_t: Some(1),
        }
    }

    fn scope<'a>(s: &[&'a str]) -> BTreeSet<&'a str> {
        s.iter().copied().collect()
    }

    #[test]
    fn only_moved_time_series_are_replaced_and_keep_their_key() {
        let held = [row(1, "hr", "g", "a"), row(2, "steps", "g", "b")];
        let wanted = [row(0, "hr", "g", "a"), row(0, "steps", "g", "c")];
        let p = plan(&held, &wanted, &scope(&["g"]));
        assert_eq!(p.unchanged, 1);
        assert_eq!(p.replace, vec![row(2, "steps", "g", "c")]);
        assert!(p.remove.is_empty());
    }

    #[test]
    fn a_new_time_series_gets_a_key_past_every_held_one() {
        let held = [row(4, "hr", "g", "a")];
        let wanted = [
            row(0, "hr", "g", "a"),
            row(0, "sleep", "g", "s"),
            row(0, "vo2", "g", "v"),
        ];
        let p = plan(&held, &wanted, &scope(&["g"]));
        let keys: Vec<i64> = p.replace.iter().map(|r| r.ts_key).collect();
        assert_eq!(keys, vec![5, 6]);
    }

    #[test]
    fn a_time_series_no_longer_wanted_goes_only_from_the_sources_in_scope() {
        let held = [row(1, "hr", "g", "a"), row(2, "temp", "y", "t")];
        let p = plan(&held, &[], &scope(&["g"]));
        assert_eq!(p.remove, vec![1]);
        let p = plan(&held, &[], &scope(&["g", "y"]));
        assert_eq!(p.remove, vec![1, 2]);
    }

    #[test]
    fn a_relabel_rewrites_the_row_but_not_the_points() {
        let held = [row(1, "hr", "g", "a")];
        let mut relabelled = row(0, "hr", "g", "a");
        relabelled.label = "Resting heart rate".into();
        let p = plan(&held, std::slice::from_ref(&relabelled), &scope(&["g"]));
        assert_eq!(p.replace.len(), 1);
        assert!(!points_moved(&held, &p.replace[0]));
        assert!(points_moved(&held, &row(0, "hr", "g", "z")));
        assert!(points_moved(&held, &row(0, "new", "g", "a")));
    }
}
