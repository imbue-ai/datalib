//! The time-series store: a source's (`<group>/render_time_series`) and
//! the index's (`unified_time_series_index/time_series_index`) are the
//! same two tables, written the same way. `plan` decides what a write
//! has to change; `store` opens the file and runs the statements.
//! `docs/dev/plans/time_series.md` is the design.

pub mod index;
pub mod plan;
pub mod read;
pub mod store;

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use datalib_time_series_schema::{TimeSeries, TimeSeriesRow};
use sqlx::SqlitePool;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Time series whose row was written.
    pub written: usize,
    /// Of those, the ones whose points were replaced.
    pub points_replaced: usize,
    pub removed: usize,
    pub unchanged: usize,
}

/// Make the store hold exactly `wanted` for the sources in `sources`,
/// in one transaction, replacing only the time series that moved. The
/// caller seals it (`commit_run`).
pub async fn apply(
    pool: &SqlitePool,
    wanted: &[TimeSeries],
    sources: &BTreeSet<&str>,
) -> Result<Summary> {
    let held = store::rows(pool).await?;
    let wanted_rows: Vec<TimeSeriesRow> = wanted.iter().map(|s| s.row(0)).collect();
    let plan = plan::plan(&held, &wanted_rows, sources);
    let by_id: std::collections::HashMap<&str, &TimeSeries> = wanted
        .iter()
        .map(|s| (s.time_series_id.as_str(), s))
        .collect();

    let mut tx = pool.begin().await.context("begin a time-series write")?;
    let mut summary = Summary {
        unchanged: plan.unchanged,
        removed: plan.remove.len(),
        ..Summary::default()
    };
    for &key in &plan.remove {
        store::remove(&mut tx, key).await?;
    }
    for row in &plan.replace {
        store::write_row(&mut tx, row).await?;
        summary.written += 1;
        if plan::points_moved(&held, row) {
            let points: Vec<_> = by_id[row.time_series_id.as_str()]
                .point_rows(row.ts_key)
                .collect();
            store::replace_points(&mut tx, row.ts_key, &points).await?;
            summary.points_replaced += 1;
        }
    }
    tx.commit().await.context("commit a time-series write")?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_time_series_schema::{Point, TimeKind};

    fn series(id: &str, source: &str, points: &[(i64, f64)]) -> TimeSeries {
        TimeSeries {
            time_series_id: id.into(),
            source_id: source.into(),
            entity: "riker".into(),
            metric: id.into(),
            label: id.into(),
            unit: "kg".into(),
            time_kind: TimeKind::Day,
            points: points.iter().map(|&(t, v)| Point::value(t, v)).collect(),
        }
    }

    fn scope<'a>(s: &[&'a str]) -> BTreeSet<&'a str> {
        s.iter().copied().collect()
    }

    /// A second render that appends to one time series must replace only
    /// that one, and the store must read back exactly what was wanted.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_write_replaces_only_what_moved() {
        let dir = tempfile::tempdir().unwrap();
        let pool = store::open(&dir.path().join(store::DB), doltlite_raw_kind())
            .await
            .unwrap();
        let weight = series("weight", "g", &[(1, 80.0), (2, 80.5)]);
        let hr = series("hr", "g", &[(1, 48.0)]);
        let first = apply(&pool, &[weight.clone(), hr.clone()], &scope(&["g"]))
            .await
            .unwrap();
        assert_eq!((first.written, first.points_replaced), (2, 2));

        let longer = series("weight", "g", &[(1, 80.0), (2, 80.5), (3, 79.9)]);
        let second = apply(&pool, &[longer.clone(), hr.clone()], &scope(&["g"]))
            .await
            .unwrap();
        assert_eq!(
            second,
            Summary {
                written: 1,
                points_replaced: 1,
                removed: 0,
                unchanged: 1
            }
        );

        let rows = store::rows(&pool).await.unwrap();
        let w = rows.iter().find(|r| r.time_series_id == "weight").unwrap();
        assert_eq!(w, &longer.row(w.ts_key));
        let points = store::points(&pool, w.ts_key).await.unwrap();
        assert_eq!(points, longer.point_rows(w.ts_key).collect::<Vec<_>>());

        let third = apply(&pool, &[longer], &scope(&["g"])).await.unwrap();
        assert_eq!(third.removed, 1);
        assert_eq!(store::rows(&pool).await.unwrap().len(), 1);
        pool.close().await;
    }

    /// A text reading and a missing value survive the JSON the points are
    /// inserted through.
    #[tokio::test(flavor = "multi_thread")]
    async fn text_and_missing_values_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let pool = store::open(&dir.path().join(store::DB), doltlite_raw_kind())
            .await
            .unwrap();
        let mut stages = series("sleep_stage", "g", &[]);
        stages.points = vec![
            Point {
                t: 10,
                value: None,
                text: Some("rem \"deep\"".into()),
            },
            Point {
                t: 20,
                value: Some(0.5),
                text: None,
            },
            Point {
                t: 30,
                value: None,
                text: None,
            },
        ];
        apply(&pool, std::slice::from_ref(&stages), &scope(&["g"]))
            .await
            .unwrap();
        let key = store::rows(&pool).await.unwrap()[0].ts_key;
        assert_eq!(
            store::points(&pool, key).await.unwrap(),
            stages.point_rows(key).collect::<Vec<_>>()
        );
        pool.close().await;
    }

    fn doltlite_raw_kind() -> datalib_etl::doltlite_raw::StoreKind {
        datalib_etl::doltlite_raw::StoreKind::TimeSeries
    }
}
