//! `AppStore` — the three stores this server owns: filed feedback and
//! remote media (the allow-list and the download CAS's index), one
//! doltlite file each, and the disk timeseries, one plain SQLite file.

use crate::app_store_migrate::{
    carry_legacy_usage, DISK_STATS_LADDER, FEEDBACK_LADDER, REMOTE_MEDIA_LADDER,
};
use crate::repo::{AppRepo, RepoError};
use crate::store::{open_plain_pool, open_pool};
use app_schema::disk_free::DiskFreeRow;
use app_schema::disk_usage::DiskUsageRow;
use app_schema::feedback::{FeedbackRow, DDL as FEEDBACK_DDL};
use app_schema::remote_media::allow::RemoteMediaAllowRow;
use app_schema::remote_media::media::RemoteMediaRow;
use app_schema::remote_media::{AllowScope, DDL as REMOTE_MEDIA_DDL};
use app_schema::DISK_STATS_DDL;
use async_trait::async_trait;
use datalib_store_meta::StoreKind;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// The three application stores: filed feedback, the disk timeseries,
/// and remote media.
pub struct AppStore {
    /// Filed feedback. Outside the cache-tagged index tree, because
    /// nothing regenerates it.
    feedback_pool: SqlitePool,
    /// The disk timeseries (`disk_usage`, `disk_free`): plain SQLite,
    /// written every few seconds while the server is up. The rows are the
    /// history; there is nothing to commit.
    disk_stats_pool: SqlitePool,
    /// What remote media a person let a document load, and the URLs
    /// fetched into the download CAS. Committed per write, like
    /// feedback, so the decisions have a history.
    remote_media_pool: SqlitePool,
    /// Whether the linked libsqlite3 is doltlite (exposes `dolt_commit`).
    /// Probed once at connect time via `pragma_function_list`. When
    /// false, every `commit_version` call is a no-op — the row still
    /// lands, you just don't get the dolt_log audit entry. This keeps
    /// CI hosts without doltlite installed runnable; production hosts
    /// should always have doltlite linked.
    has_dolt: bool,
}

