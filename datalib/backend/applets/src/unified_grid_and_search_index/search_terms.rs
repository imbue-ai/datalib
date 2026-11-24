//! What the grid index's search terms say about the library's owner: every
//! handle a source says is its account's own, which `me` reaches. The
//! terms and what they hold: `datalib_etl_render::search_terms`.

use std::path::Path;

use anyhow::{Context, Result};

/// Every handle a source says is its account's own, each once. `None`
/// when the root has no grid index yet.
pub async fn account_handles(root: &Path) -> Result<Option<Vec<String>>> {
    let path = datalib_runtime::layout::grid_index_db(root);
    let Some(pool) = datalib_etl_render::grid_index::open_for_reading(&path).await? else {
        return Ok(None);
    };
    let handles = sqlx::query_scalar("SELECT DISTINCT handle FROM account_handles ORDER BY handle")
        .fetch_all(&pool)
        .await
        .context("read the account handles");
    pool.close().await;
    Ok(Some(handles?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handles are read from an index in this build's shape; one an
    /// older build left is refused, saying so, rather than read as holding
    /// none.
    #[tokio::test]
    async fn the_accounts_handles_are_read_only_from_an_index_in_this_shape() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(account_handles(tmp.path()).await.unwrap(), None);

        let path = datalib_runtime::layout::grid_index_db(tmp.path());
        let pool = datalib_etl_render::grid_index::open_index(&path)
            .await
            .unwrap();
        for (handle, source) in [("slack:T/U1", "slack"), ("slack:T/U1", "beeper")] {
            sqlx::query("INSERT INTO account_handles (handle, source_id) VALUES (?, ?)")
                .bind(handle)
                .bind(source)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(
            account_handles(tmp.path()).await.unwrap(),
            Some(vec!["slack:T/U1".to_string()])
        );

        sqlx::query("UPDATE _datalib_meta SET value = 'older' WHERE key = 'schema_hash'")
            .execute(&pool)
            .await
            .unwrap();
        datalib_etl_render::grid_index::close_index(pool)
            .await
            .unwrap();
        let refused = account_handles(tmp.path()).await.unwrap_err();
        assert!(
            datalib_store_meta::other_shape(&refused).is_some(),
            "{refused:#}"
        );
    }
}
