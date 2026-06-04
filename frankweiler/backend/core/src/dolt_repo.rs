//! `DoltRepo` — production [`MirrorRepo`](crate::repo::MirrorRepo) backed
//! by a `sqlx::SqlitePool` against a single doltlite file on disk.
//!
//! doltlite is a SQLite fork: the C API and on-disk format are
//! libsqlite3-compatible, so we drop the `dolt sql-server` subprocess
//! and the TCP port. The audit-trail story stays — doltlite preserves
//! the `dolt_commit()` / `dolt_log()` SQL functions, just invoked via
//! SQLite's scalar-function syntax (`SELECT dolt_commit(...)`) instead
//! of MySQL's `CALL DOLT_COMMIT(...)`.
//!
//! Reads: grid search + chat-preview metadata. Writes:
//! [`Self::insert_feedback`] appends a row to the `feedback` table and
//! stamps `SELECT dolt_commit('-Am', ?)` so each piece of feedback gets
//! its own entry in `dolt_log` (lazy man's audit trail). The DDL is
//! shipped by [`frankweiler_schema::feedback`]; `CREATE TABLE IF NOT
//! EXISTS` keeps the init idempotent across restarts.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::db::{build_where, snippet, ChatMeta};
use crate::qmd::GridRowRef;
use crate::query::ParsedQuery;
use crate::repo::{MirrorRepo, RepoError};
use crate::search::SearchRow;
use frankweiler_schema::feedback::{FeedbackRow, DDL as FEEDBACK_DDL};
use frankweiler_schema::sync_jobs::{SyncJobRow, DDL as SYNC_JOBS_DDL};

/// SQLite/doltlite-backed implementation of [`MirrorRepo`].
///
/// `root` is the data root (e.g. `~/Documents/mixed-up-files`) — needed
/// because `qmd_path` in `grid_rows` is stored relative to the root and
/// the trait contract returns an absolute path.
pub struct DoltRepo {
    pool: SqlitePool,
    root: Arc<PathBuf>,
    /// Whether the linked libsqlite3 is doltlite (exposes `dolt_commit`).
    /// Probed once at connect time via `pragma_function_list`. When
    /// false, every `commit_version` call is a no-op — the row still
    /// lands, you just don't get the dolt_log audit entry. This keeps
    /// CI hosts without doltlite installed runnable; production hosts
    /// should always have doltlite linked.
    has_dolt: bool,
}

impl DoltRepo {
    /// Wrap an existing pool. Probes for doltlite extensions; the caller
    /// can also use [`from_pool_with_dolt`](Self::from_pool_with_dolt)
    /// to skip the probe in tests that know the answer.
    pub async fn from_pool(pool: SqlitePool, root: Arc<PathBuf>) -> Self {
        let has_dolt = probe_dolt_extensions(&pool).await;
        Self {
            pool,
            root,
            has_dolt,
        }
    }

    pub fn from_pool_with_dolt(pool: SqlitePool, root: Arc<PathBuf>, has_dolt: bool) -> Self {
        Self {
            pool,
            root,
            has_dolt,
        }
    }

    /// True when the linked libsqlite3 is doltlite and version-control
    /// SQL functions (`dolt_commit`, `dolt_log`, ...) are available.
    pub fn has_dolt_extensions(&self) -> bool {
        self.has_dolt
    }

    /// Stamp a doltlite version commit covering the working set on this
    /// connection. No-op when the linked libsqlite3 is stock SQLite.
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