impl AppStore {
    /// Open (or create) the stores for a data root and ensure their
    /// tables exist. DDL is `CREATE TABLE IF NOT EXISTS`, so a populated
    /// file is untouched — which is why each store's ladder runs first
    /// (`app_store_migrate`).
    pub async fn open(root: &std::path::Path) -> Result<Self, sqlx::Error> {
        let feedback_pool = open_pool(&crate::layout::feedback_db(root)).await?;
        let disk_stats_pool = open_plain_pool(&crate::layout::disk_stats_db(root)).await?;
        let remote_media_pool = open_pool(&crate::layout::remote_media_db(root)).await?;
        let has_dolt = probe_dolt_extensions(&feedback_pool).await;
        let internal = |e: anyhow::Error| sqlx::Error::Protocol(format!("{e:#}"));
        // Before the ladder and the DDL: a store a newer line of datalib
        // wrote is refused whole (`datalib_store_meta::guard`). Then the
        // rungs above the store's version. Feedback is committed per row
        // with `-Am`, so each rung is sealed as its own commit; disk stats
        // is plain SQLite, with nothing to commit.
        for (pool, path, ladder, commits) in [
            (
                &feedback_pool,
                crate::layout::feedback_db(root),
                FEEDBACK_LADDER,
                has_dolt,
            ),
            (
                &disk_stats_pool,
                crate::layout::disk_stats_db(root),
                DISK_STATS_LADDER,
                false,
            ),
            (
                &remote_media_pool,
                crate::layout::remote_media_db(root),
                REMOTE_MEDIA_LADDER,
                has_dolt,
            ),
        ] {
            let written_by = datalib_store_meta::read(pool).await.map_err(internal)?;
            if let Err(newer) = datalib_store_meta::refuse_if_newer(&path, written_by.as_ref()) {
                for p in [&feedback_pool, &disk_stats_pool, &remote_media_pool] {
                    p.close().await;
                }
                return Err(sqlx::Error::Configuration(Box::new(newer)));
            }
            let stored = written_by.map_or(0, |m| m.schema_version);
            let top = datalib_store_meta::ladder::top(ladder);
            if stored > top {
                for p in [&feedback_pool, &disk_stats_pool, &remote_media_pool] {
                    p.close().await;
                }
                return Err(sqlx::Error::Configuration(Box::new(
                    datalib_store_meta::ladder::AheadOfLadder { stored, top },
                )));
            }
            for rung in datalib_store_meta::ladder::pending(ladder, stored).map_err(internal)? {
                sqlx::query(datalib_store_meta::DDL).execute(pool).await?;
                datalib_store_meta::ladder::apply(pool, rung, datalib_store_meta::Ladder::Own)
                    .await
                    .map_err(internal)?;
                if commits {
                    sqlx::query("SELECT dolt_commit('-Am', ?)")
                        .bind(format!("migrate v{}: {}", rung.version, rung.name))
                        .execute(pool)
                        .await?;
                }
            }
        }
        let store = Self {
            feedback_pool,
            disk_stats_pool,
            remote_media_pool,
            has_dolt,
        };
        store.init_feedback_table().await?;
        store.init_disk_stats_tables().await?;
        store.init_remote_media_tables().await?;
        // A root from before `disk_stats` keeps its timeseries in the
        // doltlite `usage` store. Losing it costs a sparkline's history,
        // not the app, so a copy that fails is logged and tried again on
        // the next open rather than refusing to start.
        match carry_legacy_usage(root, &store.disk_stats_pool).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "disk stats: carried over the old usage store"),
            Err(e) => tracing::error!(
                "disk stats: could not carry over the old usage store, left in place for \
                 the next open: {e}"
            ),
        }
        // Which build wrote each store, beside its tables. Feedback and
        // remote media are committed per row with `-Am`, so a changed
        // meta row is sealed here rather than left to ride into the next
        // commit; disk stats is plain SQLite and its rows just land.
        for (pool, kind, ddl, ladder) in [
            (
                &store.feedback_pool,
                StoreKind::Feedback,
                FEEDBACK_DDL,
                FEEDBACK_LADDER,
            ),
            (
                &store.disk_stats_pool,
                StoreKind::DiskStats,
                DISK_STATS_DDL,
                DISK_STATS_LADDER,
            ),
            (
                &store.remote_media_pool,
                StoreKind::RemoteMedia,
                REMOTE_MEDIA_DDL,
                REMOTE_MEDIA_LADDER,
            ),
        ] {
            let hash = datalib_store_meta::schema_hash(ddl.iter().map(|(_t, d)| *d));
            let top = datalib_store_meta::ladder::top(ladder);
            let versions = datalib_store_meta::Versions {
                schema: top,
                shared: 0,
            };
            let changed = datalib_store_meta::write(pool, kind, &hash, versions)
                .await
                .map_err(internal)?;
            let committed = matches!(kind, StoreKind::Feedback | StoreKind::RemoteMedia);
            if changed && has_dolt && committed {
                sqlx::query("SELECT dolt_commit('-Am', ?)")
                    .bind(format!(
                        "meta: written by datalib {}",
                        datalib_runtime::build_id::DATALIB_VERSION
                    ))
                    .execute(pool)
                    .await?;
            }
        }
        Ok(store)
    }

    pub fn has_dolt_extensions(&self) -> bool {
        self.has_dolt
    }

    async fn commit_version(
        &self,
        conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
        message: &str,
    ) -> Result<(), RepoError> {
        if !self.has_dolt {
            return Ok(());
        }
        sqlx::query("SELECT dolt_commit('-Am', ?)")
            .bind(message)
            .execute(&mut **conn)
            .await
            .map_err(|e| RepoError::Internal(format!("dolt_commit: {e}")))?;
        Ok(())
    }
    async fn init_feedback_table(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in FEEDBACK_DDL {
            sqlx::query(*ddl).execute(&self.feedback_pool).await?;
        }
        Ok(())
    }
    async fn init_disk_stats_tables(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in DISK_STATS_DDL {
            sqlx::query(*ddl).execute(&self.disk_stats_pool).await?;
        }
        Ok(())
    }
    async fn init_remote_media_tables(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in REMOTE_MEDIA_DDL {
            sqlx::query(*ddl).execute(&self.remote_media_pool).await?;
        }
        Ok(())
    }
    pub fn feedback_pool(&self) -> &SqlitePool {
        &self.feedback_pool
    }
}

