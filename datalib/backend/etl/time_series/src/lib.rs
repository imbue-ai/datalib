//! The time-series stores: a source's (`<group>/render_time_series`),
//! which holds its time series and their points as chunks, and the
//! index (`unified_time_series_index/time_series_index`), which holds
//! every source's rows and the commit of the source store to read the
//! points at. `chunk` encodes points, `plan` decides what a write has to
//! change, `store` runs the statements, `read` answers a range.
//! `docs/dev/time_series.md` is the design.

pub mod chunk;
pub mod index;
pub mod plan;
pub mod read;
pub mod store;

use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use datalib_time_series_schema::{TimeSeries, TimeSeriesRow, Unit};
use sqlx::SqlitePool;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Time series whose row was written.
    pub written: usize,
    pub removed: usize,
    /// Time series of which nothing was written.
    pub unchanged: usize,
    pub chunks_written: usize,
    pub chunks_removed: usize,
}

/// Why `wanted` cannot be stored as `source_id`'s time series, if it
/// cannot: a time series of another source, an id or a name used twice,
/// a unit that is not UCUM, points out of order.
pub fn problem(wanted: &[TimeSeries], source_id: &str) -> Option<String> {
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for s in wanted {
        let what = &s.time_series_id;
        if s.source_id != source_id {
            return Some(format!("{what} is {}'s, not {source_id}'s", s.source_id));
        }
        if !ids.insert(what) {
            return Some(format!("{what} is there twice"));
        }
        if !names.insert((&s.about_uuid, &s.name)) {
            return Some(format!("the name {:?} is used twice", s.name));
        }
        if let Err(e) = Unit::parse(&s.unit) {
            return Some(format!("{what}: {e}"));
        }
        if let Some(p) = s.points.problem() {
            return Some(format!("{what}: {p}"));
        }
    }
    None
}

