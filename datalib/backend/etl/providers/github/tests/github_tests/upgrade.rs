//! A store written while a PR was held by having a payload and no
//! `last_error`, each search scope kept a cursor, and a cap wrote what
//! it left as skipped opens under this build holding what it held,
//! owing what it had not fetched whole, searching on from where the
//! cursors stood, and with the cap's rows gone: rung 1 of
//! `schema_raw::LADDER`.

use datalib_etl::bulk::bulk_upsert;
use datalib_etl::doltlite_raw::{self as dr, WirePayloadRow as _};
use datalib_etl_github::ingest::schema_raw::{
    pr_pk, IssueCommentRow, PrReviewCommentRow, PrReviewRow, PullRequestRow, SelfIdentityRow,
    DATA_TABLES, ISSUE_COMMENTS_BY_PR_INDEX_DDL, PR_REVIEWS_BY_PR_INDEX_DDL,
    PR_REVIEW_COMMENTS_BY_PR_INDEX_DDL, PULL_REQUESTS_BY_REPO_INDEX_DDL,
};
use datalib_etl_github::ingest::{db_path_for, RawDb};
use datalib_etl_web::owed;
use serde_json::json;

use crate::support::*;

/// The sidecar as the build before `held_version` wrote it.
fn old_bookkeeping_ddl(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table}_bookkeeping (id TEXT PRIMARY KEY, \
         fetched_at_utc TEXT NULL, attempt_count INTEGER NOT NULL, \
         last_attempt_at_utc TEXT NULL, last_error TEXT NULL, \
         volatile_payload TEXT NULL, tz_offset TEXT NULL)"
    )
}

const T1: &str = "2369-04-10T00:00:00Z";
const T2: &str = "2369-04-12T00:00:00Z";

async fn strings(pool: &sqlx::SqlitePool, sql: &'static str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(sql)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_from_before_the_listing_opens_holding_what_it_had() {
    let d = tempfile::tempdir().unwrap();
    let path = db_path_for(d.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut ddl = vec![
        SelfIdentityRow::ddl(),
        PullRequestRow::ddl(),
        PULL_REQUESTS_BY_REPO_INDEX_DDL.to_string(),
        IssueCommentRow::ddl(),
        ISSUE_COMMENTS_BY_PR_INDEX_DDL.to_string(),
        PrReviewRow::ddl(),
        PR_REVIEWS_BY_PR_INDEX_DDL.to_string(),
        PrReviewCommentRow::ddl(),
        PR_REVIEW_COMMENTS_BY_PR_INDEX_DDL.to_string(),
    ];
    ddl.extend(DATA_TABLES.iter().map(|t| old_bookkeeping_ddl(t)));
    let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
    let pool = dr::open(&path, &ddl).await.unwrap();
    let key = |n: u32| pr_pk(REPO, n);
    // PR 1 fetched whole; PR 2 stored, then its reviews would not list;
    // PR 3 never came at all; PR 4 was past a run's cap.
    for (num, at) in [(1, T1), (2, T2)] {
        bulk_upsert(
            &pool,
            &[PullRequestRow::from_payload(
                REPO,
                num,
                &json!({"number": num, "title": "Safety protocols", "updated_at": at}),
            )
            .unwrap()],
        )
        .await
        .unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    dr::record_object_error(
        &mut tx,
        "pull_requests",
        &key(2),
        "could not list its reviews: HTTP 500",
    )
    .await
    .unwrap();
    dr::record_object_error(&mut tx, "pull_requests", &key(3), "HTTP 500")
        .await
        .unwrap();
    dr::record_object_skipped(
        &mut tx,
        "pull_requests",
        &key(4),
        datalib_problems::Reason::OverSizeLimit,
        "over this run's cap of 1 PRs",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    dr::upsert_scope_state(&pool, "author:@me", "2369-04-15T00:00:00.000000+00:00")
        .await
        .unwrap();
    // The table an older build declared, and its record.
    sqlx::query("CREATE TABLE IF NOT EXISTS sync_scope_config (scope TEXT PRIMARY KEY, config TEXT NOT NULL, updated_at_utc TEXT NOT NULL, tz_offset TEXT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sync_scope_config VALUES ('github:download', '{\"refresh_window_days\":30}', '2369-04-15T00:00:00Z', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    dr::commit_run(&pool, "an earlier build").await.unwrap();
    pool.close().await;

    let db = RawDb::open(&path).await.expect("the rung carries it");
    let pool = db.pool();
    let held: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT id, fetched_at_utc IS NOT NULL, held_version \
         FROM pull_requests_bookkeeping ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        held,
        [
            (key(1), true, Some(T1.to_string())),
            (key(2), true, None),
            (key(3), false, None),
            (key(4), false, None),
        ],
        "a PR fetched whole is held at the updated_at its record carries; a stale one is not"
    );
    let listed: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, updated_at FROM listed_change_requests ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(
        listed,
        [
            (key(1), Some(T1.to_string())),
            (key(2), Some(T2.to_string())),
            (key(3), None),
            (key(4), None),
        ],
        "every PR the store knew of is listed, a stub at no version"
    );
    let owed: Vec<String> = owed::owed(
        pool,
        "pull_requests",
        listed
            .into_iter()
            .map(|(id, at)| owed::Listed::new(id, at))
            .collect(),
    )
    .await
    .unwrap()
    .into_iter()
    .map(|l| l.key)
    .collect();
    assert_eq!(owed, [key(2), key(3), key(4)]);
    let spans: Vec<(String, String, String)> =
        sqlx::query_as("SELECT scope, lo, hi FROM coverage ORDER BY scope")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(
        spans,
        [(
            "search:author:@me".to_string(),
            String::new(),
            "2369-04-15T00:00:00Z".to_string()
        )],
        "the cursor is the top of a span from the beginning of time"
    );
    assert_eq!(
        strings(pool, "SELECT scope_key FROM problems ORDER BY scope_key").await,
        [
            format!("pull_requests:{}", key(2)),
            format!("pull_requests:{}", key(3)),
        ],
        "what a cap left needs no row; what failed keeps its row"
    );
    assert!(strings(pool, "SELECT scope FROM sync_scope_state")
        .await
        .is_empty());
    let retired: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sync_scope_config'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(retired, 0, "the retired table is dropped on open");
    assert_eq!(
        strings(
            pool,
            "SELECT value FROM _datalib_meta WHERE key = 'schema_version'"
        )
        .await,
        ["1"]
    );
    db.close().await;
}
