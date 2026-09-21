//! `AppStore` — the three stores this server owns: filed feedback, the
//! sync job queue, and the bytes-on-disk timeseries, one doltlite file
//! each.

use crate::app_store_migrate::{migrate_stamps, DISK_USAGE, FEEDBACK, SYNC_JOBS};
use crate::repo::{AppRepo, RepoError};
use crate::store::open_pool;
use app_schema::disk_usage::{DiskUsageRow, DDL as DISK_USAGE_DDL};
use app_schema::feedback::{FeedbackRow, DDL as FEEDBACK_DDL};
use app_schema::sync_jobs::{JobKind, JobState, SyncJobRow, DDL as SYNC_JOBS_DDL};
use async_trait::async_trait;
use datalib_store_meta::StoreKind;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// The three application stores: filed feedback, the sync job queue,
/// and the bytes-on-disk timeseries.
pub struct AppStore {
    /// Filed feedback. Outside the cache-tagged index tree, because
    /// nothing regenerates it.
    feedback_pool: SqlitePool,
    /// The sync job queue and its history.
    jobs_pool: SqlitePool,
    /// The disk-usage timeseries. Written every few seconds while the
    /// server is up, and never committed — so it must not share a file
    /// with anything that is.
    usage_pool: SqlitePool,
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
    /// file is untouched — which is why a store from before a column
    /// rename is migrated first (`app_store_migrate`).
    pub async fn open(root: &std::path::Path) -> Result<Self, sqlx::Error> {
        let feedback_pool = open_pool(&crate::layout::feedback_db(root)).await?;
        let jobs_pool = open_pool(&crate::layout::jobs_db(root)).await?;
        let usage_pool = open_pool(&crate::layout::usage_db(root)).await?;
        let has_dolt = probe_dolt_extensions(&feedback_pool).await;
        let store = Self {
            feedback_pool,
            jobs_pool,
            usage_pool,
            has_dolt,
        };
        for (pool, spec) in [
            (&store.feedback_pool, &FEEDBACK),
            (&store.jobs_pool, &SYNC_JOBS),
        ] {
            if migrate_stamps(pool, spec).await? && has_dolt {
                // The rewrite is a change to a versioned file; leaving it
                // in the working set would fold it into whichever commit
                // comes next.
                sqlx::query("SELECT dolt_commit('-Am', ?)")
                    .bind(format!("migrate {}: stamps to utc + tz_offset", spec.table))
                    .execute(pool)
                    .await?;
            }
        }
        // Usage is the store nothing commits, by design.
        migrate_stamps(&store.usage_pool, &DISK_USAGE).await?;
        store.init_feedback_table().await?;
        store.init_sync_jobs_table().await?;
        store.init_disk_usage_table().await?;
        // Which build wrote each store, beside its tables. Feedback is
        // committed per row with `-Am`, so a changed meta row is sealed
        // here rather than left to ride into the next feedback commit;
        // jobs and usage are never committed and their rows just land.
        for (pool, kind, ddl) in [
            (&store.feedback_pool, StoreKind::Feedback, FEEDBACK_DDL),
            (&store.jobs_pool, StoreKind::Jobs, SYNC_JOBS_DDL),
            (&store.usage_pool, StoreKind::Usage, DISK_USAGE_DDL),
        ] {
            let hash = datalib_store_meta::schema_hash(ddl.iter().map(|(_t, d)| *d));
            let changed = datalib_store_meta::write(pool, kind, &hash, 0)
                .await
                .map_err(|e| sqlx::Error::Protocol(format!("_datalib_meta: {e:#}")))?;
            if changed && has_dolt && kind == StoreKind::Feedback {
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
    async fn init_sync_jobs_table(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in SYNC_JOBS_DDL {
            sqlx::query(*ddl).execute(&self.jobs_pool).await?;
        }
        Ok(())
    }
    async fn init_disk_usage_table(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in DISK_USAGE_DDL {
            sqlx::query(*ddl).execute(&self.usage_pool).await?;
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
    async fn list_jobs(
        &self,
        only_active: bool,
        limit: usize,
    ) -> Result<Vec<SyncJobRow>, RepoError> {
        let base = "SELECT id, source_ids, kind, parent_job_id, state, created_at_utc, \
                           started_at_utc, finished_at_utc, tz_offset, error, pid, \
                           progress_pct, progress_msg \
                    FROM sync_jobs";
        // The SQL form of `SyncJobRow::is_active`: a job told to stop is
        // active until the worker stamps it finished.
        let sql = if only_active {
            format!(
                "{base} WHERE state IN (?, ?) \
                 OR (state = ? AND started_at_utc IS NOT NULL AND finished_at_utc IS NULL) \
                 ORDER BY created_at_utc DESC, id DESC LIMIT ?"
            )
        } else {
            format!("{base} ORDER BY created_at_utc DESC, id DESC LIMIT ?")
        };
        // Audited for injection per sqlx 0.9's `SqlSafeStr` bound: `sql` is
        // `format!` over two `&'static str` templates selected by a bool, and
        // every runtime value (the three states, `limit`) is a bound `?`
        // parameter. Nothing caller-supplied reaches the string.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        if only_active {
            q = q
                .bind(JobState::Pending.as_str())
                .bind(JobState::Running.as_str())
                .bind(JobState::Canceled.as_str());
        }
        let rows = q
            .bind(limit as i64)
            .fetch_all(&self.jobs_pool)
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        let mut out: Vec<SyncJobRow> = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(row_to_sync_job(&r));
        }
        Ok(out)
    }
    async fn get_job(&self, job_id: &str) -> Result<Option<SyncJobRow>, RepoError> {
        let sql = "SELECT id, source_ids, kind, parent_job_id, state, created_at_utc, \
                          started_at_utc, finished_at_utc, tz_offset, error, pid, \
                          progress_pct, progress_msg \
                   FROM sync_jobs WHERE id = ? LIMIT 1";
        let row = sqlx::query(sql)
            .bind(job_id)
            .fetch_optional(&self.jobs_pool)
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        Ok(row.as_ref().map(row_to_sync_job))
    }
    async fn enqueue_job(
        &self,
        kind: JobKind,
        source_ids: Option<&str>,
    ) -> Result<SyncJobRow, RepoError> {
        let id = uuid::Uuid::new_v4().to_string();
        let (created_at_utc, tz_offset) =
            datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let row = SyncJobRow {
            id: id.clone(),
            source_ids: source_ids.map(|s| s.to_string()),
            kind: kind.as_str().to_string(),
            parent_job_id: None,
            state: JobState::Pending.as_str().to_string(),
            created_at_utc,
            started_at_utc: None,
            finished_at_utc: None,
            tz_offset: Some(tz_offset),
            error: None,
            pid: None,
            progress_pct: None,
            progress_msg: None,
        };
        let mut conn = self
            .jobs_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        sqlx::query(
            "INSERT INTO sync_jobs \
             (id, source_ids, kind, parent_job_id, state, created_at_utc, \
              started_at_utc, finished_at_utc, tz_offset, error, pid, progress_pct, \
              progress_msg) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.source_ids)
        .bind(&row.kind)
        .bind(&row.parent_job_id)
        .bind(&row.state)
        .bind(&row.created_at_utc)
        .bind(&row.started_at_utc)
        .bind(&row.finished_at_utc)
        .bind(&row.tz_offset)
        .bind(&row.error)
        .bind(row.pid)
        .bind(row.progress_pct)
        .bind(&row.progress_msg)
        .execute(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("insert sync_jobs: {e}")))?;
        // NB: no DOLT_COMMIT here, and the reason has changed. It used
        // to be forced: `sync_jobs` shared a file with the grid index,
        // so a pipeline child committing mid-run would collide with a
        // commit from here. `system/jobs.doltlite_db` has one writer,
        // so that hazard is gone — what remains is that the queue is
        // transient and a per-update dolt history would buy nothing.
        // Queue writes persist as plain SQL in the working set.
        Ok(row)
    }
    async fn request_cancel_job(&self, job_id: &str) -> Result<(), RepoError> {
        let mut conn = self
            .jobs_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        sqlx::query("UPDATE sync_jobs SET state = ? WHERE id = ? AND state IN (?, ?)")
            .bind(JobState::Canceled.as_str())
            .bind(job_id)
            .bind(JobState::Pending.as_str())
            .bind(JobState::Running.as_str())
            .execute(&mut *conn)
            .await
            .map_err(|e| RepoError::Internal(format!("cancel sync_job: {e}")))?;
        // No DOLT_COMMIT — see the note in `enqueue_job`.
        Ok(())
    }
    async fn claim_next_job(&self) -> Result<Option<SyncJobRow>, RepoError> {
        let mut conn = self
            .jobs_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM sync_jobs WHERE state = ? \
             ORDER BY created_at_utc ASC, id ASC LIMIT 1",
        )
        .bind(JobState::Pending.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("claim select: {e}")))?;
        let Some(id) = id else {
            return Ok(None);
        };
        let (started_at_utc, tz_offset) =
            datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
        sqlx::query(
            "UPDATE sync_jobs SET state = ?, started_at_utc = ?, tz_offset = ?, \
             progress_msg = 'starting…' WHERE id = ? AND state = ?",
        )
        .bind(JobState::Running.as_str())
        .bind(&started_at_utc)
        .bind(&tz_offset)
        .bind(&id)
        .bind(JobState::Pending.as_str())
        .execute(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("claim update: {e}")))?;
        // No DOLT_COMMIT — see the note in `enqueue_job`.
        // Re-read so the caller gets the row exactly as persisted.
        let sql = "SELECT id, source_ids, kind, parent_job_id, state, created_at_utc, \
                          started_at_utc, finished_at_utc, tz_offset, error, pid, \
                          progress_pct, progress_msg \
                   FROM sync_jobs WHERE id = ? LIMIT 1";
        let row = sqlx::query(sql)
            .bind(&id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| RepoError::Internal(format!("claim refetch: {e}")))?;
        Ok(row.as_ref().map(row_to_sync_job))
    }
    async fn set_job_pid(&self, job_id: &str, pid: i64) -> Result<(), RepoError> {
        sqlx::query("UPDATE sync_jobs SET pid = ? WHERE id = ?")
            .bind(pid)
            .bind(job_id)
            .execute(&self.jobs_pool)
            .await
            .map_err(|e| RepoError::Internal(format!("set pid: {e}")))?;
        Ok(())
    }
    async fn update_job_progress(
        &self,
        job_id: &str,
        pct: Option<f64>,
        msg: Option<&str>,
    ) -> Result<(), RepoError> {
        // No DOLT_COMMIT here on purpose: progress ticks are high-frequency
        // and would flood `dolt log`. Only the lifecycle transitions
        // (claim / finish) are versioned.
        sqlx::query("UPDATE sync_jobs SET progress_pct = ?, progress_msg = ? WHERE id = ?")
            .bind(pct)
            .bind(msg)
            .bind(job_id)
            .execute(&self.jobs_pool)
            .await
            .map_err(|e| RepoError::Internal(format!("update progress: {e}")))?;
        Ok(())
    }
    async fn finish_job(
        &self,
        job_id: &str,
        state: JobState,
        error: Option<&str>,
    ) -> Result<(), RepoError> {
        let mut conn = self
            .jobs_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        let (finished_at_utc, tz_offset) =
            datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
        sqlx::query(
            "UPDATE sync_jobs SET state = ?, finished_at_utc = ?, tz_offset = ?, error = ?, \
             pid = NULL WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(&finished_at_utc)
        .bind(&tz_offset)
        .bind(error)
        .bind(job_id)
        .execute(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("finish job: {e}")))?;
        // No DOLT_COMMIT — see the note in `enqueue_job`. (This is the
        // transition that actually raced the sync child's commit in
        // testing and produced "commit conflict".)
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

fn row_to_sync_job(r: &sqlx::sqlite::SqliteRow) -> SyncJobRow {
    // Decoded through `Option`, which is what checks for NULL: a bare
    // `String` decode reads a NULL VARCHAR as `""`, and a job that had
    // not finished then carried `finished_at_utc: ""` to every reader.
    fn nullable<'r, T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>>(
        r: &'r sqlx::sqlite::SqliteRow,
        col: &str,
    ) -> Option<T> {
        r.try_get::<Option<T>, _>(col).ok().flatten()
    }
    SyncJobRow {
        id: r.try_get("id").unwrap_or_default(),
        source_ids: nullable(r, "source_ids"),
        kind: r.try_get("kind").unwrap_or_default(),
        parent_job_id: nullable(r, "parent_job_id"),
        state: r.try_get("state").unwrap_or_default(),
        created_at_utc: r.try_get("created_at_utc").unwrap_or_default(),
        started_at_utc: nullable(r, "started_at_utc"),
        finished_at_utc: nullable(r, "finished_at_utc"),
        tz_offset: nullable(r, "tz_offset"),
        error: nullable(r, "error"),
        pid: nullable::<i64>(r, "pid"),
        progress_pct: nullable(r, "progress_pct"),
        progress_msg: nullable(r, "progress_msg"),
    }
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
    /// the queue returned 500 on a real root before it did — and must
    /// keep the rows: feedback is filed by a person and nothing
    /// regenerates it.
    #[tokio::test]
    async fn a_store_from_before_the_utc_columns_is_migrated_on_open() {
        let td = tempfile::tempdir().unwrap();
        // The old shape, by hand, in all three stores.
        {
            let jobs = open_pool(&crate::layout::jobs_db(td.path())).await.unwrap();
            sqlx::query(
                "CREATE TABLE sync_jobs (id VARCHAR(36) NOT NULL, source_ids VARCHAR(64), \
                 kind VARCHAR(16) NOT NULL, parent_job_id VARCHAR(36), state VARCHAR(16) NOT NULL, \
                 created_at VARCHAR(40) NOT NULL, started_at VARCHAR(40), finished_at VARCHAR(40), \
                 error TEXT, pid INT, progress_pct DOUBLE, progress_msg VARCHAR(512), \
                 PRIMARY KEY (id))",
            )
            .execute(&jobs)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO sync_jobs (id, kind, state, created_at, started_at, finished_at) \
                 VALUES ('job-1', 'all', 'done', '2026-09-10T10:00:00+02:00', \
                         '2026-09-10T10:00:05+02:00', '2026-09-10T10:01:00+02:00'), \
                        ('job-2', 'all', 'pending', '2026-09-11T09:00:00+02:00', NULL, NULL)",
            )
            .execute(&jobs)
            .await
            .unwrap();
            jobs.close().await;

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

        let jobs = store.list_jobs(false, 10).await.unwrap();
        assert_eq!(jobs.len(), 2, "{jobs:?}");
        let done = jobs.iter().find(|j| j.id == "job-1").unwrap();
        assert_eq!(done.created_at_utc, "2026-09-10T08:00:00.000000+00:00");
        assert_eq!(
            done.finished_at_utc.as_deref(),
            Some("2026-09-10T08:01:00.000000+00:00")
        );
        assert_eq!(done.tz_offset.as_deref(), Some("+02:00"));
        let pending = jobs.iter().find(|j| j.id == "job-2").unwrap();
        assert_eq!(pending.tz_offset.as_deref(), Some("+02:00"));
        // Asked in SQL rather than through the mapper, which reads a
        // NULL text column as `Some("")` on doltlite.
        let null_start: i64 =
            sqlx::query_scalar("SELECT started_at_utc IS NULL FROM sync_jobs WHERE id = 'job-2'")
                .fetch_one(&store.jobs_pool)
                .await
                .unwrap();
        assert_eq!(null_start, 1, "a stamp that was NULL stays NULL");

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

        // Opening again is a no-op: nothing to rename, nothing rewritten.
        drop(store);
        let again = AppStore::open(td.path()).await.unwrap();
        assert_eq!(again.list_jobs(false, 10).await.unwrap().len(), 2);
    }

    /// Each of the three stores says which build wrote it, with its own
    /// kind; feedback's rows are committed on the spot rather than left
    /// to ride into the next filed feedback, and a second open by the
    /// same build commits nothing more.
    #[tokio::test]
    async fn every_app_store_names_the_build_that_wrote_it() {
        let td = tempfile::tempdir().unwrap();
        let store = AppStore::open(td.path()).await.unwrap();
        for (pool, kind) in [
            (&store.feedback_pool, StoreKind::Feedback),
            (&store.jobs_pool, StoreKind::Jobs),
            (&store.usage_pool, StoreKind::Usage),
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