/// Make the store hold exactly `wanted`, `source_id`'s time series, in
/// one transaction, writing only the rows and chunks that differ. The
/// caller seals it (`commit_run`).
pub async fn apply(pool: &SqlitePool, wanted: &[TimeSeries], source_id: &str) -> Result<Summary> {
    if let Some(p) = problem(wanted, source_id) {
        bail!("cannot store {source_id}'s time series: {p}");
    }
    let held = store::rows(pool).await?;
    let wanted_rows: Vec<TimeSeriesRow> = wanted.iter().map(|s| s.row(0)).collect();
    let keyed = plan::assign_keys(&held, &wanted_rows);
    let rows = plan::rows(&held, &keyed);

    // Read every held chunk before the transaction takes the pool's one
    // connection.
    let mut chunk_plans = Vec::with_capacity(wanted.len());
    for (series, row) in wanted.iter().zip(&keyed) {
        let held_chunks = store::all_chunks(pool, row.ts_key).await?;
        let want = chunk::cut(row.ts_key, &series.points, chunk::CHUNK_POINTS);
        chunk_plans.push((row.ts_key, plan::chunks(&held_chunks, &want)));
    }

    let mut summary = Summary {
        written: rows.write.len(),
        removed: rows.remove.len(),
        ..Summary::default()
    };
    let mut tx = pool.begin().await.context("begin a time-series write")?;
    for &key in &rows.remove {
        store::remove_chunks(&mut tx, key).await?;
        store::remove_row(&mut tx, source_id, key).await?;
    }
    for row in &rows.write {
        store::write_row(&mut tx, row).await?;
    }
    for (ts_key, d) in &chunk_plans {
        for &first_t in &d.remove {
            store::remove_chunk(&mut tx, *ts_key, first_t).await?;
        }
        for c in &d.write {
            store::write_chunk(&mut tx, c).await?;
        }
        summary.chunks_written += d.write.len();
        summary.chunks_removed += d.remove.len();
        let row_written = rows.write.iter().any(|w| w.ts_key == *ts_key);
        if !row_written && d.write.is_empty() && d.remove.is_empty() {
            summary.unchanged += 1;
        }
    }
    tx.commit().await.context("commit a time-series write")?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_time_series_schema::{Points, TimeKind};

    pub(crate) fn series(id: &str, source: &str, points: Points) -> TimeSeries {
        TimeSeries {
            time_series_id: format!("{source}/{id}"),
            source_id: source.into(),
            about_uuid: None,
            about_name: None,
            name: id.into(),
            human_name: id.into(),
            unit: "kg".into(),
            time_kind: TimeKind::Day,
            points,
        }
    }

    async fn points_of(pool: &SqlitePool, key: i64) -> Points {
        let chunks = store::all_chunks(pool, key).await.unwrap();
        read::decoded(&chunks).unwrap()
    }

    /// A second render that appends to one time series must write only
    /// its last chunk, and the store must read back exactly what was
    /// wanted.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_write_writes_only_the_chunk_that_moved() {
        let dir = tempfile::tempdir().unwrap();
        let pool = store::open(&dir.path().join(store::DB), kind())
            .await
            .unwrap();
        let n = chunk::CHUNK_POINTS as i64 * 2 + 5;
        let weight = series(
            "weight",
            "g",
            Points::floats((0..n).map(|t| (t, 80.0 + t as f64 / 10.0))),
        );
        let hr = series("hr", "g", Points::integers([(1, 48)]));
        let first = apply(&pool, &[weight.clone(), hr.clone()], "g")
            .await
            .unwrap();
        assert_eq!((first.written, first.chunks_written), (2, 4));

        let longer = series(
            "weight",
            "g",
            Points::floats((0..=n).map(|t| (t, 80.0 + t as f64 / 10.0))),
        );
        let second = apply(&pool, &[longer.clone(), hr.clone()], "g")
            .await
            .unwrap();
        assert_eq!(
            second,
            Summary {
                written: 1,
                removed: 0,
                unchanged: 1,
                chunks_written: 1,
                chunks_removed: 0,
            }
        );

        let rows = store::rows(&pool).await.unwrap();
        let w = rows.iter().find(|r| r.name == "weight").unwrap();
        assert_eq!(w, &longer.row(w.ts_key));
        assert_eq!(points_of(&pool, w.ts_key).await, longer.points);

        let again = apply(&pool, &[longer.clone(), hr.clone()], "g")
            .await
            .unwrap();
        assert_eq!(
            (again.written, again.unchanged, again.chunks_written),
            (0, 2, 0)
        );

        let third = apply(&pool, &[longer], "g").await.unwrap();
        assert_eq!((third.removed, third.chunks_removed), (1, 0));
        assert_eq!(store::rows(&pool).await.unwrap().len(), 1);
        let hr_key = rows.iter().find(|r| r.name == "hr").unwrap().ts_key;
        assert!(store::all_chunks(&pool, hr_key).await.unwrap().is_empty());
        pool.close().await;
    }

    #[test]
    fn what_cannot_be_stored_is_named() {
        let ok = series("weight", "g", Points::floats([(1, 80.0)]));
        assert_eq!(problem(std::slice::from_ref(&ok), "g"), None);
        assert!(problem(std::slice::from_ref(&ok), "y")
            .unwrap()
            .contains("g's, not y's"));
        assert!(problem(&[ok.clone(), ok.clone()], "g")
            .unwrap()
            .contains("twice"));
        let bpm = TimeSeries {
            unit: "bpm".into(),
            ..ok.clone()
        };
        assert!(problem(&[bpm], "g").unwrap().contains("bpm"));
        let backwards = TimeSeries {
            points: Points::floats([(2, 1.0), (1, 1.0)]),
            ..ok
        };
        assert!(problem(&[backwards], "g").unwrap().contains("followed by"));
    }

    pub(crate) fn kind() -> datalib_etl::doltlite_raw::StoreKind {
        datalib_etl::doltlite_raw::StoreKind::TimeSeries
    }
}
