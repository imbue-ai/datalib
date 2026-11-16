//! The `problems` of a derived store whose step finds all of them again
//! on every run (`render_time_series`, `render_occurrences`): the run's
//! rows replace the store's whole, in the store's transaction, and the
//! store's earlier rows only lend their stamps. How the rows reach the
//! index: `README.md` §"Problems flow downstream with the data".

use std::collections::HashMap;

use anyhow::{Context, Result};
use datalib_problems::ProblemRow;
use datalib_table::BulkUpsertable as _;
use sqlx::SqlitePool;

/// The `problems` table's DDL, for the store's open.
pub fn ddl() -> &'static str {
    datalib_problems::DDL[0].1
}

/// Make the store's `problems` hold `rows` and nothing else, each
/// stamped against the row it held under the same id
/// (`ProblemRow::stamped`), so a problem found again unchanged changes
/// nothing. Rows that share an id are one row, their samples joined.
/// Uncommitted: the caller seals.
pub async fn replace(
    pool: &SqlitePool,
    rows: Vec<ProblemRow>,
    now: &datalib_time::StoredStamp,
) -> Result<()> {
    let mut tx = pool.begin().await.context("begin the problems write")?;
    let mut earlier: HashMap<String, ProblemRow> = HashMap::new();
    for r in sqlx::query("SELECT * FROM problems")
        .fetch_all(&mut *tx)
        .await
        .context("read the problems held")?
    {
        let row = ProblemRow::from_row(&r)?;
        earlier.insert(row.problem_uuid.clone(), row);
    }
    sqlx::query("DELETE FROM problems")
        .execute(&mut *tx)
        .await
        .context("clear the problems held")?;
    let mut news = Vec::new();
    for row in one_per_id(rows) {
        let held = earlier.get(&row.problem_uuid);
        let row = row.stamped(held, &now.utc, now.tz_offset.as_deref());
        // Audited: `insert_sql` is built from `ProblemRow`'s associated
        // consts, never from row data; all values bound.
        row.bind_into(sqlx::query(sqlx::AssertSqlSafe(crate::bulk::insert_sql::<
            ProblemRow,
        >())))
        .execute(&mut *tx)
        .await
        .with_context(|| format!("record problem {}", row.problem_uuid))?;
        if row.is_news(held) {
            news.push(row);
        }
    }
    tx.commit().await.context("commit the problems write")?;
    datalib_problems::note_recorded(&news);
    Ok(())
}

/// One row per id, in first-seen order, a later row's sample appended
/// to the first's: two reasons for one record are one problem.
fn one_per_id(rows: Vec<ProblemRow>) -> Vec<ProblemRow> {
    let mut out: Vec<ProblemRow> = Vec::with_capacity(rows.len());
    let mut at: HashMap<String, usize> = HashMap::new();
    for row in rows {
        match at.get(&row.problem_uuid) {
            Some(&i) => {
                let first = &mut out[i];
                if !first.sample.split("; ").any(|s| s == row.sample) {
                    first.sample = format!("{}; {}", first.sample, row.sample);
                }
            }
            None => {
                at.insert(row.problem_uuid.clone(), out.len());
                out.push(row);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_problems::{Outcome, Problem, Reason, Scope, Stage};

    fn row(record: &str, why: &str) -> ProblemRow {
        ProblemRow::new(
            "riker",
            Stage::TimeSeries,
            Scope::Entity(record),
            None,
            Outcome::Dropped,
            Problem::record(Reason::RenderFailed, why),
            None,
        )
    }

    async fn store(dir: &std::path::Path) -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(datalib_sqlite_open::file(&dir.join("s.sqlite")).create_if_missing(true))
            .await
            .unwrap();
        sqlx::query(ddl()).execute(&pool).await.unwrap();
        pool
    }

    async fn held(pool: &SqlitePool) -> Vec<ProblemRow> {
        sqlx::query("SELECT * FROM problems ORDER BY scope_key")
            .fetch_all(pool)
            .await
            .unwrap()
            .iter()
            .map(|r| ProblemRow::from_row(r).unwrap())
            .collect()
    }

    /// A run's rows are the store's whole; one found again unchanged
    /// keeps its stamps, one fixed since is gone, and two reasons for one
    /// record are one row saying both.
    #[tokio::test]
    async fn a_run_replaces_the_rows_and_keeps_the_stamps_of_the_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let pool = store(dir.path()).await;
        let day1 = datalib_time::split_stamp("2370-01-01T00:00:00+02:00");
        replace(&pool, vec![row("a", "torn"), row("b", "empty")], &day1)
            .await
            .unwrap();
        let day2 = datalib_time::split_stamp("2370-01-02T00:00:00+02:00");
        replace(
            &pool,
            vec![row("a", "torn"), row("c", "x"), row("c", "y")],
            &day2,
        )
        .await
        .unwrap();
        let rows = held(&pool).await;
        let keys: Vec<&str> = rows.iter().map(|r| r.scope_key.as_str()).collect();
        assert_eq!(keys, ["a", "c"]);
        assert_eq!(
            rows[0].changed_at_utc, day1.utc,
            "unchanged keeps its stamp"
        );
        assert_eq!(rows[1].changed_at_utc, day2.utc);
        assert_eq!(rows[1].sample, "x; y");
        pool.close().await;
    }
}
