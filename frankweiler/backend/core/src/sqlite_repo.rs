//! `SqliteRepo` — read-only [`MirrorRepo`](crate::repo::MirrorRepo) backed
//! by `sqlx::SqlitePool` against `<root>/mirror.sqlite`.
//!
//! Reference / backwards-compat path. The production app uses
//! [`crate::dolt_repo::DoltRepo`]; this exists so we can still read the
//! periodically-materialized SQLite mirror for debugging and side-by-side
//! comparison. Writes (`insert_feedback`) are not supported and surface
//! as [`RepoError::ReadOnly`] when added in T12.
//!
//! The SQL is identical to DoltRepo's — both speak `?` placeholders and
//! both target the `grid_rows` projection in the same shape, so we
//! share [`crate::db::build_where`] / [`crate::db::snippet`].

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::db::{build_where, snippet, ChatMeta};
use crate::qmd::GridRowRef;
use crate::query::ParsedQuery;
use crate::repo::{MirrorRepo, RepoError};
use crate::search::SearchRow;

pub struct SqliteRepo {
    pool: SqlitePool,
    root: Arc<PathBuf>,
}

impl SqliteRepo {
    pub fn from_pool(pool: SqlitePool, root: Arc<PathBuf>) -> Self {
        Self { pool, root }
    }

