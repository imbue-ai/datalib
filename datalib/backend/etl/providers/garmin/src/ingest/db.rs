//! Doltlite-backed raw store for the `garmin` provider.

use std::collections::HashSet;

use anyhow::{Context, Result};
use sqlx::Row;

use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw::{self as dr};
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::owed::{self, Listed};

use super::schema_raw::{full_ddl, ActivityRow, FILE_KIND_FIT, LADDER};

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

/// The `sync_scope_state` key, less the table, saying that table's file
/// edges have been through [`RawDb::repair_shared_file_hashes`].
pub const REPAIRED_PREFIX: &str = "garmin:shared_hash_repair:";

/// The `coverage` scope of the activity listing: spans of start dates.
pub const ACTIVITIES_SCOPE: &str = "activities";

/// A listing of activities that reached its end: the start dates it
/// covered, and the first start time it is evidence of absence for.
pub struct Enumerated {
    pub covered: Span,
    pub prune_from: String,
}

impl RawDb {
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
        let ids = rows
            .iter()
            .map(|r| r.try_get::<String, _>("id"))
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("{table} id"))?;
        Ok(ids
            .into_iter()
            .filter(|id| !keep.contains(id.as_str()))
            .collect())
    }

    async fn delete_ids(&self, table: &'static str, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await?;
        delete_ids_in(&mut tx, table, ids).await?;
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

    /// Store one listing of activities. When it was an enumeration, the
    /// same transaction deletes what it did not name from `prune_from`
    /// on, with details and file edges, and records the start dates it
    /// covered: a span is never claimed without the rows found in it.
    /// Returns how many activities were pruned.
    pub async fn store_activity_listing(
        &self,
        rows: &[ActivityRow],
        enumerated: Option<&Enumerated>,
    ) -> Result<usize> {
        let gone = match enumerated {
            Some(e) => {
                let keep: HashSet<&str> =
                    rows.iter().map(|r| r.id_and_payload.id.as_str()).collect();
                self.ids_not_in(
                    "garmin_activities",
                    "start_time_gmt >= ?",
                    &[&e.prune_from],
                    &keep,
                )
                .await?
            }
            None => Vec::new(),
        };
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = self.pool().begin().await?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        if let Some(e) = enumerated {
            let edges: Vec<String> = gone
                .iter()
                .map(|id| format!("{id}#{FILE_KIND_FIT}"))
                .collect();
            delete_ids_in(&mut tx, "garmin_activities", &gone).await?;
            delete_ids_in(&mut tx, "garmin_activity_details", &gone).await?;
            delete_ids_in(&mut tx, "garmin_activity_files", &edges).await?;
            coverage::cover(&mut tx, ACTIVITIES_SCOPE, e.covered.clone()).await?;
        }
        tx.commit().await?;
        Ok(gone.len())
    }

    /// Every stored activity at its listing version, newest first: what
    /// a detail is listed for. A row no listing by this build has named
    /// has no version and is listed for nothing until one does.
    pub async fn listed_activities(&self) -> Result<Vec<Listed>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, listing_hash FROM garmin_activities \
             WHERE listing_hash IS NOT NULL ORDER BY start_time_gmt DESC, id",
        )
        .fetch_all(self.pool())
        .await
        .context("select the garmin activities as listed")?;
        Ok(rows
            .into_iter()
            .map(|(id, hash)| Listed::new(id, Some(hash)))
            .collect())
    }

    /// The FIT file edges of the stored activities that hold no bytes,
    /// each at its activity's listing version, newest first: what a file
    /// is listed for. An edge with bytes is never listed again, since the
    /// original upload does not change when the activity is edited.
    pub async fn activity_files_without_bytes(&self) -> Result<Vec<Listed>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT a.id || '#fit', a.listing_hash \
             FROM garmin_activities a \
             LEFT JOIN garmin_activity_files f ON f.id = a.id || '#fit' \
             WHERE a.listing_hash IS NOT NULL AND f.blake3 IS NULL \
             ORDER BY a.start_time_gmt DESC, a.id",
        )
        .fetch_all(self.pool())
        .await
        .context("select the garmin activities without a file")?;
        Ok(rows
            .into_iter()
            .map(|(id, hash)| Listed::new(id, Some(hash)))
            .collect())
    }

    /// Mend the file edges an earlier build wrote: it keyed every file of
    /// a batch under one ref, so each edge of the batch got the last
    /// file's hash, and a hash several records' edges share is the mark it
    /// left. Those edges lose the hash and hold nothing, which is what
    /// makes the walks fetch them again.
    ///
    /// Once per table, recorded under [`REPAIRED_PREFIX`], and only while
    /// the walk that would refetch it is on: two activities may share a
    /// file for real (a multisport leg and its parent, perhaps), and a
    /// repair run every time would refetch those on every run. A fresh
    /// store is marked repaired on its first run.
    pub async fn repair_shared_file_hashes(
        &self,
        activity_files: bool,
        wellness_files: bool,
    ) -> Result<()> {
        for (on, table, owner, what) in [
            (
                activity_files,
                "garmin_activity_files",
                "activity_id",
                "activity",
            ),
            (
                wellness_files,
                "garmin_wellness_files",
                "calendar_date",
                "day",
            ),
        ] {
            let marker = format!("{REPAIRED_PREFIX}{table}");
            if !on || self.marker(&marker).await?.is_some() {
                continue;
            }
            // Audited: `table` and `owner` are literals from the list above.
            let ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT id FROM {table} WHERE blake3 IN \
                 (SELECT blake3 FROM {table} WHERE blake3 IS NOT NULL \
                  GROUP BY blake3 HAVING COUNT(DISTINCT {owner}) > 1) ORDER BY id"
            )))
            .fetch_all(self.pool())
            .await
            .with_context(|| format!("find shared hashes in {table}"))?;
            let err = format!(
                "the stored hash was another {what}'s file, written by an earlier build; \
                 fetching it again"
            );
            let mut tx = self.pool().begin().await?;
            for id in &ids {
                // Audited: `table` is a literal from the list above; `id` is bound.
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE {table} SET blake3 = NULL WHERE id = ?"
                )))
                .bind(id)
                .execute(&mut *tx)
                .await?;
                owed::forget(&mut tx, table, id).await?;
                dr::record_object_error(&mut tx, table, id, &err).await?;
            }
            tx.commit().await?;
            self.set_marker(&marker, "done").await?;
        }
        Ok(())
    }

    /// Drop the fetch problems of days outside `[since, end]`: the window
    /// no longer asks for them, so no run will fetch them and clear the
    /// row. Returns how many went.
    pub async fn forget_daily_problems_outside(&self, since: &str, end: &str) -> Result<u64> {
        let done = sqlx::query(
            "DELETE FROM problems WHERE scope_kind = ? AND stage = ? \
             AND scope_key LIKE 'garmin_daily:%' \
             AND substr(scope_key, instr(scope_key, '#') + 1) NOT BETWEEN ? AND ?",
        )
        .bind(datalib_problems::ScopeKind::Entity.as_str())
        .bind(datalib_problems::Stage::Fetch.as_str())
        .bind(since)
        .bind(end)
        .execute(self.pool())
        .await
        .context("forget failed garmin_daily days outside the window")?;
        Ok(done.rows_affected())
    }

    pub async fn marker(&self, scope: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT last_seen_at_utc FROM sync_scope_state WHERE scope = ?")
            .bind(scope)
            .fetch_optional(self.pool())
            .await
            .with_context(|| format!("select marker {scope}"))?;
        row.map(|r| r.try_get::<String, _>("last_seen_at_utc"))
            .transpose()
            .context("sync_scope_state last_seen_at_utc")
    }

    pub async fn set_marker(&self, scope: &str, value: &str) -> Result<()> {
        dr::upsert_scope_state(self.pool(), scope, value).await
    }
}

async fn delete_ids_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    ids: &[String],
) -> Result<()> {
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
            q.execute(&mut **tx).await?;
        }
        // A row that is gone cannot fail to fetch any more.
        // Audited: `placeholders` is a `?,?,?` run sized from the
        // chunk; every key is bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM problems WHERE scope_kind = ? AND scope_key IN ({placeholders})"
        )))
        .bind(datalib_problems::ScopeKind::Entity.as_str());
        for id in chunk {
            q = q.bind(format!("{table}:{id}"));
        }
        q.execute(&mut **tx).await?;
    }
    Ok(())
}
