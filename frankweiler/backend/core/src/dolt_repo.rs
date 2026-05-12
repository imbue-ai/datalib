//! `DoltRepo` — production [`MirrorRepo`](crate::repo::MirrorRepo) backed
//! by a `sqlx::MySqlPool` against the managed [`DoltServer`](crate::dolt_server::DoltServer).
//!
//! Reads: grid search + chat-preview metadata. Writes: [`Self::insert_feedback`]
//! appends a row to the `feedback` table and calls `DOLT_COMMIT` so each
//! piece of feedback becomes its own entry in `dolt log` (lazy man's audit
//! trail). The `feedback` table itself is created on connect via the DDL
//! shipped by [`frankweiler_schema::feedback`]; the `CREATE TABLE IF NOT
//! EXISTS` makes the init idempotent across restarts.
//!
//! Dialect notes: SQL strings use `?` placeholders, which works for both
//! MySQL (sqlx bind index) and the legacy SQLite path. We reuse
//! [`crate::db::build_where`] verbatim. `grid_rows` is written into Dolt
//! by `src/ingest/sql_writers.py` with the same column shape ingest
//! materializes to SQLite, so reads are schema-compatible without
//! per-backend branches.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::Row;

use crate::db::{build_where, snippet, ChatMeta};
use crate::qmd::GridRowRef;
use crate::query::ParsedQuery;
use crate::repo::{MirrorRepo, RepoError};
use crate::search::SearchRow;
use frankweiler_schema::feedback::{FeedbackRow, DDL as FEEDBACK_DDL};

/// MySQL/Dolt-backed implementation of [`MirrorRepo`].
///
/// `root` is the data root (e.g. `~/Documents/personal-mirror`) — needed
/// because `qmd_path` in `grid_rows` is stored relative to the root and
/// the trait contract returns an absolute path.
pub struct DoltRepo {
    pool: MySqlPool,
    root: Arc<PathBuf>,
}

impl DoltRepo {
    /// Wrap an existing pool. Tests / callers that manage their own pool
    /// use this.
    pub fn from_pool(pool: MySqlPool, root: Arc<PathBuf>) -> Self {
        Self { pool, root }
    }

    /// Connect to the Dolt MySQL endpoint at `mysql_url` (typically from
    /// [`crate::dolt_server::DoltServer::mysql_url`]) and ensure the
    /// `feedback` table exists. The DDL is `CREATE TABLE IF NOT EXISTS`,
    /// so a real ingest-populated repo is left untouched.
    pub async fn connect(mysql_url: &str, root: Arc<PathBuf>) -> Result<Self, sqlx::Error> {
        let pool = MySqlPoolOptions::new()
            .max_connections(8)
            .connect(mysql_url)
            .await?;
        let repo = Self::from_pool(pool, root);
        repo.init_feedback_table().await?;
        Ok(repo)
    }

    /// Apply the feedback DDL. Idempotent — `CREATE TABLE IF NOT EXISTS`
    /// is a no-op when the table already exists. Dolt auto-commits DDL
    /// even with `--no-auto-commit`, so no DOLT_COMMIT is needed here.
    async fn init_feedback_table(&self) -> Result<(), sqlx::Error> {
        for (_table, ddl) in FEEDBACK_DDL {
            sqlx::query(ddl).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub fn pool(&self) -> &MySqlPool {
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
                    channel, conversation_name, conversation_uuid, message_index, \
                    entire_chat, text, slack_link, notion_page_uuid \
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
            let channel: String = r.try_get("channel").unwrap_or_default();
            let conversation_name: String = r.try_get("conversation_name").unwrap_or_default();
            let conversation_uuid: String = r.try_get("conversation_uuid").unwrap_or_default();
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
                message_index: message_index.map(|n| n as usize),
                snippet: snip,
                sender: author.clone(),
                when: when_ts,
                conversation_name,
                project,
                account,
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

    async fn chat_meta(&self, conversation_uuid: &str) -> Result<Option<ChatMeta>, RepoError> {
        let sql = "SELECT conversation_name, account, project, channel, when_ts, source_label, \
                          COALESCE(source_url, slack_link) AS source_url_or_link \
                   FROM grid_rows \
                   WHERE conversation_uuid = ? \
                   ORDER BY CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END \
                   LIMIT 1";
        let row = sqlx::query(sql)
            .bind(conversation_uuid)
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
        // Pin INSERT + DOLT_COMMIT to one connection: Dolt's working set
        // is session-scoped under `--no-auto-commit`, so a SELECT from a
        // different connection in the pool wouldn't see the uncommitted
        // row. DOLT_COMMIT here publishes the row across every connection
        // *and* stamps a `dolt log` entry — that audit trail is the whole
        // point of routing feedback through Dolt instead of SQLite.
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
        sqlx::query("CALL DOLT_COMMIT('-Am', ?)")
            .bind(msg)
            .execute(&mut *conn)
            .await
            .map_err(|e| RepoError::Internal(format!("dolt_commit: {e}")))?;
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
                    channel, conversation_name, conversation_uuid, message_index, \
                    entire_chat, text, slack_link, notion_page_uuid \
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
            let channel: String = r.try_get("channel").unwrap_or_default();
            let conversation_name: String = r.try_get("conversation_name").unwrap_or_default();
            let conversation_uuid: String = r.try_get("conversation_uuid").unwrap_or_default();
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
                    message_index: message_index.map(|n| n as usize),
                    snippet: snip,
                    sender: author.clone(),
                    when: when_ts,
                    conversation_name,
                    project,
                    account,
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

    async fn qmd_path_for_conversation(
        &self,
        conversation_uuid: &str,
    ) -> Result<Option<PathBuf>, RepoError> {
        let sql = "SELECT qmd_path FROM grid_rows \
                   WHERE conversation_uuid = ? AND qmd_path IS NOT NULL \
                   ORDER BY CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END \
                   LIMIT 1";
        let row = sqlx::query(sql)
            .bind(conversation_uuid)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        let Some(r) = row else { return Ok(None) };
        let rel: Option<String> = r.try_get("qmd_path").ok();
        Ok(rel.map(|p| self.root.as_ref().join(p)))
    }
}