    /// Open `<root>/mirror.sqlite` read-only. Missing files are not an
    /// error — the resulting pool will simply yield zero rows, matching
    /// the legacy rusqlite path's behavior.
    pub async fn open(root: Arc<PathBuf>) -> Result<Self, sqlx::Error> {
        let db_path = root.as_ref().join("mirror.sqlite");
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .read_only(true)
            .create_if_missing(false);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        Ok(Self::from_pool(pool, root))
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl MirrorRepo for SqliteRepo {
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
        // Free-text portion already consumed by qmd; build the structured
        // WHERE only.
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

#[cfg(test)]
mod tests {
    //! Parity-with-rusqlite tests. The fixture matches the now-removed
    //! `db::tests` fixture row-for-row; the only difference is execution
    //! via `sqlx::SqlitePool` instead of `rusqlite::Connection`.

    use super::*;
    use crate::query::parse_query;
    use frankweiler_schema::grid_rows::DDL as GRID_DDL;
    use sqlx::sqlite::SqliteConnectOptions;
    use std::str::FromStr;

    async fn build_fixture_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1) // in-memory DB: must reuse the single conn
            .connect_with(opts)
            .await
            .unwrap();
        for (_table, ddl) in GRID_DDL {
            sqlx::query(ddl).execute(&pool).await.unwrap();
        }
        // 5 anthropic chats × (1 chat row + 4 message rows).
        for i in 0..5u32 {
            let cuuid = format!("a-conv-{i:02}");
            let when = format!("2026-04-{:02}T10:00:00+00:00", i + 1);
            sqlx::query(
                "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
                 author, account, project, channel, conversation_name, conversation_uuid, \
                 message_index, entire_chat, text, slack_link) \
                 VALUES (?, 'anthropic', 'Chat', 'Claude', ?, NULL, 'acct-a', NULL, NULL, ?, ?, NULL, ?, ?, NULL)",
            )
            .bind(&cuuid)
            .bind(&when)
            .bind(format!("Anthropic chat {i}"))
            .bind(&cuuid)
            .bind(format!("/chat/{cuuid}"))
            .bind(format!("summary {i}"))
            .execute(&pool)
            .await
            .unwrap();
            for j in 0..4u32 {
                let mwhen = format!("2026-04-{:02}T10:0{}:00+00:00", i + 1, j);
                let kind = if j % 2 == 0 {
                    "User Input"
                } else {
                    "LLM Response"
                };
                let author = if kind == "User Input" {
                    "acct-a"
                } else {
                    "claude-opus-4-7"
                };
                sqlx::query(
                    "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
                     author, account, project, channel, conversation_name, conversation_uuid, \
                     message_index, entire_chat, text, slack_link) \
                     VALUES (?, 'anthropic', ?, 'Claude', ?, ?, 'acct-a', NULL, NULL, ?, ?, ?, ?, ?, NULL)",
                )
                .bind(format!("a-msg-{i:02}-{j:02}"))
                .bind(kind)
                .bind(&mwhen)
                .bind(author)
                .bind(format!("Anthropic chat {i}"))
                .bind(&cuuid)
                .bind(j as i64)
                .bind(format!("/chat/{cuuid}"))
                .bind(format!("anthropic msg {i}-{j} body text"))
                .execute(&pool)
                .await
                .unwrap();
            }
        }
        // One March-2025 ChatGPT conversation with 4 messages.
        sqlx::query(
            "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
             author, account, project, channel, conversation_name, conversation_uuid, \
             message_index, entire_chat, text, slack_link) \
             VALUES ('o-conv-01', 'openai', 'Chat', 'ChatGPT', '2025-03-31T22:02:49+00:00', \
                     NULL, 'acct-o', NULL, NULL, 'Defaults in Program Design', 'o-conv-01', \
                     NULL, '/chat/o-conv-01', 'Defaults in Program Design', NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (j, role) in ["user", "assistant", "user", "assistant"]
            .iter()
            .enumerate()
        {
            let kind = if *role == "user" {
                "User Input"
            } else {
                "LLM Response"
            };
            let author = if kind == "User Input" {
                "acct-o"
            } else {
                "gpt-5"
            };
            sqlx::query(
                "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
                 author, account, project, channel, conversation_name, conversation_uuid, \
                 message_index, entire_chat, text, slack_link) \
                 VALUES (?, 'openai', ?, 'ChatGPT', ?, ?, 'acct-o', NULL, NULL, \
                         'Defaults in Program Design', 'o-conv-01', ?, '/chat/o-conv-01', ?, NULL)",
            )
            .bind(format!("o-msg-{j:02}"))
            .bind(kind)
            .bind(format!("2025-03-31T22:02:5{j}+00:00"))
            .bind(author)
            .bind(j as i64)
            .bind(format!("openai msg {j} body text"))
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    fn fixture_repo(pool: SqlitePool) -> SqliteRepo {
        SqliteRepo::from_pool(pool, Arc::new(PathBuf::from("/tmp/fw-test-root")))
    }

    #[tokio::test]
    async fn rows_sorted_by_time_ascending_with_chat_before_its_messages() {
        let repo = fixture_repo(build_fixture_pool().await);
        let rows = repo.search(&parse_query(""), 1000).await.unwrap();

        let openai_msg_count = rows
            .iter()
            .filter(|r| r.source == "ChatGPT" && r.kind != "Chat")
            .count();
        assert_eq!(openai_msg_count, 4);

        let times: Vec<&str> = rows.iter().map(|r| r.when.as_str()).collect();
        for w in times.windows(2) {
            assert!(
                w[0] <= w[1],
                "rows not sorted ascending by when: {:?}",
                times
            );
        }

        let first_chatgpt_chat = rows
            .iter()
            .position(|r| r.source == "ChatGPT" && r.kind == "Chat")
            .expect("no ChatGPT Chat row found");
        let first_chatgpt_msg = rows
            .iter()
            .position(|r| r.source == "ChatGPT" && r.kind != "Chat")
            .expect("no ChatGPT message rows found");
        assert!(first_chatgpt_chat < first_chatgpt_msg);
    }

    #[tokio::test]
    async fn source_filter_keeps_only_matching_rows() {
        let repo = fixture_repo(build_fixture_pool().await);
        let rows = repo
            .search(&parse_query("source:Claude type:all"), 1000)
            .await
            .unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.source == "Claude"));
    }

    #[tokio::test]
    async fn negated_source_excludes_matching_rows() {
        let repo = fixture_repo(build_fixture_pool().await);
        let rows = repo
            .search(&parse_query("-source:ChatGPT type:all"), 1000)
            .await
            .unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.source != "ChatGPT"));
    }

    #[tokio::test]
    async fn same_field_repeated_with_different_values_is_empty() {
        let repo = fixture_repo(build_fixture_pool().await);
        let rows = repo
            .search(&parse_query("source:Claude source:ChatGPT type:all"), 1000)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn negated_filter_keeps_null_values() {
        let repo = fixture_repo(build_fixture_pool().await);
        let baseline = repo
            .search(&parse_query("type:all"), 1000)
            .await
            .unwrap()
            .len();
        let filtered = repo
            .search(&parse_query("-channel:announce type:all"), 1000)
            .await
            .unwrap()
            .len();
        assert_eq!(baseline, filtered);
    }
}