#[async_trait]
impl AppRepo for AppStore {
    async fn insert_feedback(&self, row: FeedbackRow) -> Result<(), RepoError> {
        // The INSERT and the `dolt_commit` ride one acquired connection,
        // the pool's only one, so no sibling task's INSERT can land
        // between them: doltlite's working set belongs to the branch,
        // not the connection, and `-Am` would sweep it into this commit
        // (docs/dev/doltlite.md#branches-head-and-the-working-set).
        let mut conn = self
            .feedback_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        sqlx::query(
            "INSERT INTO feedback \
             (feedback_uuid, created_at_utc, tz_offset, sentiment, comment, app_version, \
              git_hash, context_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.feedback_uuid)
        .bind(&row.created_at_utc)
        .bind(&row.tz_offset)
        .bind(&row.sentiment)
        .bind(&row.comment)
        .bind(&row.app_version)
        .bind(&row.git_hash)
        .bind(&row.context_json)
        .execute(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("insert: {e}")))?;
        let msg = format!("feedback: {}", row.feedback_uuid);
        self.commit_version(&mut conn, &msg).await?;
        Ok(())
    }
    async fn record_disk_usage(&self, rows: &[DiskUsageRow]) -> Result<(), RepoError> {
        if rows.is_empty() {
            return Ok(());
        }
        // One transaction per walk: an autocommit per row costs a sync each.
        let mut tx = self
            .disk_stats_pool
            .begin()
            .await
            .map_err(|e| RepoError::Internal(format!("begin: {e}")))?;
        for row in rows {
            // INSERT OR REPLACE, not plain INSERT: the key is
            // (path, measured_at_utc) and the sampler stamps one instant per
            // walk, so a second walk finishing inside the same
            // whole-microsecond would otherwise fail the whole batch
            // over a duplicate that carries the same number anyway.
            sqlx::query(
                "INSERT OR REPLACE INTO disk_usage (path, measured_at_utc, tz_offset, bytes) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&row.path)
            .bind(&row.measured_at_utc)
            .bind(&row.tz_offset)
            .bind(row.bytes)
            .execute(&mut *tx)
            .await
            .map_err(|e| RepoError::Internal(format!("insert disk_usage: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| RepoError::Internal(format!("commit disk_usage: {e}")))
    }

    async fn forget_disk_stats_before(&self, before_utc: &str) -> Result<u64, RepoError> {
        let internal = |e: sqlx::Error| RepoError::Internal(format!("forget disk stats: {e}"));
        let mut tx = self.disk_stats_pool.begin().await.map_err(internal)?;
        // A row before the cutoff goes only when a later row of its series
        // is also at or before it. That later one is the series' value from
        // the cutoff on: the carry-in any range after it opens with
        // (`disk_usage_between`), for a tree that has not moved since.
        let usage = sqlx::query(
            "DELETE FROM disk_usage WHERE measured_at_utc < ? AND EXISTS ( \
               SELECT 1 FROM disk_usage AS later WHERE later.path = disk_usage.path \
               AND later.measured_at_utc > disk_usage.measured_at_utc \
               AND later.measured_at_utc <= ?)",
        )
        .bind(before_utc)
        .bind(before_utc)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        let free = sqlx::query(
            "DELETE FROM disk_free WHERE measured_at_utc < ( \
               SELECT MAX(measured_at_utc) FROM disk_free WHERE measured_at_utc <= ?)",
        )
        .bind(before_utc)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(usage.rows_affected() + free.rows_affected())
    }

    async fn recent_disk_usage(&self, limit: usize) -> Result<Vec<DiskUsageRow>, RepoError> {
        let rows = sqlx::query(
            "SELECT path, measured_at_utc, tz_offset, bytes FROM disk_usage \
             ORDER BY measured_at_utc DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.disk_stats_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        rows.iter()
            .map(usage_row)
            .collect::<Result<_, _>>()
            .map_err(decode_error)
    }

    async fn disk_usage_between(
        &self,
        path: &str,
        since_utc: &str,
        until_utc: &str,
    ) -> Result<Vec<DiskUsageRow>, RepoError> {
        let before = sqlx::query(
            "SELECT path, measured_at_utc, tz_offset, bytes FROM disk_usage \
             WHERE path = ? AND measured_at_utc < ? ORDER BY measured_at_utc DESC LIMIT 1",
        )
        .bind(path)
        .bind(since_utc)
        .fetch_all(&self.disk_stats_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        let during = sqlx::query(
            "SELECT path, measured_at_utc, tz_offset, bytes FROM disk_usage \
             WHERE path = ? AND measured_at_utc >= ? AND measured_at_utc <= ? \
             ORDER BY measured_at_utc",
        )
        .bind(path)
        .bind(since_utc)
        .bind(until_utc)
        .fetch_all(&self.disk_stats_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        before
            .iter()
            .chain(&during)
            .map(usage_row)
            .collect::<Result<_, _>>()
            .map_err(decode_error)
    }

    async fn record_disk_free(&self, row: &DiskFreeRow) -> Result<(), RepoError> {
        sqlx::query(
            "INSERT OR REPLACE INTO disk_free \
             (measured_at_utc, tz_offset, available_bytes, total_bytes) VALUES (?, ?, ?, ?)",
        )
        .bind(&row.measured_at_utc)
        .bind(&row.tz_offset)
        .bind(row.available_bytes)
        .bind(row.total_bytes)
        .execute(&self.disk_stats_pool)
        .await
        .map_err(|e| RepoError::Internal(format!("insert disk_free: {e}")))?;
        Ok(())
    }

    async fn recent_disk_free(&self, limit: usize) -> Result<Vec<DiskFreeRow>, RepoError> {
        let rows = sqlx::query(
            "SELECT measured_at_utc, tz_offset, available_bytes, total_bytes FROM disk_free \
             ORDER BY measured_at_utc DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.disk_stats_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        rows.iter()
            .map(disk_free_row)
            .collect::<Result<_, _>>()
            .map_err(decode_error)
    }

    async fn list_remote_allows(&self) -> Result<Vec<RemoteMediaAllowRow>, RepoError> {
        let rows = sqlx::query(
            "SELECT allow_uuid, scope, key, created_at_utc, tz_offset FROM remote_media_allow \
             ORDER BY created_at_utc DESC",
        )
        .fetch_all(&self.remote_media_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        rows.iter()
            .map(allow_row)
            .collect::<Result<_, _>>()
            .map_err(decode_error)
    }

    async fn allow_remote(
        &self,
        scope: AllowScope,
        key: &str,
    ) -> Result<RemoteMediaAllowRow, RepoError> {
        let mut conn = self
            .remote_media_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        let existing = sqlx::query(
            "SELECT allow_uuid, scope, key, created_at_utc, tz_offset FROM remote_media_allow \
             WHERE scope = ? AND key = ?",
        )
        .bind(scope.as_str())
        .bind(key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        if let Some(r) = existing {
            return allow_row(&r).map_err(decode_error);
        }
        let (created_at_utc, tz_offset) =
            datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let row = RemoteMediaAllowRow {
            allow_uuid: uuid::Uuid::new_v4().to_string(),
            scope: scope.as_str().to_string(),
            key: key.to_string(),
            created_at_utc,
            tz_offset: Some(tz_offset),
        };
        sqlx::query(
            "INSERT INTO remote_media_allow (allow_uuid, scope, key, created_at_utc, tz_offset) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&row.allow_uuid)
        .bind(&row.scope)
        .bind(&row.key)
        .bind(&row.created_at_utc)
        .bind(&row.tz_offset)
        .execute(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("insert remote_media_allow: {e}")))?;
        let msg = format!("remote media: allow {} {}", row.scope, row.key);
        self.commit_version(&mut conn, &msg).await?;
        Ok(row)
    }

    async fn delete_remote_allow(&self, allow_uuid: &str) -> Result<bool, RepoError> {
        let mut conn = self
            .remote_media_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        let done = sqlx::query("DELETE FROM remote_media_allow WHERE allow_uuid = ?")
            .bind(allow_uuid)
            .execute(&mut *conn)
            .await
            .map_err(|e| RepoError::Internal(format!("delete remote_media_allow: {e}")))?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        let msg = format!("remote media: forget {allow_uuid}");
        self.commit_version(&mut conn, &msg).await?;
        Ok(true)
    }

    async fn get_remote_media(&self, url: &str) -> Result<Option<RemoteMediaRow>, RepoError> {
        let row = sqlx::query(
            "SELECT url, sha256, content_type, byte_size, fetched_at_utc, tz_offset \
             FROM remote_media WHERE url = ?",
        )
        .bind(url)
        .fetch_optional(&self.remote_media_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        row.as_ref()
            .map(media_row)
            .transpose()
            .map_err(decode_error)
    }

    async fn list_remote_media(&self) -> Result<Vec<RemoteMediaRow>, RepoError> {
        let rows = sqlx::query(
            "SELECT url, sha256, content_type, byte_size, fetched_at_utc, tz_offset \
             FROM remote_media ORDER BY fetched_at_utc DESC",
        )
        .fetch_all(&self.remote_media_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        rows.iter()
            .map(media_row)
            .collect::<Result<_, _>>()
            .map_err(decode_error)
    }

    async fn record_remote_media(&self, row: RemoteMediaRow) -> Result<(), RepoError> {
        let mut conn = self
            .remote_media_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        sqlx::query(
            "INSERT OR REPLACE INTO remote_media \
             (url, sha256, content_type, byte_size, fetched_at_utc, tz_offset) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.url)
        .bind(&row.sha256)
        .bind(&row.content_type)
        .bind(row.byte_size)
        .bind(&row.fetched_at_utc)
        .bind(&row.tz_offset)
        .execute(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("insert remote_media: {e}")))?;
        let msg = format!("remote media: fetched {}", row.url);
        self.commit_version(&mut conn, &msg).await?;
        Ok(())
    }
}

fn decode_error(e: sqlx::Error) -> RepoError {
    RepoError::Internal(format!("decode row: {e}"))
}

fn usage_row(r: &sqlx::sqlite::SqliteRow) -> Result<DiskUsageRow, sqlx::Error> {
    Ok(DiskUsageRow {
        path: r.try_get("path")?,
        measured_at_utc: r.try_get("measured_at_utc")?,
        tz_offset: r.try_get("tz_offset")?,
        bytes: r.try_get("bytes")?,
    })
}

fn disk_free_row(r: &sqlx::sqlite::SqliteRow) -> Result<DiskFreeRow, sqlx::Error> {
    Ok(DiskFreeRow {
        measured_at_utc: r.try_get("measured_at_utc")?,
        tz_offset: r.try_get("tz_offset")?,
        available_bytes: r.try_get("available_bytes")?,
        total_bytes: r.try_get("total_bytes")?,
    })
}

fn allow_row(r: &sqlx::sqlite::SqliteRow) -> Result<RemoteMediaAllowRow, sqlx::Error> {
    Ok(RemoteMediaAllowRow {
        allow_uuid: r.try_get("allow_uuid")?,
        scope: r.try_get("scope")?,
        key: r.try_get("key")?,
        created_at_utc: r.try_get("created_at_utc")?,
        tz_offset: r.try_get("tz_offset")?,
    })
}

fn media_row(r: &sqlx::sqlite::SqliteRow) -> Result<RemoteMediaRow, sqlx::Error> {
    Ok(RemoteMediaRow {
        url: r.try_get("url")?,
        sha256: r.try_get("sha256")?,
        content_type: r.try_get("content_type")?,
        byte_size: r.try_get("byte_size")?,
        fetched_at_utc: r.try_get("fetched_at_utc")?,
        tz_offset: r.try_get("tz_offset")?,
    })
}

/// Ask the linked libsqlite3 whether `dolt_commit` is a registered
/// scalar function. `pragma_function_list` is a SQLite built-in
/// table-valued pragma that's been there since 3.30; doltlite inherits
/// it. Probe failures fall through to `false` — we'd rather skip the
/// audit trail than refuse to start.
async fn probe_dolt_extensions(pool: &SqlitePool) -> bool {
    let res = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pragma_function_list WHERE name = 'dolt_commit'",
    )
    .fetch_one(pool)
    .await;
    matches!(res, Ok(n) if n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_schema::disk_usage::ROOT_PATH;

    fn sample(path: &str, at: &str, bytes: i64) -> DiskUsageRow {
        DiskUsageRow {
            path: path.to_string(),
            measured_at_utc: at.to_string(),
            tz_offset: None,
            bytes,
        }
    }

    /// An allow is one row however often it is asked for, comes back
    /// in the list until deleted, and a fetched URL is one row that a
    /// second fetch replaces.
    #[tokio::test]
    async fn remote_media_allows_are_unique_and_deletable() {
        let td = tempfile::tempdir().unwrap();
        let store = AppStore::open(td.path()).await.unwrap();
        let first = store
            .allow_remote(AllowScope::Host, "cdn.example")
            .await
            .unwrap();
        let again = store
            .allow_remote(AllowScope::Host, "cdn.example")
            .await
            .unwrap();
        assert_eq!(first.allow_uuid, again.allow_uuid);
        let other = store
            .allow_remote(AllowScope::Url, "https://cdn.example/a.png")
            .await
            .unwrap();
        let listed = store.list_remote_allows().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|r| AllowScope::parse(&r.scope).is_some()));
        assert!(store.delete_remote_allow(&first.allow_uuid).await.unwrap());
        assert!(!store.delete_remote_allow(&first.allow_uuid).await.unwrap());
        let listed = store.list_remote_allows().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].allow_uuid, other.allow_uuid);

        let fetched = |sha: &str| RemoteMediaRow {
            url: "https://cdn.example/a.png".into(),
            sha256: sha.into(),
            content_type: "image/png".into(),
            byte_size: 3,
            fetched_at_utc: "2026-09-22T00:00:00.000000Z".into(),
            tz_offset: Some("+00:00".into()),
        };
        assert!(store
            .get_remote_media("https://cdn.example/a.png")
            .await
            .unwrap()
            .is_none());
        store.record_remote_media(fetched("aaa")).await.unwrap();
        store.record_remote_media(fetched("bbb")).await.unwrap();
        let got = store
            .get_remote_media("https://cdn.example/a.png")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.sha256, "bbb");
        assert_eq!(store.list_remote_media().await.unwrap().len(), 1);
    }

    /// The free-space series sits in disk stats beside disk usage,
    /// keeps every sample, and reads back newest first — what the status
    /// bar's sparkline is seeded from after a restart.
    #[tokio::test]
    async fn disk_free_keeps_every_sample_newest_first() {
        let td = tempfile::tempdir().unwrap();
        let store = AppStore::open(td.path()).await.unwrap();
        for (at, available) in [
            ("2026-10-10T17:00:00.000000Z", 50),
            ("2026-10-10T17:00:10.000000Z", 40),
        ] {
            store
                .record_disk_free(&DiskFreeRow {
                    measured_at_utc: at.into(),
                    tz_offset: Some("-07:00".into()),
                    available_bytes: available,
                    total_bytes: 100,
                })
                .await
                .unwrap();
        }
        let back = store.recent_disk_free(10).await.unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].available_bytes, 40);
        assert_eq!(back[1].measured_at_utc, "2026-10-10T17:00:00.000000Z");
    }

    /// The disk-usage timeseries round-trips, and — the part worth
    /// pinning — one series holds *many* rows. A single-column primary
    /// key would make each write replace the last, which reads exactly
    /// like a working store right up until someone asks for a history.
    #[tokio::test]
    async fn disk_usage_keeps_every_sample_of_a_series() {
        let td = tempfile::tempdir().unwrap();
        let store = AppStore::open(td.path()).await.unwrap();
        store
            .record_disk_usage(&[
                sample(ROOT_PATH, "2026-09-02T10:00:00-07:00", 100),
                sample(ROOT_PATH, "2026-09-02T10:00:05-07:00", 180),
                sample("slack/raw", "2026-09-02T10:00:05-07:00", 80),
            ])
            .await
            .unwrap();

        let back = store.recent_disk_usage(50).await.unwrap();
        assert_eq!(back.len(), 3, "a series must keep more than its newest row");
        // Newest first, so the two root samples bracket the read.
        assert_eq!(back[0].measured_at_utc, "2026-09-02T10:00:05-07:00");
        let root: Vec<i64> = back
            .iter()
            .filter(|r| r.path == ROOT_PATH)
            .map(|r| r.bytes)
            .collect();
        assert_eq!(root, vec![180, 100]);
    }

    /// Retention drops the samples of both series from before the cutoff,
    /// but keeps each series' newest one at or before it: a tree that has
    /// not moved since is still drawn, from that value, in a later run's
    /// chart. The cutoff is passed in, so no clock is read.
    #[tokio::test]
    async fn disk_stats_before_the_cutoff_are_forgotten_but_the_carry_in_stays() {
        let td = tempfile::tempdir().unwrap();
        let store = AppStore::open(td.path()).await.unwrap();
        store
            .record_disk_usage(&[
                sample(ROOT_PATH, "2026-08-01T00:00:00.000000+00:00", 1),
                sample(ROOT_PATH, "2026-08-31T23:59:59.999999+00:00", 2),
                sample(ROOT_PATH, "2026-09-20T00:00:00.000000+00:00", 3),
                sample("a/ingest", "2026-08-01T00:00:00.000000+00:00", 10),
                sample("a/ingest", "2026-08-15T00:00:00.000000+00:00", 11),
                sample("b/ingest", "2026-07-01T00:00:00.000000+00:00", 20),
                sample("c/ingest", "2026-08-01T00:00:00.000000+00:00", 30),
                sample("c/ingest", "2026-09-01T00:00:00.000000+00:00", 31),
            ])
            .await
            .unwrap();
        for (at, available) in [
            ("2026-08-01T00:00:00.000000+00:00", 50),
            ("2026-08-20T00:00:00.000000+00:00", 40),
            ("2026-09-10T00:00:00.000000+00:00", 30),
        ] {
            store
                .record_disk_free(&DiskFreeRow {
                    measured_at_utc: at.into(),
                    tz_offset: Some("+00:00".into()),
                    available_bytes: available,
                    total_bytes: 100,
                })
                .await
                .unwrap();
        }

        let gone = store
            .forget_disk_stats_before("2026-09-01T00:00:00.000000+00:00")
            .await
            .unwrap();
        assert_eq!(gone, 4);

        let mut kept: Vec<(String, i64)> = store
            .recent_disk_usage(50)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.path, r.bytes))
            .collect();
        kept.sort();
        let want = [
            (ROOT_PATH, 2),
            (ROOT_PATH, 3),
            ("a/ingest", 11),
            ("b/ingest", 20),
            ("c/ingest", 31),
        ]
        .map(|(p, b)| (p.to_string(), b));
        assert_eq!(kept, want);
        let free: Vec<i64> = store
            .recent_disk_free(10)
            .await
            .unwrap()
            .iter()
            .map(|r| r.available_bytes)
            .collect();
        assert_eq!(free, [30, 40]);

        let again = store
            .forget_disk_stats_before("2026-09-01T00:00:00.000000+00:00")
            .await
            .unwrap();
        assert_eq!(again, 0, "a second pass over the same cutoff drops nothing");
    }

    /// A row stored without an offset reads back as `None`, not `Some("")`:
    /// a bare `String` read of a NULL succeeds with `""` in sqlx.
    #[tokio::test]
    async fn a_null_tz_offset_reads_back_as_none() {
        let td = tempfile::tempdir().unwrap();
        let store = AppStore::open(td.path()).await.unwrap();
        let mut stamped = sample(ROOT_PATH, "2026-09-02T10:00:05-07:00", 180);
        stamped.tz_offset = Some("-07:00".into());
        store
            .record_disk_usage(&[sample(ROOT_PATH, "2026-09-02T10:00:00-07:00", 100), stamped])
            .await
            .unwrap();
        let offsets: Vec<Option<String>> = store
            .recent_disk_usage(10)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.tz_offset)
            .collect();
        assert_eq!(offsets, vec![Some("-07:00".into()), None]);

        let between = store
            .disk_usage_between(ROOT_PATH, "2026-09-02T00:00:00", "2026-09-03T00:00:00")
            .await
            .unwrap();
        assert_eq!(between[0].tz_offset, None);
    }

    /// A data root from before the stamps moved to `<x>_at_utc` +
    /// `tz_offset` (#427) still has the old column names, and
    /// `CREATE TABLE IF NOT EXISTS` leaves them. Opening such a root
    /// has to rename the columns and rewrite the stamps — every read of
    /// a store returned 500 on a real root before it did — and must
    /// keep the rows: feedback is filed by a person and nothing
    /// regenerates it.
    #[tokio::test]
    async fn a_store_from_before_the_utc_columns_is_migrated_on_open() {
        let td = tempfile::tempdir().unwrap();
        // The old shape, by hand, in both stores.
        {
            let feedback = open_pool(&crate::layout::feedback_db(td.path()))
                .await
                .unwrap();
            sqlx::query(
                "CREATE TABLE feedback (feedback_uuid VARCHAR(36) NOT NULL, \
                 created_at VARCHAR(40) NOT NULL, sentiment VARCHAR(8), comment TEXT NOT NULL, \
                 app_version VARCHAR(32) NOT NULL, git_hash VARCHAR(40) NOT NULL, \
                 context_json JSON NOT NULL, fixed_in_git_hash VARCHAR(40), notes TEXT, \
                 PRIMARY KEY (feedback_uuid))",
            )
            .execute(&feedback)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO feedback (feedback_uuid, created_at, comment, app_version, git_hash, \
                 context_json) VALUES ('fb-1', '2026-09-01T08:30:00-07:00', 'the bar is bizarre', \
                 '0.1', 'abc', '{}')",
            )
            .execute(&feedback)
            .await
            .unwrap();
            feedback.close().await;

            let usage = open_pool(&crate::layout::legacy_usage_db(td.path()))
                .await
                .unwrap();
            sqlx::query(
                "CREATE TABLE disk_usage (path VARCHAR(512) NOT NULL, \
                 measured_at VARCHAR(40) NOT NULL, bytes BIGINT NOT NULL, \
                 PRIMARY KEY (path, measured_at))",
            )
            .execute(&usage)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO disk_usage (path, measured_at, bytes) \
                 VALUES ('.', '2026-09-10T10:00:00+02:00', 100)",
            )
            .execute(&usage)
            .await
            .unwrap();
            usage.close().await;
        }

        let store = AppStore::open(td.path()).await.expect("open migrates");

        // The feedback row is still there, and still readable by the
        // current queries.
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM feedback WHERE created_at_utc = '2026-09-01T15:30:00.000000+00:00' \
             AND tz_offset = '-07:00'",
        )
        .fetch_one(store.feedback_pool())
        .await
        .unwrap();
        assert_eq!(n, 1);

        // The old usage store's row climbed the stamp rung on its way
        // into disk stats.
        let usage = store.recent_disk_usage(10).await.unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].measured_at_utc, "2026-09-10T08:00:00.000000+00:00");

        // The rung is recorded: feedback is at its ladder's top, and its
        // rung is its own commit. Disk stats was born past the rung.
        let meta = datalib_store_meta::read(&store.feedback_pool)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.schema_version, 1);
        if store.has_dolt {
            let messages: Vec<String> = sqlx::query_scalar("SELECT message FROM dolt_log()")
                .fetch_all(store.feedback_pool())
                .await
                .unwrap();
            assert!(
                messages
                    .iter()
                    .any(|m| m.starts_with("migrate v1: stamps to utc")),
                "{messages:?}"
            );
        }

        // Opening again is a no-op: nothing to rename, nothing rewritten.
        drop(store);
        let again = AppStore::open(td.path()).await.unwrap();
        assert_eq!(again.recent_disk_usage(10).await.unwrap().len(), 1);
    }

    /// Each of the stores says which build wrote it, with its own
    /// kind; feedback's rows are committed on the spot rather than left
    /// to ride into the next filed feedback, and a second open by the
    /// same build commits nothing more.
    #[tokio::test]
    async fn every_app_store_names_the_build_that_wrote_it() {
        let td = tempfile::tempdir().unwrap();
        let store = AppStore::open(td.path()).await.unwrap();
        for (pool, kind) in [
            (&store.feedback_pool, StoreKind::Feedback),
            (&store.disk_stats_pool, StoreKind::DiskStats),
            (&store.remote_media_pool, StoreKind::RemoteMedia),
        ] {
            let meta = datalib_store_meta::read(pool)
                .await
                .unwrap()
                .expect("written at open");
            assert_eq!(meta.store_kind, Some(kind));
            assert_eq!(
                meta.datalib_version,
                datalib_runtime::build_id::DATALIB_VERSION
            );
        }
        if !store.has_dolt {
            return;
        }
        let commits = |pool: &SqlitePool| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM dolt_log()")
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        let first = commits(&store.feedback_pool).await;
        // Newest first without an ORDER BY: `date` is whole seconds and
        // this test fits inside one.
        let message: String = sqlx::query_scalar("SELECT message FROM dolt_log() LIMIT 1")
            .fetch_one(&store.feedback_pool)
            .await
            .unwrap();
        assert!(
            message.starts_with("meta: written by datalib "),
            "feedback commits its meta rows on open, got {message:?}"
        );
        drop(store);
        let again = AppStore::open(td.path()).await.unwrap();
        assert_eq!(
            commits(&again.feedback_pool).await,
            first,
            "the same build opening again has nothing to commit"
        );
    }

    /// A root whose app stores a newer line of datalib wrote is refused
    /// before the migration or the DDL runs, naming both versions.
    #[tokio::test]
    async fn app_stores_a_newer_build_wrote_are_refused() {
        let td = tempfile::tempdir().unwrap();
        drop(AppStore::open(td.path()).await.unwrap());
        let usage = open_plain_pool(&crate::layout::disk_stats_db(td.path()))
            .await
            .unwrap();
        sqlx::query("UPDATE _datalib_meta SET value = '99.0.0' WHERE key = 'datalib_version'")
            .execute(&usage)
            .await
            .unwrap();
        usage.close().await;
        let err = match AppStore::open(td.path()).await {
            Ok(_) => panic!("an older build opened a newer root"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("was written by datalib 99.0.0"), "{err}");
    }

    /// A root from before `disk_stats` keeps both series in the doltlite
    /// `usage` store. Opening it copies every row into the plain-SQLite
    /// file and removes the old one with what the engine kept beside it;
    /// opening again adds nothing.
    #[tokio::test]
    async fn the_old_usage_store_is_carried_into_disk_stats_and_removed() {
        let td = tempfile::tempdir().unwrap();
        let legacy = crate::layout::legacy_usage_db(td.path());
        {
            let usage = open_pool(&legacy).await.unwrap();
            for (_, ddl) in DISK_STATS_DDL {
                sqlx::query(*ddl).execute(&usage).await.unwrap();
            }
            for (path, at, bytes) in [
                (".", "2026-10-01T10:00:00.000000Z", 100),
                (".", "2026-10-01T10:00:05.000000Z", 120),
                ("slack/ingest", "2026-10-01T10:00:05.000000Z", 80),
            ] {
                sqlx::query(
                    "INSERT INTO disk_usage (path, measured_at_utc, tz_offset, bytes) \
                     VALUES (?, ?, '-07:00', ?)",
                )
                .bind(path)
                .bind(at)
                .bind(bytes)
                .execute(&usage)
                .await
                .unwrap();
            }
            sqlx::query(
                "INSERT INTO disk_free (measured_at_utc, tz_offset, available_bytes, total_bytes) \
                 VALUES ('2026-10-01T10:00:00.000000Z', '-07:00', 40, 100)",
            )
            .execute(&usage)
            .await
            .unwrap();
            usage.close().await;
        }
        let magic = std::fs::read(&legacy).unwrap();
        assert!(
            magic.starts_with(b"CTLD"),
            "a real doltlite store: {:?}",
            &magic[..8]
        );
        std::fs::write(legacy.with_extension("doltlite_db.lock"), b"").unwrap();

        let store = AppStore::open(td.path()).await.unwrap();
        assert_eq!(store.recent_disk_usage(10).await.unwrap().len(), 3);
        assert_eq!(
            store.recent_disk_free(10).await.unwrap()[0].available_bytes,
            40
        );
        let left: Vec<String> = std::fs::read_dir(crate::layout::system_dir(td.path()))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("usage"))
            .collect();
        assert!(
            left.is_empty(),
            "the old store and its sidecars stay: {left:?}"
        );
        // What a stock `sqlite3` opens, not doltlite's own format.
        let head = std::fs::read(crate::layout::disk_stats_db(td.path())).unwrap();
        assert!(head.starts_with(b"SQLite format 3\0"), "{:?}", &head[..16]);

        drop(store);
        let again = AppStore::open(td.path()).await.unwrap();
        assert_eq!(again.recent_disk_usage(10).await.unwrap().len(), 3);
    }

    /// An old store the copy cannot read costs its history, never the
    /// app: the stores open, and the file is left for a later open.
    #[tokio::test]
    async fn an_unreadable_old_usage_store_is_left_and_the_app_opens() {
        let td = tempfile::tempdir().unwrap();
        let legacy = crate::layout::legacy_usage_db(td.path());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"not a database at all, not even close").unwrap();
        let store = AppStore::open(td.path()).await.expect("opens all the same");
        assert!(store.recent_disk_usage(10).await.unwrap().is_empty());
        assert!(legacy.is_file(), "left for the next open to try again");
    }

    /// Re-recording the same (series, instant) overwrites rather than
    /// failing the whole batch — two walks finishing inside one
    /// timestamp tick carry the same number anyway.
    #[tokio::test]
    async fn a_repeated_instant_replaces_rather_than_erroring() {
        let td = tempfile::tempdir().unwrap();
        let store = AppStore::open(td.path()).await.unwrap();
        let at = "2026-09-02T10:00:00-07:00";
        store
            .record_disk_usage(&[sample("a/raw", at, 1)])
            .await
            .unwrap();
        store
            .record_disk_usage(&[sample("a/raw", at, 2)])
            .await
            .unwrap();
        let back = store.recent_disk_usage(50).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].bytes, 2);
    }
}
