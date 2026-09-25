//! Doltlite-backed raw store for the `garmin` provider.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use sqlx::Row;

use datalib_etl::doltlite_raw::{self as dr};

use super::schema_raw::full_ddl;

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl());

impl RawDb {
    /// `(id → payload text)` for every id listed, from one table. Ids
    /// absent from the table are absent from the map.
    pub async fn payloads_of(&self, table: &str, ids: &[&str]) -> Result<HashMap<String, String>> {
        let mut out = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            // Audited: `table` is a literal at every callsite; `placeholders`
            // is a `?,?,?` run sized from the chunk and each id is bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT id, json(payload) AS payload FROM {table} \
                 WHERE id IN ({placeholders}) AND payload IS NOT NULL"
            )));
            for id in chunk {
                q = q.bind(*id);
            }
            for r in q
                .fetch_all(self.pool())
                .await
                .with_context(|| format!("payloads_of {table}"))?
            {
                let id: String = r.try_get("id").unwrap_or_default();
                let payload: String = r.try_get("payload").unwrap_or_default();
                out.insert(id, payload);
            }
        }
        Ok(out)
    }

    /// Ids in `table` whose id is not in `keep`, optionally only those
    /// matching `filter_sql` (a `&'static str` predicate over the
    /// table's own columns, with its bound values in `binds`).
    async fn ids_not_in(
        &self,
        table: &'static str,
        filter_sql: &'static str,
        binds: &[&str],
        keep: &HashSet<&str>,
    ) -> Result<Vec<String>> {
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id FROM {table} WHERE {filter_sql}"
        )));
        for b in binds {
            q = q.bind(*b);
        }
        let rows = q
            .fetch_all(self.pool())
            .await
            .with_context(|| format!("scan {table} for pruning"))?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.try_get::<String, _>("id").ok())
            .filter(|id| !keep.contains(id.as_str()))
            .collect())
    }

    async fn delete_ids(&self, table: &'static str, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await?;
        for chunk in ids.chunks(500) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            for stmt in [
                format!("DELETE FROM {table} WHERE id IN ({placeholders})"),
                format!("DELETE FROM {table}_bookkeeping WHERE id IN ({placeholders})"),
            ] {
                // Audited: `table` is `&'static str`; every id is bound.
                let mut q = sqlx::query(sqlx::AssertSqlSafe(stmt));
                for id in chunk {
                    q = q.bind(id);
                }
                q.execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Delete the rows of one `garmin_items` kind that the latest
    /// complete listing did not name.
    pub async fn prune_items(&self, kind: &str, keep: &HashSet<&str>) -> Result<usize> {
        let gone = self
            .ids_not_in("garmin_items", "kind = ?", &[kind], keep)
            .await?;
        self.delete_ids("garmin_items", &gone).await?;
        Ok(gone.len())
    }

    /// Delete the devices the registration listing no longer names.
    pub async fn prune_devices(&self, keep: &HashSet<&str>) -> Result<usize> {
        let gone = self
            .ids_not_in("garmin_devices", "1 = 1", &[], keep)
            .await?;
        self.delete_ids("garmin_devices", &gone).await?;
        Ok(gone.len())
    }

    /// Delete weigh-ins dated inside `[start, end]` that the range
    /// listing for that window did not name. Scoped to the window
    /// because only that window was listed completely.
    pub async fn prune_weigh_ins(
        &self,
        start: &str,
        end: &str,
        keep: &HashSet<&str>,
    ) -> Result<usize> {
        let gone = self
            .ids_not_in(
                "garmin_weigh_ins",
                "calendar_date >= ? AND calendar_date <= ?",
                &[start, end],
                keep,
            )
            .await?;
        self.delete_ids("garmin_weigh_ins", &gone).await?;
        Ok(gone.len())
    }

    /// Delete activities that started at or after `start_gmt` which the
    /// listing walked from that instant did not name, with their
    /// details and file edges.
    pub async fn prune_activities(&self, start_gmt: &str, keep: &HashSet<&str>) -> Result<usize> {
        let gone = self
            .ids_not_in(
                "garmin_activities",
                "start_time_gmt >= ?",
                &[start_gmt],
                keep,
            )
            .await?;
        self.delete_ids("garmin_activities", &gone).await?;
        self.delete_ids("garmin_activity_details", &gone).await?;
        let edges: Vec<String> = gone
            .iter()
            .map(|id| format!("{id}#{}", super::schema_raw::FILE_KIND_FIT))
            .collect();
        self.delete_ids("garmin_activity_files", &edges).await?;
        Ok(gone.len())
    }

    /// `activity_id → blake3` for every activity whose FIT file is in
    /// the CAS. An edge with a NULL hash (a failed fetch) is absent, so
    /// it is retried.
    pub async fn stored_activity_files(&self) -> Result<HashMap<String, String>> {
        let rows = sqlx::query(
            "SELECT activity_id, blake3 FROM garmin_activity_files WHERE blake3 IS NOT NULL",
        )
        .fetch_all(self.pool())
        .await
        .context("select garmin_activity_files")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some((
                    r.try_get::<String, _>("activity_id").ok()?,
                    r.try_get::<String, _>("blake3").ok()?,
                ))
            })
            .collect())
    }

    pub async fn stored_wellness_files(&self) -> Result<HashMap<String, String>> {
        let rows = sqlx::query(
            "SELECT calendar_date, blake3 FROM garmin_wellness_files WHERE blake3 IS NOT NULL",
        )
        .fetch_all(self.pool())
        .await
        .context("select garmin_wellness_files")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some((
                    r.try_get::<String, _>("calendar_date").ok()?,
                    r.try_get::<String, _>("blake3").ok()?,
                ))
            })
            .collect())
    }

    /// Every stored activity with no detail payload: its fetch failed,
    /// was interrupted, or never happened. A detail Garmin answered with
    /// nothing is stored as `null` and is not here.
    pub async fn activities_without_detail(&self) -> Result<Vec<String>> {
        sqlx::query_scalar(
            "SELECT a.id FROM garmin_activities a \
             LEFT JOIN garmin_activity_details d ON d.id = a.id \
             WHERE d.payload IS NULL ORDER BY a.id",
        )
        .fetch_all(self.pool())
        .await
        .context("select garmin_activities without a detail")
    }

    /// Every `garmin_daily` id whose last attempt failed. Read from the
    /// sidecar: a day that never fetched has no data row, since the stub
    /// insert cannot fill `metric` and `calendar_date`.
    pub async fn failed_daily_ids(&self) -> Result<Vec<String>> {
        sqlx::query_scalar(
            "SELECT id FROM garmin_daily_bookkeeping WHERE last_error IS NOT NULL ORDER BY id",
        )
        .fetch_all(self.pool())
        .await
        .context("select failed garmin_daily ids")
    }

    pub async fn cursor(&self, scope: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT last_seen_at_utc FROM sync_scope_state WHERE scope = ?")
            .bind(scope)
            .fetch_optional(self.pool())
            .await
            .with_context(|| format!("select cursor {scope}"))?;
        Ok(row.and_then(|r| r.try_get::<String, _>("last_seen_at_utc").ok()))
    }

    pub async fn set_cursor(&self, scope: &str, value: &str) -> Result<()> {
        dr::upsert_scope_state(self.pool(), scope, value).await
    }
}
