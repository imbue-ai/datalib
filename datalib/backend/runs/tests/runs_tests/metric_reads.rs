//! What Manage's reads of a few metrics cost: the same whether or not the
//! store also holds many samples of other metrics.

use std::collections::BTreeMap;
use std::path::Path;

use datalib_runs::{InvocationMetricRow, Retention};
use datalib_sql::{test_sql_id, Sql};

const MEASURED: &str = "items";
const OTHER_SERIES: i64 = 20_000;
const FACTOR: u64 = 2;
const SLACK: u64 = 2_000;

/// A store holding a few samples of `MEASURED`, and with `beside`, as
/// many samples and series of another metric as a long sync leaves, in
/// this build's invocation tables and an older build's run tables alike.
async fn store(root: &Path, beside: bool) {
    let keep = Retention {
        max_age_days: 36500,
        ..Retention::default()
    };
    for (request, stamp) in [
        ("r-1", "2026-09-01T00:00:00.000000+00:00"),
        ("r-2", "2026-09-02T00:00:00.000000+00:00"),
    ] {
        let w = crate::request::Request::dated(root, request, stamp, keep);
        for step in ["slack/render_markdown", "mail/render_markdown"] {
            w.metric(InvocationMetricRow {
                step: step.into(),
                name: MEASURED.into(),
                value: 1,
                updated_at_utc: stamp.into(),
                ..Default::default()
            });
        }
    }
    if !beside {
        return;
    }
    let pool = datalib_runs::open_or_create(&datalib_runs::runs_path(root))
        .await
        .unwrap();
    for (table, key, stamp, id) in [
        (
            "invocation_metrics",
            "invocation_id",
            "updated_at_utc",
            "r-1/big/ingest",
        ),
        (
            "invocation_metric_samples",
            "invocation_id",
            "ts_utc",
            "r-2/big/ingest",
        ),
        ("metrics", "run_id", "updated_at_utc", "old-1"),
        ("metric_samples", "run_id", "ts_utc", "old-1"),
    ] {
        // Safe: the table, columns and id are literals, the count a
        // constant.
        Sql::dynamic(test_sql_id!(), format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {OTHER_SERIES}) \
             INSERT INTO {table} ({key}, step, name, labels, value, {stamp}) \
             SELECT '{id}', 'big/ingest', 'rss_bytes', 'n=' || i, i, \
               '2026-09-01T00:00:00.000000+00:00' FROM n"
        )).query()
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;
}

/// The most work each statement did while Manage's reads ran, by
/// statement.
async fn manage_reads(root: &Path, beside: bool) -> BTreeMap<String, u64> {
    store(root, beside).await;
    let recording = datalib_sql_work::record_under(root);
    let latest = datalib_runs::latest_metric(root, MEASURED).await.unwrap();
    let by_run = datalib_runs::metric_by_invocation(root, MEASURED)
        .await
        .unwrap();
    let history = datalib_runs::metric_history(root, MEASURED, "2026-09-01T12:00:00.000000+00:00")
        .await
        .unwrap();
    let snapshot = datalib_runs::snapshot(root).await.unwrap();
    assert_eq!((latest.len(), by_run.len(), history.len()), (2, 4, 4));
    assert_eq!(snapshot.steps.len(), 2);
    let mut most: BTreeMap<String, u64> = BTreeMap::new();
    for ran in recording.take() {
        let sql = ran.sql.split_whitespace().collect::<Vec<_>>().join(" ");
        let steps = most.entry(sql).or_default();
        *steps = (*steps).max(ran.work.vm_steps);
    }
    most
}

/// Guards the Manage refresh that read every sample of every metric
/// (143k on one library) to draw the 598 of one, and every sample the
/// running sync had taken to find its queue's: the metric tables are keyed
/// by run or invocation first, so a read by name scanned them.
#[tokio::test(flavor = "multi_thread")]
async fn manages_metric_reads_cost_the_same_beside_many_samples_of_others() {
    let (alone_dir, beside_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let alone = manage_reads(alone_dir.path(), false).await;
    let beside = manage_reads(beside_dir.path(), true).await;
    for table in [
        "FROM invocation_metric_samples",
        "FROM invocation_metrics",
        "FROM metric_samples",
        "FROM metrics",
    ] {
        assert!(
            beside.keys().any(|sql| sql.contains(table)),
            "no statement read {table}; this test checks nothing: {:#?}",
            beside.keys()
        );
    }
    let grew: Vec<String> = beside
        .iter()
        .filter(|(sql, b)| !alone.get(*sql).is_some_and(|a| **b <= FACTOR * a + SLACK))
        .map(|(sql, b)| format!("alone {:?}, beside {b}: {sql}", alone.get(sql)))
        .collect();
    assert!(grew.is_empty(), "{grew:#?}");
}
