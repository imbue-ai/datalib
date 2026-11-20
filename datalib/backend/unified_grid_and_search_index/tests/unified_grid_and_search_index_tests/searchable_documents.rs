//! How a search lists the searchable documents it could find, read on the
//! fixture's grid index as the step builds it.

use datalib_unified_grid_and_search_index::dolt_repo::documents_with_pages_sql;
use std::collections::BTreeSet;

async fn fixture_index() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let td = tempfile::tempdir().unwrap();
    datalib_search_fixture::copy_grid_index(td.path());
    let path = datalib_runtime::layout::grid_index_db(td.path());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(datalib_runtime::plain_sqlite::uri(&path))
                .read_only(true),
        )
        .await
        .expect("open the fixture's grid index");
    (td, pool)
}

async fn plan(pool: &sqlx::SqlitePool, sql: &str) -> Vec<String> {
    // Audited: statements of ours with no values.
    let rows: Vec<(i64, i64, i64, String)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
            .fetch_all(pool)
            .await
            .unwrap();
    rows.into_iter().map(|r| r.3).collect()
}

/// Guards the count of what search by meaning cannot reach, which every
/// first page of a search waits for: it looked each grid row's document up
/// in `markdowns`, reading that whole row for one flag, 8 s on a library of
/// 1.2M rows. The searchable documents come from their index, once, and the
/// grid is read from an index alone; a filter still narrows the grid.
#[tokio::test]
async fn the_searchable_documents_are_listed_from_their_indexes() {
    let (_td, pool) = fixture_index().await;
    let unfiltered = documents_with_pages_sql("grid_rows", "markdowns", "", " WHERE");
    let filtered = documents_with_pages_sql(
        "grid_rows",
        "markdowns",
        " WHERE source_id = 'enterprise'",
        " AND",
    );
    for sql in [&unfiltered, &filtered] {
        let plan = plan(&pool, sql).await;
        assert!(
            plan.iter().any(|p| p
                == "SEARCH markdowns USING COVERING INDEX markdowns_searchable (searchable=?)"),
            "{plan:#?}"
        );
        assert!(!plan.iter().any(|p| p.contains("CORRELATED")), "{plan:#?}");
        assert!(
            !plan.iter().any(|p| p.starts_with("SCAN markdowns")),
            "{plan:#?}"
        );
    }
    let plan = plan(&pool, &unfiltered).await;
    assert!(
        plan.iter()
            .any(|p| p.contains("grid_rows USING COVERING INDEX grid_rows_by_markdown_source")),
        "{plan:#?}"
    );
}

/// The listing is the documents it always was: every searchable one with
/// a grid row, under each source its rows are filed under.
#[tokio::test]
async fn the_listing_holds_the_same_documents_as_a_look_up_per_row() {
    let (_td, pool) = fixture_index().await;
    let read = |sql: String| {
        let pool = pool.clone();
        async move {
            // Audited: statements of ours with no values.
            sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql))
                .fetch_all(&pool)
                .await
                .unwrap()
                .into_iter()
                .collect::<BTreeSet<_>>()
        }
    };
    let listed = read(documents_with_pages_sql(
        "grid_rows",
        "markdowns",
        "",
        " WHERE",
    ))
    .await;
    let looked_up = read(
        "SELECT DISTINCT markdown_uuid, source_id FROM grid_rows WHERE EXISTS \
         (SELECT 1 FROM markdowns WHERE markdowns.markdown_uuid = grid_rows.markdown_uuid \
          AND markdowns.searchable = 1)"
            .to_string(),
    )
    .await;
    assert!(!listed.is_empty(), "the fixture has searchable documents");
    assert_eq!(listed, looked_up);
}
