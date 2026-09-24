//! `AppStore` — the three stores this server owns: filed feedback, the
//! bytes-on-disk timeseries, and remote media (the allow-list and the
//! download CAS's index), one doltlite file each.

use crate::app_store_migrate::{DISK_USAGE_LADDER, FEEDBACK_LADDER, REMOTE_MEDIA_LADDER};
use crate::repo::{AppRepo, RepoError};
use crate::store::open_pool;
use app_schema::disk_usage::{DiskUsageRow, DDL as DISK_USAGE_DDL};
use app_schema::feedback::{FeedbackRow, DDL as FEEDBACK_DDL};
use app_schema::remote_media::allow::RemoteMediaAllowRow;
use app_schema::remote_media::media::RemoteMediaRow;
use app_schema::remote_media::{AllowScope, DDL as REMOTE_MEDIA_DDL};
use async_trait::async_trait;
use datalib_store_meta::StoreKind;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// The three application stores: filed feedback, the bytes-on-disk
/// timeseries, and remote media.
pub struct AppStore {
    /// Filed feedback. Outside the cache-tagged index tree, because
    /// nothing regenerates it.
    feedback_pool: SqlitePool,
    /// The disk-usage timeseries. Written every few seconds while the
    /// server is up, and never committed — so it must not share a file
    /// with anything that is.
    usage_pool: SqlitePool,
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
        let usage_pool = open_pool(&crate::layout::usage_db(root)).await?;
        let remote_media_pool = open_pool(&crate::layout::remote_media_db(root)).await?;
        let has_dolt = probe_dolt_extensions(&feedback_pool).await;
        let internal = |e: anyhow::Error| sqlx::Error::Protocol(format!("{e:#}"));
        // Before the ladder and the DDL: a store a newer line of datalib
        // wrote is refused whole (`datalib_store_meta::guard`). Then the
        // rungs above the store's version. Feedback is committed per row
        // with `-Am`, so each rung is sealed as its own commit; usage is
        // never committed, by design.
        for (pool, path, ladder, commits) in [
            (
                &feedback_pool,
                crate::layout::feedback_db(root),
                FEEDBACK_LADDER,
                has_dolt,
            ),
            (
                &usage_pool,
                crate::layout::usage_db(root),
                DISK_USAGE_LADDER,
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
                for p in [&feedback_pool, &usage_pool, &remote_media_pool] {
                    p.close().await;
                }
                return Err(sqlx::Error::Configuration(Box::new(newer)));
            }
            let stored = written_by.map_or(0, |m| m.schema_version);
            let top = datalib_store_meta::ladder::top(ladder);
            if stored > top {
                for p in [&feedback_pool, &usage_pool, &remote_media_pool] {
                    p.close().await;
                }
                return Err(sqlx::Error::Configuration(Box::new(
                    datalib_store_meta::ladder::AheadOfLadder { stored, top },
                )));
            }
            for rung in datalib_store_meta::ladder::pending(ladder, stored).map_err(internal)? {
                sqlx::query(datalib_store_meta::DDL).execute(pool).await?;
                datalib_store_meta::ladder::apply(pool, rung)
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
            usage_pool,
            remote_media_pool,
            has_dolt,
        };
        store.init_feedback_table().await?;
        store.init_disk_usage_table().await?;
        store.init_remote_media_tables().await?;
        // Which build wrote each store, beside its tables. Feedback and
        // remote media are committed per row with `-Am`, so a changed
        // meta row is sealed here rather than left to ride into the next
        // commit; usage is never committed and its rows just land.
        for (pool, kind, ddl, ladder) in [
            (
                &store.feedback_pool,
                StoreKind::Feedback,
                FEEDBACK_DDL,
                FEEDBACK_LADDER,
            ),
            (
                &store.usage_pool,
                StoreKind::Usage,
                DISK_USAGE_DDL,
                DISK_USAGE_LADDER,
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
            let changed = datalib_store_meta::write(pool, kind, &hash, top)
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
    async fn init_disk_usage_table(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in DISK_USAGE_DDL {
            sqlx::query(*ddl).execute(&self.usage_pool).await?;
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
        // The INSERT and the `dolt_commit` ride the same connection so
        // the commit covers exactly the row we just wrote, with no
        // chance of a concurrent writer's INSERT slipping into the same
        // dolt_log entry. (The pool may hand a different connection to
        // a sibling task, which is fine — doltlite's working set is
        // per-file, not per-connection.)
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
        let mut conn = self
            .usage_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
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
            .execute(&mut *conn)
            .await
            .map_err(|e| RepoError::Internal(format!("insert disk_usage: {e}")))?;
        }
        // No DOLT_COMMIT: the rows are the history. See the module docs
        // on `app_schema::disk_usage`.
        Ok(())
    }

    async fn recent_disk_usage(&self, limit: usize) -> Result<Vec<DiskUsageRow>, RepoError> {
        let rows = sqlx::query(
            "SELECT path, measured_at_utc, tz_offset, bytes FROM disk_usage \
             ORDER BY measured_at_utc DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.usage_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        Ok(rows
            .iter()
            .map(|r| DiskUsageRow {
                path: r.try_get("path").unwrap_or_default(),
                measured_at_utc: r.try_get("measured_at_utc").unwrap_or_default(),
                tz_offset: r.try_get("tz_offset").ok(),
                bytes: r.try_get("bytes").unwrap_or_default(),
            })
            .collect())
    }

    async fn list_remote_allows(&self) -> Result<Vec<RemoteMediaAllowRow>, RepoError> {
        let rows = sqlx::query(
            "SELECT allow_uuid, scope, key, created_at_utc, tz_offset FROM remote_media_allow \
             ORDER BY created_at_utc DESC",
        )
        .fetch_all(&self.remote_media_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        Ok(rows.iter().map(allow_row).collect())
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
            return Ok(allow_row(&r));
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
        Ok(row.as_ref().map(media_row))
    }

    async fn list_remote_media(&self) -> Result<Vec<RemoteMediaRow>, RepoError> {
        let rows = sqlx::query(
            "SELECT url, sha256, content_type, byte_size, fetched_at_utc, tz_offset \
             FROM remote_media ORDER BY fetched_at_utc DESC",
        )
        .fetch_all(&self.remote_media_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        Ok(rows.iter().map(media_row).collect())
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

fn allow_row(r: &sqlx::sqlite::SqliteRow) -> RemoteMediaAllowRow {
    RemoteMediaAllowRow {
        allow_uuid: r.try_get("allow_uuid").unwrap_or_default(),
        scope: r.try_get("scope").unwrap_or_default(),
        key: r.try_get("key").unwrap_or_default(),
        created_at_utc: r.try_get("created_at_utc").unwrap_or_default(),
        tz_offset: r.try_get("tz_offset").ok(),
    }
}

fn media_row(r: &sqlx::sqlite::SqliteRow) -> RemoteMediaRow {
    RemoteMediaRow {
        url: r.try_get("url").unwrap_or_default(),
        sha256: r.try_get("sha256").unwrap_or_default(),
        content_type: r.try_get("content_type").unwrap_or_default(),
        byte_size: r.try_get("byte_size").unwrap_or_default(),
        fetched_at_utc: r.try_get("fetched_at_utc").unwrap_or_default(),
        tz_offset: r.try_get("tz_offset").ok(),
    }
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

            let usage = open_pool(&crate::layout::usage_db(td.path()))
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

        let usage = store.recent_disk_usage(10).await.unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].measured_at_utc, "2026-09-10T08:00:00.000000+00:00");

        // The rung is recorded: every store is at the ladder's top, and
        // feedback's rung is its own commit.
        for pool in [&store.feedback_pool, &store.usage_pool] {
            let meta = datalib_store_meta::read(pool).await.unwrap().unwrap();
            assert_eq!(meta.schema_version, 1);
        }
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
            (&store.usage_pool, StoreKind::Usage),
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
        let usage = open_pool(&crate::layout::usage_db(td.path()))
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
