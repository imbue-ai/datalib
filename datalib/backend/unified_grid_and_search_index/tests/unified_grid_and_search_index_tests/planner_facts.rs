//! What SQLite's planner does with a search's statements, as the README
//! beside this crate states it: one test each, so a new SQLite that moves
//! one says which. Most on a table of their own, in a grid index's file.

use sqlx::sqlite::SqlitePool;

use crate::dolt_backend_integration::{explain, unique_db_path, writer};

const BY_SOURCE: &str = "CREATE INDEX by_source ON t (source, stamp, flag, id)";
const BY_CHANNEL: &str = "CREATE INDEX by_channel ON t (channel, stamp, flag, id)";

const GROUPED: &str =
    "SELECT author, count(*) FROM t WHERE source = 'a' AND channel = 'b' GROUP BY 1";

async fn table(indexes: &[&str]) -> SqlitePool {
    let root = unique_db_path().parent().unwrap().to_path_buf();
    let db = writer(&root).await;
    run(
        &db,
        "CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, source, channel, flag, stamp, author)",
    )
    .await;
    for ddl in indexes {
        run(&db, ddl).await;
    }
    db
}

async fn run(db: &SqlitePool, sql: &str) {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(db)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A thousand rows in twenty sources, fifty each; a tenth of them in ten
/// channels, ten each, and the rest in none.
async fn fill(db: &SqlitePool) {
    run(
        db,
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 999) \
         INSERT INTO t (id, source, channel, flag, stamp) \
         SELECT i, 's' || (i % 20), CASE WHEN i % 10 = 0 THEN 'c' || (i / 100) END, i % 2, i FROM n",
    )
    .await;
}

async fn starts_at(db: &SqlitePool, sql: &str) -> String {
    let plan = explain(db, sql, &[]).await;
    plan.iter()
        .find(|d| d.starts_with("SEARCH t "))
        .and_then(|d| d.split("INDEX ").nth(1)?.split_whitespace().next())
        .unwrap_or_else(|| panic!("no index of t: {plan:?}"))
        .to_string()
}

#[tokio::test]
async fn with_no_statistics_two_equalities_tie_and_the_order_of_the_indexes_decides() {
    let source_first = table(&[BY_SOURCE, BY_CHANNEL]).await;
    let channel_first = table(&[BY_CHANNEL, BY_SOURCE]).await;
    assert_ne!(
        starts_at(&source_first, GROUPED).await,
        starts_at(&channel_first, GROUPED).await
    );
}

#[tokio::test]
async fn a_likelihood_on_one_equality_makes_the_other_the_start() {
    let weighed = GROUPED.replace("source = 'a'", "likelihood(source = 'a', 0.1)");
    for indexes in [[BY_SOURCE, BY_CHANNEL], [BY_CHANNEL, BY_SOURCE]] {
        let db = table(&indexes).await;
        assert_eq!(starts_at(&db, &weighed).await, "by_channel");
    }
}

/// So a key most rows leave empty is rated wider than a source, and the
/// search starts at the source.
#[tokio::test]
async fn analyze_counts_the_rows_with_no_value_as_one_value() {
    let db = table(&[BY_CHANNEL, BY_SOURCE]).await;
    fill(&db).await;
    run(&db, "ANALYZE t").await;
    let per_value = |index: &'static str| {
        let db = db.clone();
        async move {
            let stat: String = sqlx::query_scalar("SELECT stat FROM sqlite_stat1 WHERE idx = ?")
                .bind(index)
                .fetch_one(&db)
                .await
                .unwrap();
            let second = stat.split(' ').nth(1).unwrap();
            second.parse::<i64>().unwrap()
        }
    };
    assert_eq!(per_value("by_source").await, 50);
    assert_eq!(
        per_value("by_channel").await,
        91,
        "ten rows, or none at all"
    );
    assert_eq!(starts_at(&db, GROUPED).await, "by_source");
}

/// With `STAT4` the planner would look at the value bound, and tell a big
/// source from a small one.
#[tokio::test]
async fn our_sqlite_keeps_only_an_average_per_column() {
    let db = table(&[]).await;
    let options: Vec<String> = sqlx::query_scalar("PRAGMA compile_options")
        .fetch_all(&db)
        .await
        .unwrap();
    assert!(!options.iter().any(|o| o.contains("STAT4")), "{options:?}");
}

/// Statistics are rows of a table we may write. The planner believes them
/// over the table itself, so an empty store plans as a full one does.
#[tokio::test]
async fn declared_statistics_are_believed_whatever_the_table_holds() {
    for full in [false, true] {
        for indexes in [[BY_SOURCE, BY_CHANNEL], [BY_CHANNEL, BY_SOURCE]] {
            let db = table(&indexes).await;
            if full {
                fill(&db).await;
            }
            run(&db, "ANALYZE sqlite_schema").await;
            run(
                &db,
                "INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES \
                 ('t', 'by_source', '1000000 10000 2 2 1'), \
                 ('t', 'by_channel', '1000000 100 2 2 1')",
            )
            .await;
            run(&db, "ANALYZE sqlite_schema").await;
            assert_eq!(starts_at(&db, GROUPED).await, "by_channel", "full: {full}");
        }
    }
}

#[tokio::test]
async fn an_or_of_two_keys_beside_a_flag_reads_the_flags_index() {
    let root = unique_db_path().parent().unwrap().to_path_buf();
    let db = writer(&root).await;
    let or = "SELECT uuid FROM grid_rows WHERE likelihood(is_document = 1, 0.5) \
         AND (place_uuid = 'a' OR conversation_uuid IN \
              (SELECT uuid FROM grid_rows WHERE uuid = 'a' AND is_document = 1)) \
         ORDER BY touched_at_utc DESC";
    let plan = explain(&db, or, &[]).await;
    let reads_every_document =
        |plan: &[String]| plan.iter().any(|d| d.contains("grid_rows_by_is_document"));
    assert!(reads_every_document(&plan), "{plan:?}");
    let set = "SELECT uuid FROM grid_rows WHERE likelihood(is_document = 1, 0.5) \
         AND rowid IN (SELECT rowid FROM grid_rows WHERE place_uuid = 'a' \
              UNION SELECT rowid FROM grid_rows WHERE conversation_uuid IN \
              (SELECT uuid FROM grid_rows WHERE uuid = 'a' AND is_document = 1)) \
         ORDER BY touched_at_utc DESC";
    let plan = explain(&db, set, &[]).await;
    assert!(!reads_every_document(&plan), "{plan:?}");
}

/// Every member of one set with every member of the other. A `+` takes the
/// column away from the index, and leaves its set to test.
#[tokio::test]
async fn two_sets_one_index_covers_are_tried_in_pairs() {
    let root = unique_db_path().parent().unwrap().to_path_buf();
    let db = writer(&root).await;
    let pairs = "SELECT markdown_uuid FROM grid_rows INDEXED BY grid_rows_by_markdown_source \
         WHERE source_id = 'x' AND rowid IN (SELECT row_id FROM terms WHERE val_id = 1) \
         AND markdown_uuid IN (SELECT markdown_uuid FROM markdowns)";
    let plan = explain(&db, pairs, &[]).await;
    assert!(
        plan[0].ends_with("(markdown_uuid=? AND source_id=? AND rowid=?)"),
        "{plan:?}"
    );
    let tested = pairs.replace("AND markdown_uuid IN", "AND +markdown_uuid IN");
    let plan = explain(&db, &tested, &[]).await;
    assert!(plan[0].starts_with("SCAN "), "{plan:?}");
}
