//! What SQLite's planner is told of each of `grid_rows`' indexes: how many
//! rows one value is taken to have. Written into `sqlite_stat1`, the same in
//! every library, where `ANALYZE` would write what it measured. Why:
//! docs/dev/search.md § "How SQLite chooses".

use std::sync::OnceLock;

use anyhow::{Context, Result};

/// How many rows one value of an index's first column has, against the
/// others. Only their order matters, so they are far apart, and all far
/// under the rows the table is said to have: near that, SQLite would rather
/// read an index whole than search it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rank {
    /// A flag keeps a large share of the library.
    Flag,
    /// A source can be half of it.
    Source,
    Many,
    Some,
    Few,
    One,
}

const ROWS: u32 = 1_000_000;

impl Rank {
    fn rows_per_value(self) -> u32 {
        match self {
            Rank::Flag => ROWS / 10,
            Rank::Source => ROWS / 100,
            Rank::Many => ROWS / 1_000,
            Rank::Some => ROWS / 10_000,
            Rank::Few => ROWS / 100_000,
            Rank::One => 1,
        }
    }
}

/// Every index of `grid_rows`, the primary key's among them; a test holds
/// the list to the indexes made.
const RANKS: &[(&str, Rank)] = &[
    ("sqlite_autoindex_grid_rows_1", Rank::One),
    ("grid_rows_by_touched", Rank::One),
    ("grid_rows_by_source_id", Rank::Source),
    ("grid_rows_by_is_document", Rank::Flag),
    ("grid_rows_by_kind", Rank::Many),
    ("grid_rows_by_account", Rank::Many),
    ("grid_rows_by_project", Rank::Many),
    ("grid_rows_by_diff_status", Rank::Many),
    ("grid_rows_by_channel", Rank::Some),
    ("grid_rows_by_place", Rank::Some),
    ("grid_rows_by_author", Rank::Some),
    ("grid_rows_by_conversation", Rank::Few),
    ("grid_rows_by_subject", Rank::Few),
    ("grid_rows_by_notion_page", Rank::Few),
    ("grid_rows_by_h3", Rank::Few),
    ("grid_rows_by_markdown", Rank::Few),
    ("grid_rows_by_markdown_source", Rank::Few),
    ("grid_rows_by_upstream_id", Rank::One),
];

/// How many columns the index is over: one, for the primary key's.
fn columns(index: &str) -> usize {
    super::indexes()
        .find(|ddl| super::index_name(ddl) == Some(index))
        .and_then(|ddl| ddl.rsplit('(').next())
        .map_or(1, |columns| columns.split(',').count())
}

/// The statements that fill `sqlite_stat1`, which the index's shape counts
/// among its indexes.
pub(super) fn statements() -> &'static [String] {
    static STATEMENTS: OnceLock<Vec<String>> = OnceLock::new();
    STATEMENTS.get_or_init(|| {
        RANKS
            .iter()
            .map(|(index, rank)| {
                let rows = rank.rows_per_value();
                // A column after the first narrows to one row or two; left
                // unsaid, SQLite would take it to narrow to nine.
                let stat: Vec<String> = [ROWS, rows]
                    .into_iter()
                    .chain(std::iter::repeat_n(rows.min(2), columns(index) - 1))
                    .map(|n| n.to_string())
                    .collect();
                format!(
                    "INSERT INTO sqlite_stat1 (tbl, idx, stat) \
                     VALUES ('grid_rows', '{index}', '{}')",
                    stat.join(" ")
                )
            })
            .collect()
    })
}

/// Fill `sqlite_stat1`. Before the indexes are made: dropping an index
/// takes its row away, and making one is what tells an open reader to read
/// the rows again. SQLite ignores a row for an index not there yet.
pub(super) async fn write(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    // Makes the table, and gathers nothing.
    load(conn).await?;
    sqlx::query("DELETE FROM sqlite_stat1")
        .execute(&mut *conn)
        .await
        .context("clear the planner's ranks")?;
    for sql in statements() {
        sqlx::query(sql.as_str())
            .execute(&mut *conn)
            .await
            .with_context(|| format!("tell the planner: {sql}"))?;
    }
    Ok(())
}

/// Have this connection plan by what `sqlite_stat1` says.
pub(super) async fn load(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    sqlx::query("ANALYZE sqlite_schema")
        .execute(&mut *conn)
        .await
        .context("load the planner's ranks")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sqlx::Row;

    use super::*;

    #[tokio::test]
    async fn every_index_of_the_grids_rows_has_a_rank_and_no_other() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::grid_index::open_index(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        let made: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'grid_rows'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let ranked: BTreeSet<&str> = RANKS.iter().map(|(index, _)| *index).collect();
        assert_eq!(ranked.len(), RANKS.len(), "an index ranked twice");
        assert_eq!(
            made.iter().map(String::as_str).collect::<BTreeSet<_>>(),
            ranked
        );
        let told: Vec<String> = sqlx::query_scalar("SELECT idx FROM sqlite_stat1")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            told.iter().map(String::as_str).collect::<BTreeSet<_>>(),
            ranked
        );
        crate::grid_index::close_index(pool).await.unwrap();
    }

    /// The search applet keeps its connections open across a pass that
    /// drops the indexes and makes them again.
    #[tokio::test]
    async fn a_reader_open_across_a_remake_plans_by_the_ranks() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let writer = crate::grid_index::open_index(&db).await.unwrap();
        let reader = datalib_etl::pin::open_reader(&db).await.unwrap();
        // Which, with no ranks, starts at the source.
        let plan = || async {
            // An EXPLAIN reads no page, so it would not see the schema move.
            sqlx::query("SELECT count(*) FROM grid_rows")
                .fetch_one(&reader)
                .await
                .unwrap();
            let rows = sqlx::query(
                "EXPLAIN QUERY PLAN SELECT author, count(*) FROM grid_rows \
                 WHERE source_id = 'a' AND channel = 'b' GROUP BY 1",
            )
            .persistent(false)
            .fetch_all(&reader)
            .await
            .unwrap();
            rows[0].get::<String, _>("detail")
        };
        let by_channel = "SEARCH grid_rows USING INDEX grid_rows_by_channel (channel=?)";
        assert_eq!(plan().await, by_channel);

        let mut conn = writer.acquire().await.unwrap();
        crate::grid_index::drop_search_indexes(&mut conn)
            .await
            .unwrap();
        assert_ne!(plan().await, by_channel);
        crate::grid_index::create_search_indexes(&mut conn)
            .await
            .unwrap();
        assert_eq!(plan().await, by_channel);
        drop(conn);
        reader.close().await;
        crate::grid_index::close_index(writer).await.unwrap();
    }
}
