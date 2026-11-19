//! What the grid index's search terms say about the library's owner: every
//! handle a source says is its account's own, which `me` reaches. The
//! terms and what they hold: `datalib_etl_render::search_terms`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};

/// Every handle a source says is its account's own, each once. `None`
/// when the root has no grid index yet, or one written before it held
/// them.
pub async fn account_handles(root: &Path) -> Result<Option<Vec<String>>> {
    let path = datalib_runtime::layout::grid_index_db(root);
    if !path.exists() {
        return Ok(None);
    }
    let mut conn = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(&path))
        .read_only(true)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(5))
        .connect()
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let read = async {
        let held: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM sqlite_master \
             WHERE type = 'table' AND name = 'account_handles'",
        )
        .fetch_one(&mut conn)
        .await?;
        if !held {
            return anyhow::Ok(None);
        }
        let handles: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT handle FROM account_handles ORDER BY handle")
                .fetch_all(&mut conn)
                .await?;
        anyhow::Ok(Some(handles))
    }
    .await
    .context("read the account handles");
    conn.close().await.ok();
    read
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid index at `root` holding the search terms' tables, empty.
    async fn terms_file(root: &Path) {
        let path = datalib_runtime::layout::grid_index_db(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut conn = SqliteConnectOptions::new()
            .filename(datalib_runtime::plain_sqlite::uri(&path))
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        for ddl in datalib_schema::search_terms::TERMS_DDL {
            sqlx::query(sqlx::AssertSqlSafe(*ddl))
                .execute(&mut conn)
                .await
                .unwrap();
        }
        conn.close().await.unwrap();
    }

    /// An index from a build before `account_handles` has none to give,
    /// so `me` is refused rather than failing on a missing table.
    #[tokio::test]
    async fn the_accounts_handles_are_read_only_from_an_index_that_holds_them() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(account_handles(tmp.path()).await.unwrap(), None);

        terms_file(tmp.path()).await;
        let path = datalib_runtime::layout::grid_index_db(tmp.path());
        let mut conn = SqliteConnectOptions::new()
            .filename(datalib_runtime::plain_sqlite::uri(&path))
            .connect()
            .await
            .unwrap();
        for (handle, source) in [("slack:T/U1", "slack"), ("slack:T/U1", "beeper")] {
            sqlx::query("INSERT INTO account_handles (handle, source_id) VALUES (?, ?)")
                .bind(handle)
                .bind(source)
                .execute(&mut conn)
                .await
                .unwrap();
        }
        assert_eq!(
            account_handles(tmp.path()).await.unwrap(),
            Some(vec!["slack:T/U1".to_string()])
        );

        sqlx::query("DROP TABLE account_handles")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        assert_eq!(account_handles(tmp.path()).await.unwrap(), None);
    }
}
