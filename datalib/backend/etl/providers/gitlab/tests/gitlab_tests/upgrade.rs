//! A store written while an MR was held by having a payload and no
//! `last_error`, each listing scope kept a cursor, and a cap wrote what
//! it left as skipped opens under this build holding what it held,
//! owing what it had not fetched whole, listing on from where the
//! cursors stood, and with the cap's rows gone: rung 1 of
//! `schema_raw::LADDER`.

use datalib_etl::bulk::bulk_upsert;
use datalib_etl::doltlite_raw::{self as dr, WirePayloadRow as _};
use datalib_etl_gitlab::ingest::schema_raw::{
    mr_pk_recipe, DiscussionRow, MergeRequestRow, SelfIdentityRow, DATA_TABLES,
    DISCUSSIONS_BY_MR_INDEX_DDL, MERGE_REQUESTS_BY_PROJ_INDEX_DDL,
};
use datalib_etl_gitlab::ingest::{db_path_for, RawDb};
use datalib_etl_web::owed;

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

const T1: &str = "2369-04-10T00:00:00.000Z";
const T2: &str = "2369-04-12T00:00:00.000Z";

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
        MergeRequestRow::ddl(),
        MERGE_REQUESTS_BY_PROJ_INDEX_DDL.to_string(),
        DiscussionRow::ddl(),
        DISCUSSIONS_BY_MR_INDEX_DDL.to_string(),
    ];
    ddl.extend(DATA_TABLES.iter().map(|t| old_bookkeeping_ddl(t)));
    let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
    let pool = dr::open(&path, &ddl).await.unwrap();
    let key = |n: u32| mr_pk_recipe(PROJECT, n);
    // MR 1 fetched whole; MR 2 stored, then its discussions would not
    // list; MR 3 never came at all; MR 4 was past a run's cap.
    for (iid, at) in [(1, T1), (2, T2)] {
        bulk_upsert(
            &pool,
            &[
                MergeRequestRow::from_payload(
                    PROJECT,
                    iid,
                    &mr(iid as u64, at, "Safety protocols"),
                )
                .unwrap(),
            ],
        )
        .await
        .unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    dr::record_object_error(
        &mut tx,
        "merge_requests",
        &key(2),
        "could not list its discussions: HTTP 500",
    )
    .await
    .unwrap();
    dr::record_object_error(&mut tx, "merge_requests", &key(3), "HTTP 500")
        .await
        .unwrap();
    dr::record_object_skipped(
        &mut tx,
        "merge_requests",
        &key(4),
        datalib_problems::Reason::OverSizeLimit,
        "over this run's cap of 1 MRs",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    dr::upsert_scope_state(&pool, "reviewer", "2369-04-15T00:00:00.000000+00:00")
        .await
        .unwrap();
    // The table an older build declared, and its record.
    sqlx::query("CREATE TABLE IF NOT EXISTS sync_scope_config (scope TEXT PRIMARY KEY, config TEXT NOT NULL, updated_at_utc TEXT NOT NULL, tz_offset TEXT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sync_scope_config VALUES ('gitlab:download', '{\"refresh_window_days\":30}', '2369-04-15T00:00:00Z', NULL)",
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
         FROM merge_requests_bookkeeping ORDER BY id",
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
        "an MR fetched whole is held at the updated_at its record carries; a stale one is not"
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
        "every MR the store knew of is listed, a stub at no version"
    );
    let owed: Vec<String> = owed::owed(
        pool,
        "merge_requests",
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
            "search:reviewer".to_string(),
            String::new(),
            "2369-04-15T00:00:00.000Z".to_string()
        )],
        "the cursor is the top of a span from the beginning of time"
    );
    assert_eq!(
        strings(pool, "SELECT scope_key FROM problems ORDER BY scope_key").await,
        [
            format!("merge_requests:{}", key(2)),
            format!("merge_requests:{}", key(3)),
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
