//! The time-series index's pass: stack every source's time-series store
//! into one, copying only the time series whose points hash moved.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};
use datalib_time_series_schema::{TimeSeriesPointRow, TimeSeriesRow};
use sqlx::SqlitePool;

use crate::{plan, store, Summary};

/// One input: a source's id and its store read at one commit, or `None`
/// when it has nothing readable yet, in which case what the index holds
/// for it is left as it is.
pub struct Source<'a> {
    pub source_id: &'a str,
    pub store: Option<&'a SqlitePool>,
}

/// Bring `index` in line with `sources`, in one transaction, and drop
/// the time series of any source not among them. The caller seals.
pub async fn index(index: &SqlitePool, sources: &[Source<'_>]) -> Result<Summary> {
    let held = store::rows(index).await?;
    let named: BTreeSet<&str> = sources.iter().map(|s| s.source_id).collect();
    let gone: BTreeSet<&str> = held
        .iter()
        .map(|r| r.source_id.as_str())
        .filter(|s| !named.contains(s))
        .collect();
    let mut summary = Summary::default();

    let mut tx = index.begin().await.context("begin the index pass")?;
    let dropped = plan::plan(&held, &[], &gone);
    for &key in &dropped.remove {
        store::remove(&mut tx, key).await?;
    }
    summary.removed += dropped.remove.len();
    let mut next_key = held.iter().map(|r| r.ts_key).max().unwrap_or(0);
    for source in sources {
        let Some(from) = source.store else { continue };
        let wanted: Vec<TimeSeriesRow> = store::rows(from)
            .await
            .with_context(|| format!("read {}'s time series", source.source_id))?
            .into_iter()
            .filter(|r| r.source_id == source.source_id)
            .collect();
        let their_key: HashMap<&str, i64> = wanted
            .iter()
            .map(|r| (r.time_series_id.as_str(), r.ts_key))
            .collect();
        let p = plan::plan(&held, &wanted, &BTreeSet::from([source.source_id]));
        summary.unchanged += p.unchanged;
        summary.removed += p.remove.len();
        for &key in &p.remove {
            store::remove(&mut tx, key).await?;
        }
        // `plan` numbers new time series from the keys held, which every
        // source's plan sees alike; renumber so two sources' never meet.
        for row in p.replace {
            let row = if held.iter().any(|h| h.ts_key == row.ts_key) {
                row
            } else {
                next_key += 1;
                TimeSeriesRow {
                    ts_key: next_key,
                    ..row
                }
            };
            store::write_row(&mut tx, &row).await?;
            summary.written += 1;
            if plan::points_moved(&held, &row) {
                let points: Vec<TimeSeriesPointRow> =
                    store::points(from, their_key[row.time_series_id.as_str()])
                        .await?
                        .into_iter()
                        .map(|p| TimeSeriesPointRow {
                            ts_key: row.ts_key,
                            ..p
                        })
                        .collect();
                store::replace_points(&mut tx, row.ts_key, &points).await?;
                summary.points_replaced += 1;
            }
        }
    }
    tx.commit().await.context("commit the index pass")?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{apply, store};
    use datalib_time_series_schema::{Point, TimeKind, TimeSeries};

    fn series(id: &str, source: &str, points: &[(i64, f64)]) -> TimeSeries {
        TimeSeries {
            time_series_id: format!("{source}/{id}"),
            source_id: source.into(),
            entity: "sensor".into(),
            metric: id.into(),
            label: id.into(),
            unit: "°C".into(),
            time_kind: TimeKind::Instant,
            points: points.iter().map(|&(t, v)| Point::value(t, v)).collect(),
        }
    }

    async fn source_store(dir: &std::path::Path, name: &str, all: &[TimeSeries]) -> SqlitePool {
        let pool = store::open(
            &dir.join(name).join(store::DB),
            datalib_etl::doltlite_raw::StoreKind::TimeSeries,
        )
        .await
        .unwrap();
        apply(&pool, all, &BTreeSet::from([name])).await.unwrap();
        pool
    }

    async fn index_points(ix: &SqlitePool, id: &str) -> Vec<(i64, Option<f64>)> {
        let rows = store::rows(ix).await.unwrap();
        let key = rows.iter().find(|r| r.time_series_id == id).unwrap().ts_key;
        store::points(ix, key)
            .await
            .unwrap()
            .into_iter()
            .map(|p| (p.t, p.value))
            .collect()
    }

    /// Two sources stack without their keys meeting; a second pass after
    /// one source appended copies only that time series; a source no
    /// longer an input loses its time series, and one with no store yet
    /// keeps what the index held.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_index_follows_its_sources_and_copies_only_what_moved() {
        let dir = tempfile::tempdir().unwrap();
        let bedroom = series("temp", "yolink", &[(1, 20.0)]);
        let hall = series("hall", "yolink", &[(1, 19.0)]);
        let weight = series("weight", "garmin", &[(5, 80.0)]);
        let yolink = source_store(dir.path(), "yolink", &[bedroom, hall.clone()]).await;
        let garmin = source_store(dir.path(), "garmin", std::slice::from_ref(&weight)).await;
        let ix = store::open(
            &dir.path().join("index").join(store::DB),
            datalib_etl::doltlite_raw::StoreKind::TimeSeriesIndex,
        )
        .await
        .unwrap();

        let both = |y, g| {
            vec![
                Source {
                    source_id: "yolink",
                    store: y,
                },
                Source {
                    source_id: "garmin",
                    store: g,
                },
            ]
        };
        let first = index(&ix, &both(Some(&yolink), Some(&garmin)))
            .await
            .unwrap();
        assert_eq!((first.written, first.points_replaced), (3, 3));
        let keys: BTreeSet<i64> = store::rows(&ix)
            .await
            .unwrap()
            .iter()
            .map(|r| r.ts_key)
            .collect();
        assert_eq!(keys.len(), 3);
        assert_eq!(
            index_points(&ix, "garmin/weight").await,
            vec![(5, Some(80.0))]
        );

        let warmer = series("temp", "yolink", &[(1, 20.0), (2, 21.5)]);
        apply(&yolink, &[warmer, hall], &BTreeSet::from(["yolink"]))
            .await
            .unwrap();
        let second = index(&ix, &both(Some(&yolink), Some(&garmin)))
            .await
            .unwrap();
        assert_eq!(
            second,
            Summary {
                written: 1,
                points_replaced: 1,
                removed: 0,
                unchanged: 2
            }
        );
        assert_eq!(
            index_points(&ix, "yolink/temp").await,
            vec![(1, Some(20.0)), (2, Some(21.5))]
        );

        let unread = index(&ix, &both(Some(&yolink), None)).await.unwrap();
        assert_eq!(unread.removed, 0);
        assert_eq!(store::rows(&ix).await.unwrap().len(), 3);

        let only_garmin = index(
            &ix,
            &[Source {
                source_id: "garmin",
                store: Some(&garmin),
            }],
        )
        .await
        .unwrap();
        assert_eq!(only_garmin.removed, 2);
        let left: Vec<String> = store::rows(&ix)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.time_series_id)
            .collect();
        assert_eq!(left, vec!["garmin/weight".to_string()]);
        for pool in [yolink, garmin, ix] {
            pool.close().await;
        }
    }
}
