//! The time-series index's pass: copy every source store's rows, and
//! record the commit each store was read at, so a reader of the index
//! reads the points from that store at that commit. No points are
//! copied.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use datalib_time_series_schema::{TimeSeriesRow, TimeSeriesSourceRow};
use sqlx::SqlitePool;

use crate::{plan, store, Summary};

/// One input: a source's id, where its store is under the data root,
/// and the store read at one commit, or `None` when it has nothing
/// readable yet, in which case what the index holds for it is left as
/// it is.
pub struct Source<'a> {
    pub source_id: &'a str,
    pub store: &'a str,
    pub read: Option<Read<'a>>,
}

pub struct Read<'a> {
    pub pool: &'a SqlitePool,
    pub commit_hash: &'a str,
}

/// Bring `index` in line with `sources`, in one transaction, and drop
/// every source not among them. The caller seals.
pub async fn index(index: &SqlitePool, sources: &[Source<'_>]) -> Result<Summary> {
    let held = store::rows(index).await?;
    let held_sources = store::sources(index).await?;
    let named: BTreeSet<&str> = sources.iter().map(|s| s.source_id).collect();
    let gone: BTreeSet<&str> = held
        .iter()
        .map(|r| r.source_id.as_str())
        .chain(held_sources.iter().map(|s| s.source_id.as_str()))
        .filter(|s| !named.contains(s))
        .collect();

    let mut summary = Summary {
        removed: held
            .iter()
            .filter(|r| gone.contains(r.source_id.as_str()))
            .count(),
        ..Summary::default()
    };
    let mut writes = Vec::new();
    for source in sources {
        let Some(read) = &source.read else { continue };
        let theirs: Vec<TimeSeriesRow> = store::rows(read.pool)
            .await
            .with_context(|| format!("read {}'s time series", source.source_id))?
            .into_iter()
            .filter(|r| r.source_id == source.source_id)
            .collect();
        let ours: Vec<TimeSeriesRow> = held
            .iter()
            .filter(|r| r.source_id == source.source_id)
            .cloned()
            .collect();
        let d = plan::rows(&ours, &theirs);
        summary.written += d.write.len();
        summary.removed += d.remove.len();
        summary.unchanged += d.unchanged;
        let record = TimeSeriesSourceRow {
            source_id: source.source_id.to_string(),
            store: source.store.to_string(),
            commit_hash: read.commit_hash.to_string(),
        };
        writes.push((record, d));
    }

    let mut tx = index.begin().await.context("begin the index pass")?;
    for source_id in &gone {
        store::remove_source(&mut tx, source_id).await?;
    }
    for (record, d) in &writes {
        for &key in &d.remove {
            store::remove_row(&mut tx, &record.source_id, key).await?;
        }
        for row in &d.write {
            store::write_row(&mut tx, row).await?;
        }
        if !held_sources.contains(record) {
            store::write_source(&mut tx, record).await?;
        }
    }
    tx.commit().await.context("commit the index pass")?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply;
    use datalib_time_series_schema::{Points, TimeSeries};

    fn series(id: &str, source: &str, points: Points) -> TimeSeries {
        crate::tests::series(id, source, points)
    }

    async fn source_store(dir: &std::path::Path, name: &str, all: &[TimeSeries]) -> SqlitePool {
        let pool = store::open(
            &dir.join(name).join(store::DB),
            datalib_etl::doltlite_raw::StoreKind::TimeSeries,
        )
        .await
        .unwrap();
        apply(&pool, all, name).await.unwrap();
        pool
    }

    fn src<'a>(
        source_id: &'a str,
        pool: Option<&'a SqlitePool>,
        commit_hash: &'a str,
    ) -> Source<'a> {
        Source {
            source_id,
            store: source_id,
            read: pool.map(|pool| Read { pool, commit_hash }),
        }
    }

    /// Two sources' rows stack, keyed by source; a second pass after one
    /// source appended rewrites only that row and moves that source's
    /// commit; a source no longer an input is forgotten, and one with no
    /// store yet keeps what the index held.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_index_follows_its_sources_and_holds_no_points() {
        let dir = tempfile::tempdir().unwrap();
        let bedroom = series("temp", "yolink", Points::floats([(1, 20.0)]));
        let hall = series("hall", "yolink", Points::floats([(1, 19.0)]));
        let weight = series("weight", "garmin", Points::floats([(5, 80.0)]));
        let yolink = source_store(dir.path(), "yolink", &[bedroom, hall.clone()]).await;
        let garmin = source_store(dir.path(), "garmin", std::slice::from_ref(&weight)).await;
        let ix = store::open(
            &dir.path().join("index").join(store::DB),
            datalib_etl::doltlite_raw::StoreKind::TimeSeriesIndex,
        )
        .await
        .unwrap();

        let first = index(
            &ix,
            &[
                src("yolink", Some(&yolink), "y1"),
                src("garmin", Some(&garmin), "g1"),
            ],
        )
        .await
        .unwrap();
        assert_eq!((first.written, first.removed), (3, 0));
        let keys: Vec<(String, i64)> = store::rows(&ix)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.source_id, r.ts_key))
            .collect();
        assert_eq!(
            keys,
            [
                ("garmin".into(), 1),
                ("yolink".into(), 1),
                ("yolink".into(), 2)
            ]
        );
        let has_chunks: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'time_series_chunks'",
        )
        .fetch_one(&ix)
        .await
        .unwrap();
        assert_eq!(has_chunks, 0);

        let warmer = series("temp", "yolink", Points::floats([(1, 20.0), (2, 21.5)]));
        apply(&yolink, &[warmer, hall], "yolink").await.unwrap();
        let second = index(
            &ix,
            &[
                src("yolink", Some(&yolink), "y2"),
                src("garmin", Some(&garmin), "g1"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            (second.written, second.removed, second.unchanged),
            (1, 0, 2)
        );
        let commits: Vec<(String, String)> = store::sources(&ix)
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.source_id, s.commit_hash))
            .collect();
        assert_eq!(
            commits,
            [
                ("garmin".into(), "g1".into()),
                ("yolink".into(), "y2".into())
            ]
        );

        let unread = index(
            &ix,
            &[src("yolink", None, ""), src("garmin", Some(&garmin), "g1")],
        )
        .await
        .unwrap();
        assert_eq!(unread.removed, 0);
        assert_eq!(store::rows(&ix).await.unwrap().len(), 3);

        let only_garmin = index(&ix, &[src("garmin", Some(&garmin), "g1")])
            .await
            .unwrap();
        assert_eq!(only_garmin.removed, 2);
        let left: Vec<String> = store::rows(&ix)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.time_series_id)
            .collect();
        assert_eq!(left, ["garmin/weight".to_string()]);
        assert_eq!(store::sources(&ix).await.unwrap().len(), 1);
        for pool in [yolink, garmin, ix] {
            pool.close().await;
        }
    }
}
