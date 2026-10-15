//! What a reader asks a time-series store for: one time series' points
//! in a range, whole or, past a point budget, as buckets of min, max and
//! mean. The time-series applet serves these.

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::{Row, SqlitePool};

/// The points in a range, as they are stored.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RawPoint {
    pub t: i64,
    pub value: Option<f64>,
    pub text: Option<String>,
}

/// The points of one bucket, summarised: `t` is the first point's.
/// A bucket of text-only points has no numbers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Bucket {
    pub t: i64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub mean: Option<f64>,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Points {
    Raw { points: Vec<RawPoint> },
    Buckets { width: i64, buckets: Vec<Bucket> },
}

/// The bucket width that cuts `[from, to]` into at most `budget` buckets:
/// at least 1, so a range narrower than the budget is one point a bucket.
pub fn bucket_width(from: i64, to: i64, budget: i64) -> i64 {
    let span = to.saturating_sub(from).saturating_add(1).max(1);
    let budget = budget.max(1);
    span / budget + i64::from(span % budget != 0)
}

pub async fn count(pool: &SqlitePool, ts_key: i64, from: i64, to: i64) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM time_series_points WHERE ts_key = ? AND t BETWEEN ? AND ?",
    )
    .bind(ts_key)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await
    .context("count a time series' points")
}

/// `ts_key`'s points in `[from, to]`: every one when there are at most
/// `budget`, else `budget` buckets or fewer.
pub async fn points(
    pool: &SqlitePool,
    ts_key: i64,
    from: i64,
    to: i64,
    budget: i64,
) -> Result<Points> {
    if count(pool, ts_key, from, to).await? <= budget {
        let rows = sqlx::query(
            "SELECT t, value, text FROM time_series_points \
             WHERE ts_key = ? AND t BETWEEN ? AND ? ORDER BY t",
        )
        .bind(ts_key)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await
        .context("read a time series' points")?;
        let points = rows
            .iter()
            .map(|r| {
                Ok(RawPoint {
                    t: r.try_get("t")?,
                    value: r.try_get("value")?,
                    text: r.try_get("text")?,
                })
            })
            .collect::<Result<_>>()?;
        return Ok(Points::Raw { points });
    }
    let width = bucket_width(from, to, budget);
    let rows = sqlx::query(
        "SELECT MIN(t) AS t, MIN(value) AS min, MAX(value) AS max, AVG(value) AS mean, \
                COUNT(*) AS count \
         FROM time_series_points WHERE ts_key = ? AND t BETWEEN ? AND ? \
         GROUP BY (t - ?) / ? ORDER BY 1",
    )
    .bind(ts_key)
    .bind(from)
    .bind(to)
    .bind(from)
    .bind(width)
    .fetch_all(pool)
    .await
    .context("bucket a time series' points")?;
    let buckets = rows
        .iter()
        .map(|r| {
            Ok(Bucket {
                t: r.try_get("t")?,
                min: r.try_get("min")?,
                max: r.try_get("max")?,
                mean: r.try_get("mean")?,
                count: r.try_get("count")?,
            })
        })
        .collect::<Result<_>>()?;
    Ok(Points::Buckets { width, buckets })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use datalib_time_series_schema::{Point, TimeKind, TimeSeries};

    use super::*;
    use crate::{apply, store};

    #[test]
    fn a_width_never_makes_more_buckets_than_the_budget() {
        for (from, to, budget) in [(0, 99, 10), (0, 100, 10), (5, 5, 10), (0, 1_000_003, 7)] {
            let w = bucket_width(from, to, budget);
            let buckets = (to - from) / w + 1;
            assert!(buckets <= budget, "{from}..{to} / {budget} → width {w}");
            assert!(w >= 1);
        }
        assert_eq!(bucket_width(0, 9, 100), 1);
        assert_eq!(bucket_width(0, 9, 0), 10);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_range_past_the_budget_comes_back_as_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let pool = store::open(
            &dir.path().join(store::DB),
            datalib_etl::doltlite_raw::StoreKind::TimeSeries,
        )
        .await
        .unwrap();
        let s = TimeSeries {
            time_series_id: "temp".into(),
            source_id: "yolink".into(),
            entity: "bridge".into(),
            metric: "temperature".into(),
            label: "Temperature".into(),
            unit: "°C".into(),
            time_kind: TimeKind::Instant,
            points: (0..100).map(|i| Point::value(i, i as f64)).collect(),
        };
        apply(&pool, std::slice::from_ref(&s), &BTreeSet::from(["yolink"]))
            .await
            .unwrap();
        let key = store::rows(&pool).await.unwrap()[0].ts_key;

        let Points::Raw { points } = super::points(&pool, key, 10, 19, 50).await.unwrap() else {
            panic!("ten points under a budget of fifty come back whole")
        };
        assert_eq!(points.len(), 10);
        assert_eq!(points[0].t, 10);

        let Points::Buckets { width, buckets } =
            super::points(&pool, key, 0, 99, 10).await.unwrap()
        else {
            panic!("a hundred points over a budget of ten are bucketed")
        };
        assert_eq!((width, buckets.len()), (10, 10));
        assert_eq!(
            buckets[1],
            Bucket {
                t: 10,
                min: Some(10.0),
                max: Some(19.0),
                mean: Some(14.5),
                count: 10
            }
        );
        pool.close().await;
    }
}
