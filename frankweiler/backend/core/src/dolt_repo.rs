//! `DoltRepo` — production [`MirrorRepo`](crate::repo::MirrorRepo) backed
//! by a `sqlx::MySqlPool` against the managed [`DoltServer`](crate::dolt_server::DoltServer).
//!
//! Today this serves grid + chat-preview reads. Writes
//! (`insert_feedback` + `CALL DOLT_COMMIT`) arrive in T12.
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
use crate::query::ParsedQuery;
use crate::repo::{MirrorRepo, RepoError};
use crate::search::SearchRow;

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
    /// [`crate::dolt_server::DoltServer::mysql_url`]).
    pub async fn connect(mysql_url: &str, root: Arc<PathBuf>) -> Result<Self, sqlx::Error> {
        let pool = MySqlPoolOptions::new()
            .max_connections(8)
            .connect(mysql_url)
            .await?;
        Ok(Self::from_pool(pool, root))
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
