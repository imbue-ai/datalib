//! `AppStore` — the three stores this server owns: filed feedback, the
//! sync job queue, and the bytes-on-disk timeseries, one doltlite file
//! each.
//!
//! doltlite is a SQLite fork: the C API and on-disk format are
//! libsqlite3-compatible, so we drop the `dolt sql-server` subprocess
//! and the TCP port. The audit-trail story stays — doltlite preserves
//! the `dolt_commit()` / `dolt_log()` SQL functions, invoked via
//! SQLite's scalar-function syntax (`SELECT dolt_commit(...)`) instead
//! of MySQL's `CALL DOLT_COMMIT(...)`.
//!
//! `insert_feedback` appends a row to the `feedback` table and stamps
//! `SELECT dolt_commit('-Am', ?)` so each piece of feedback gets its own
//! entry in `dolt_log`. The DDL is shipped by [`app_schema::feedback`];
//! `CREATE TABLE IF NOT EXISTS` keeps the init idempotent.

use crate::repo::{AppRepo, RepoError};
use crate::store::open_pool;
use app_schema::disk_usage::{DiskUsageRow, DDL as DISK_USAGE_DDL};
use app_schema::feedback::{FeedbackRow, DDL as FEEDBACK_DDL};
use app_schema::sync_jobs::{SyncJobRow, DDL as SYNC_JOBS_DDL};
use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// The three application stores: filed feedback, the sync job queue,
/// and the bytes-on-disk timeseries.
///
/// One type, three files, because they share a writer (this process)
/// but must not share a database. Doltlite's working set is per file and
/// shared across processes, so a `dolt_commit('-Am', …)` covers whatever
/// else is dirty in the same file — which is why these live apart from
/// the index the pipeline writes, and apart from each other.
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
    /// Open (or create) both stores for a data root and ensure their
    /// tables exist. DDL is `CREATE TABLE IF NOT EXISTS`, so populated
    /// files are untouched.
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
        store.init_feedback_table().await?;
        store.init_sync_jobs_table().await?;
        store.init_disk_usage_table().await?;
        Ok(store)
    }

    /// True when the linked libsqlite3 is doltlite and version-control
    /// SQL functions (`dolt_commit`, `dolt_log`, ...) are available.
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
            sqlx::query(ddl).execute(&self.feedback_pool).await?;
        }
        Ok(())
    }
    async fn init_sync_jobs_table(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in SYNC_JOBS_DDL {
            sqlx::query(ddl).execute(&self.jobs_pool).await?;
        }
        Ok(())
    }
    async fn init_disk_usage_table(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in DISK_USAGE_DDL {
            sqlx::query(ddl).execute(&self.usage_pool).await?;
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
             (feedback_uuid, created_at, sentiment, comment, app_version, git_hash, context_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.feedback_uuid)
        .bind(&row.created_at)
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
        let base = "SELECT id, source_name, kind, parent_job_id, state, created_at, \
                           started_at, finished_at, error, pid, progress_pct, progress_msg \
                    FROM sync_jobs";
        let sql = if only_active {
            format!(
                "{base} WHERE state IN ('pending','running') \
                 ORDER BY created_at DESC, id DESC LIMIT ?"
            )
        } else {
            format!("{base} ORDER BY created_at DESC, id DESC LIMIT ?")
        };
        let rows = sqlx::query(&sql)
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
        let sql = "SELECT id, source_name, kind, parent_job_id, state, created_at, \
                          started_at, finished_at, error, pid, progress_pct, progress_msg \
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
        kind: &str,
        source_name: Option<&str>,
    ) -> Result<SyncJobRow, RepoError> {
        let id = uuid::Uuid::new_v4().to_string();
        let created_at = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let row = SyncJobRow {
            id: id.clone(),
            source_name: source_name.map(|s| s.to_string()),
            kind: kind.to_string(),
            parent_job_id: None,
            state: "pending".to_string(),
            created_at: created_at.clone(),
            started_at: None,
            finished_at: None,
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
             (id, source_name, kind, parent_job_id, state, created_at, \
              started_at, finished_at, error, pid, progress_pct, progress_msg) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.source_name)
        .bind(&row.kind)
        .bind(&row.parent_job_id)
        .bind(&row.state)
        .bind(&row.created_at)
        .bind(&row.started_at)
        .bind(&row.finished_at)
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
        sqlx::query(
            "UPDATE sync_jobs SET state = 'canceled' \
             WHERE id = ? AND state IN ('pending', 'running')",
        )
        .bind(job_id)
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
            "SELECT id FROM sync_jobs WHERE state = 'pending' \
             ORDER BY created_at ASC, id ASC LIMIT 1",
        )
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("claim select: {e}")))?;
        let Some(id) = id else {
            return Ok(None);
        };
        let started_at = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        sqlx::query(
            "UPDATE sync_jobs SET state = 'running', started_at = ?, \
             progress_msg = 'starting…' WHERE id = ? AND state = 'pending'",
        )
        .bind(&started_at)
        .bind(&id)
        .execute(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("claim update: {e}")))?;
        // No DOLT_COMMIT — see the note in `enqueue_job`.
        // Re-read so the caller gets the row exactly as persisted.
        let sql = "SELECT id, source_name, kind, parent_job_id, state, created_at, \
                          started_at, finished_at, error, pid, progress_pct, progress_msg \
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
        state: &str,
        error: Option<&str>,
    ) -> Result<(), RepoError> {
        let mut conn = self
            .jobs_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        let finished_at = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        sqlx::query(
            "UPDATE sync_jobs SET state = ?, finished_at = ?, error = ?, pid = NULL \
             WHERE id = ?",
        )
        .bind(state)
        .bind(&finished_at)
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
    async fn recover_running_jobs(&self) -> Result<usize, RepoError> {
        let mut conn = self
            .jobs_pool
            .acquire()
            .await
            .map_err(|e| RepoError::Internal(format!("acquire: {e}")))?;
        let finished_at = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let res = sqlx::query(
            "UPDATE sync_jobs SET state = 'failed', finished_at = ?, pid = NULL, \
             error = 'interrupted: backend restarted while job was running' \
             WHERE state = 'running'",
        )
        .bind(&finished_at)
        .execute(&mut *conn)
        .await
        .map_err(|e| RepoError::Internal(format!("recover running: {e}")))?;
        // No DOLT_COMMIT — see the note in `enqueue_job`.
        let n = res.rows_affected() as usize;
        Ok(n)
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
            // (path, measured_at) and the sampler stamps one instant per
            // walk, so a second walk finishing inside the same
            // whole-microsecond would otherwise fail the whole batch
            // over a duplicate that carries the same number anyway.
            sqlx::query(
                "INSERT OR REPLACE INTO disk_usage (path, measured_at, bytes) VALUES (?, ?, ?)",
            )
            .bind(&row.path)
            .bind(&row.measured_at)
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
            "SELECT path, measured_at, bytes FROM disk_usage \
             ORDER BY measured_at DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.usage_pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        Ok(rows
            .iter()
            .map(|r| DiskUsageRow {
                path: r.try_get("path").unwrap_or_default(),
                measured_at: r.try_get("measured_at").unwrap_or_default(),
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
    SyncJobRow {
        id: r.try_get("id").unwrap_or_default(),
        source_name: r.try_get("source_name").ok(),
        kind: r.try_get("kind").unwrap_or_default(),
        parent_job_id: r.try_get("parent_job_id").ok(),
        state: r.try_get("state").unwrap_or_default(),
        created_at: r.try_get("created_at").unwrap_or_default(),
        started_at: r.try_get("started_at").ok(),
        finished_at: r.try_get("finished_at").ok(),
        error: r.try_get("error").ok(),
        pid: r.try_get::<i64, _>("pid").ok(),
        progress_pct: r.try_get("progress_pct").ok(),
        progress_msg: r.try_get("progress_msg").ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_schema::disk_usage::ROOT_PATH;

    fn sample(path: &str, at: &str, bytes: i64) -> DiskUsageRow {
        DiskUsageRow {
            path: path.to_string(),
            measured_at: at.to_string(),
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
        assert_eq!(back[0].measured_at, "2026-09-02T10:00:05-07:00");
        let root: Vec<i64> = back
            .iter()
            .filter(|r| r.path == ROOT_PATH)
            .map(|r| r.bytes)
            .collect();
        assert_eq!(root, vec![180, 100]);
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