    /// Open (or create) a doltlite file at `db_path` and ensure the
    /// `feedback` + `sync_jobs` tables exist. DDL is `CREATE TABLE IF NOT
    /// EXISTS`, so a real ingest-populated file is left untouched.
    pub async fn open(db_path: &std::path::Path, root: Arc<PathBuf>) -> Result<Self, sqlx::Error> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
            .create_if_missing(true)
            // WAL / NORMAL synchronous are no-ops on doltlite (its
            // chunk store ignores the SQLite pager journal), but
            // harmless to leave as documentation of intent for
            // stock-libsqlite3 builds (e.g. cargo-only unit tests).
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        // Pool size 1: doltlite's per-connection HEAD pointer means
        // pool sizes >1 produce silent dolt_log dropouts and
        // `commit conflict` errors on interleaved writes. See
        // `frankweiler_etl::doltlite_raw` module docs for the full
        // story (dolt-team-confirmed advice).
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let repo = Self::from_pool(pool, root).await;
        repo.init_feedback_table().await?;
        repo.init_sync_jobs_table().await?;
        Ok(repo)
    }

    async fn init_feedback_table(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in FEEDBACK_DDL {
            sqlx::query(ddl).execute(&self.pool).await?;
        }
        Ok(())
    }

    async fn init_sync_jobs_table(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in SYNC_JOBS_DDL {
            sqlx::query(ddl).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl MirrorRepo for DoltRepo {
    async fn search(&self, q: &ParsedQuery, limit: usize) -> Result<Vec<SearchRow>, RepoError> {
        let needle = q.free_text.to_lowercase();
        let (where_sql, params) = build_where(q, &needle);
        let sql = format!(
            "SELECT uuid, provider, kind, source_label, when_ts, author, account, project, \
                    org_uuid, org_name, channel, conversation_name, conversation_uuid, markdown_uuid, \
                    message_index, entire_chat, text, slack_link, notion_page_uuid \
             FROM grid_rows{} \
             ORDER BY when_ts ASC, CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END, uuid \
             LIMIT ?",
            where_sql
        );

        let mut query = sqlx::query(&sql);
        for p in &params {
            query = query.bind(p);
        }
        query = query.bind(limit as i64);

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))?;

        let mut out: Vec<SearchRow> = Vec::with_capacity(rows.len());
        for r in rows {
            let uuid: String = r.try_get("uuid").unwrap_or_default();
            let kind: String = r.try_get("kind").unwrap_or_default();
            let source_label: String = r.try_get("source_label").unwrap_or_default();
            let when_ts: String = r.try_get("when_ts").unwrap_or_default();
            let author: String = r.try_get("author").unwrap_or_default();
            let account: String = r.try_get("account").unwrap_or_default();
            let project: String = r.try_get("project").unwrap_or_default();
            let org_uuid: String = r.try_get("org_uuid").unwrap_or_default();
            let org_name: String = r.try_get("org_name").unwrap_or_default();
            let channel: String = r.try_get("channel").unwrap_or_default();
            let conversation_name: String = r.try_get("conversation_name").unwrap_or_default();
            let conversation_uuid: String = r.try_get("conversation_uuid").unwrap_or_default();
            let markdown_uuid: Option<String> = r.try_get("markdown_uuid").ok();
            let message_index: Option<i64> = r.try_get("message_index").ok();
            let entire_chat: String = r.try_get("entire_chat").unwrap_or_default();
            let text: String = r.try_get("text").unwrap_or_default();
            let slack_link: String = r.try_get("slack_link").unwrap_or_default();
            let notion_page_uuid: String = r.try_get("notion_page_uuid").unwrap_or_default();

            let snip = if kind == "Chat" {
                text.clone()
            } else {
                snippet(&text, &needle)
            };
            out.push(SearchRow {
                uuid,
                conversation_uuid,
                markdown_uuid,
                message_index: message_index.map(|n| n as usize),
                snippet: snip,
                sender: author.clone(),
                when: when_ts,
                conversation_name,
                project,
                account,
                org_uuid,
                org_name,
                entire_chat,
                source: source_label,
                kind,
                author,
                channel,
                slack_link,
                notion_page_uuid,
                score: None,
            });
        }
        Ok(out)
    }

    async fn chat_meta(&self, markdown_uuid: &str) -> Result<Option<ChatMeta>, RepoError> {
        // Project the per-markdown header fields out of any grid_row
        // that points at this markdown — they're denormalized identically
        // across the rows of a single markdown, so picking the canonical
        // (Chat / Slack Thread / per-provider top-level row) keeps the
        // result deterministic.
        let sql = "SELECT conversation_name, account, project, channel, when_ts, source_label, \
                          COALESCE(source_url, slack_link) AS source_url_or_link \
                   FROM grid_rows \
                   WHERE markdown_uuid = ? \
                   ORDER BY CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END \
                   LIMIT 1";
        let row = sqlx::query(sql)
            .bind(markdown_uuid)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        let Some(r) = row else { return Ok(None) };
        Ok(Some(ChatMeta {
            name: r.try_get("conversation_name").ok(),
            account: r.try_get("account").ok(),
            project: r.try_get("project").ok(),
            channel: r.try_get("channel").ok(),
            when_ts: r.try_get("when_ts").ok(),
            source_label: r.try_get("source_label").ok(),
            source_url: r.try_get("source_url_or_link").ok(),
        }))
    }

    async fn insert_feedback(&self, row: FeedbackRow) -> Result<(), RepoError> {
        // The INSERT and the `dolt_commit` ride the same connection so
        // the commit covers exactly the row we just wrote, with no
        // chance of a concurrent writer's INSERT slipping into the same
        // dolt_log entry. (The pool may hand a different connection to
        // a sibling task, which is fine — doltlite's working set is
        // per-file, not per-connection.)
        let mut conn = self
            .pool
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

    async fn grid_row_refs(&self) -> Result<Vec<GridRowRef>, RepoError> {
        let rows = sqlx::query(
            "SELECT uuid, kind, COALESCE(qmd_path, '') AS qmd_path, provider FROM grid_rows",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        let mut out: Vec<GridRowRef> = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(GridRowRef {
                uuid: r.try_get("uuid").unwrap_or_default(),
                kind: r.try_get("kind").unwrap_or_default(),
                qmd_path: r.try_get("qmd_path").unwrap_or_default(),
                provider: r.try_get("provider").unwrap_or_default(),
            });
        }
        Ok(out)
    }

    async fn search_by_uuids(
        &self,
        q: &ParsedQuery,
        uuids: &[String],
        limit: usize,
    ) -> Result<Vec<SearchRow>, RepoError> {
        if uuids.is_empty() {
            return Ok(Vec::new());
        }
        let (mut where_sql, mut params) = build_where(q, "");
        let take = uuids.len().min(limit);
        let placeholders = std::iter::repeat_n("?", take).collect::<Vec<_>>().join(",");
        if where_sql.is_empty() {
            where_sql = format!(" WHERE uuid IN ({placeholders})");
        } else {
            where_sql.push_str(&format!(" AND uuid IN ({placeholders})"));
        }
        for u in uuids.iter().take(take) {
            params.push(u.clone());
        }
        let sql = format!(
            "SELECT uuid, provider, kind, source_label, when_ts, author, account, project, \
                    org_uuid, org_name, channel, conversation_name, conversation_uuid, markdown_uuid, \
                    message_index, entire_chat, text, slack_link, notion_page_uuid \
             FROM grid_rows{}",
            where_sql
        );
        let mut query = sqlx::query(&sql);
        for p in &params {
            query = query.bind(p);
        }
        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        let mut by_uuid: std::collections::HashMap<String, SearchRow> =
            std::collections::HashMap::new();
        for r in rows {
            let uuid: String = r.try_get("uuid").unwrap_or_default();
            let kind: String = r.try_get("kind").unwrap_or_default();
            let source_label: String = r.try_get("source_label").unwrap_or_default();
            let when_ts: String = r.try_get("when_ts").unwrap_or_default();
            let author: String = r.try_get("author").unwrap_or_default();
            let account: String = r.try_get("account").unwrap_or_default();
            let project: String = r.try_get("project").unwrap_or_default();
            let org_uuid: String = r.try_get("org_uuid").unwrap_or_default();
            let org_name: String = r.try_get("org_name").unwrap_or_default();
            let channel: String = r.try_get("channel").unwrap_or_default();
            let conversation_name: String = r.try_get("conversation_name").unwrap_or_default();
            let conversation_uuid: String = r.try_get("conversation_uuid").unwrap_or_default();
            let markdown_uuid: Option<String> = r.try_get("markdown_uuid").ok();
            let message_index: Option<i64> = r.try_get("message_index").ok();
            let entire_chat: String = r.try_get("entire_chat").unwrap_or_default();
            let text: String = r.try_get("text").unwrap_or_default();
            let slack_link: String = r.try_get("slack_link").unwrap_or_default();
            let notion_page_uuid: String = r.try_get("notion_page_uuid").unwrap_or_default();
            let snip = if kind == "Chat" {
                text.clone()
            } else {
                snippet(&text, "")
            };
            by_uuid.insert(
                uuid.clone(),
                SearchRow {
                    uuid,
                    conversation_uuid,
                    markdown_uuid,
                    message_index: message_index.map(|n| n as usize),
                    snippet: snip,
                    sender: author.clone(),
                    when: when_ts,
                    conversation_name,
                    project,
                    account,
                    org_uuid,
                    org_name,
                    entire_chat,
                    source: source_label,
                    kind,
                    author,
                    channel,
                    slack_link,
                    notion_page_uuid,
                    score: None,
                },
            );
        }
        let mut out: Vec<SearchRow> = Vec::with_capacity(by_uuid.len());
        for u in uuids.iter().take(take) {
            if let Some(r) = by_uuid.remove(u) {
                out.push(r);
            }
        }
        Ok(out)
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
            .fetch_all(&self.pool)
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
            .fetch_optional(&self.pool)
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
        let created_at = chrono::Local::now().to_rfc3339();
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
            .pool
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
        let msg = format!("sync_job: {} pending", row.id);
        self.commit_version(&mut conn, &msg).await?;
        Ok(row)
    }

    async fn request_cancel_job(&self, job_id: &str) -> Result<(), RepoError> {
        let mut conn = self
            .pool
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
        let msg = format!("sync_job: {job_id} cancel-requested");
        self.commit_version(&mut conn, &msg).await?;
        Ok(())
    }

    async fn qmd_path_for_markdown(
        &self,
        markdown_uuid: &str,
    ) -> Result<Option<PathBuf>, RepoError> {
        let row = sqlx::query(
            "SELECT md_path FROM markdowns WHERE markdown_uuid = ? AND md_path IS NOT NULL LIMIT 1",
        )
        .bind(markdown_uuid)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))?;
        let Some(r) = row else { return Ok(None) };
        let rel: Option<String> = r.try_get("md_path").ok();
        Ok(rel.map(|p| self.root.as_ref().join(p)))
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
