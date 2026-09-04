//! `DoltRepo` — production [`IndexRepo`](crate::repo::IndexRepo) backed
//! by a `sqlx::SqlitePool` against the grid index on disk.
//!
//! Reads only: the `grid_index` step is the file's only writer, which is
//! what lets a reader hold it open while a sync rewrites it. The pool is
//! still pinned to one connection — see
//! [`datalib_core::store::open_pool`] for why that is unrelated.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::db::{build_where, snippet, ChatMeta};
use crate::qmd::GridRowRef;
use crate::query::ParsedQuery;
use crate::repo::{DocRow, EdgeRowOut, IndexRepo};
use crate::search::SearchRow;
use datalib_core::repo::RepoError;
use datalib_core::store::{is_missing_table, open_pool};
use datalib_schema::edges::EdgeRow;

/// SQLite/doltlite-backed implementation of [`IndexRepo`].
///
/// `root` is the data root (e.g. `~/Documents/datalib`) — needed
/// because `qmd_path` in `grid_rows` is stored relative to the root and
/// the trait contract returns an absolute path.
pub struct DoltRepo {
    /// The grid index: `grid_rows`, `markdowns`, `edges`. Read-only from
    /// here — the `grid_index` step is its only writer.
    pool: SqlitePool,
    root: Arc<PathBuf>,
}

/// The `grid_rows` columns every [`SearchRow`] is built from. One
/// constant because `search` and `search_by_uuids` select exactly the
/// same set through [`search_row_from`]; two hand-kept lists drifted for
/// as long as they existed.
const SEARCH_ROW_COLUMNS: &str = "uuid, provider, kind, source_label, when_ts, author, account, \
     project, org_uuid, org_name, channel, conversation_name, conversation_uuid, markdown_uuid, \
     message_index, entire_chat, text, slack_link, source_url, notion_page_uuid, upstream_id, \
     upstream_entity_kind, qmd_path";

/// Build a [`SearchRow`] from one `grid_rows` row selected with
/// [`SEARCH_ROW_COLUMNS`]. `needle` is the free-text term the snippet is
/// centered on; pass `""` for a query that has none.
///
/// sqlx-sqlite has a load-bearing gotcha: `try_get::<T>` for a SQL NULL
/// column does NOT return Err — it silently returns `T::default()` (0
/// for i64, "" for String). That means `try_get(…).ok()` with an
/// `Option<T>` LHS gives `Some(0)` / `Some("")` for NULL, NOT `None`. To
/// distinguish NULL from an actual default value, the type passed to
/// `try_get` must itself be `Option<T>`. Pattern:
/// `try_get::<Option<T>, _>(…).ok().flatten()`. See
/// `tests/fixture_db_snapshot.rs` for the canonical example.
fn search_row_from(r: &sqlx::sqlite::SqliteRow, needle: &str) -> SearchRow {
    let kind: String = r.try_get("kind").unwrap_or_default();
    let author: String = r.try_get("author").unwrap_or_default();
    let text: String = r.try_get("text").unwrap_or_default();
    let qmd_path: String = r.try_get("qmd_path").unwrap_or_default();
    SearchRow {
        uuid: r.try_get("uuid").unwrap_or_default(),
        conversation_uuid: r.try_get("conversation_uuid").unwrap_or_default(),
        markdown_uuid: r
            .try_get::<Option<String>, _>("markdown_uuid")
            .ok()
            .flatten(),
        message_index: r
            .try_get::<Option<i64>, _>("message_index")
            .ok()
            .flatten()
            .map(|n| n as usize),
        snippet: if kind == "Chat" {
            text.clone()
        } else {
            snippet(&text, needle)
        },
        sender: author.clone(),
        when: r.try_get::<Option<String>, _>("when_ts").ok().flatten(),
        conversation_name: r.try_get("conversation_name").unwrap_or_default(),
        project: r.try_get("project").unwrap_or_default(),
        account: r.try_get("account").unwrap_or_default(),
        org_uuid: r.try_get("org_uuid").unwrap_or_default(),
        org_name: r.try_get("org_name").unwrap_or_default(),
        entire_chat: r.try_get("entire_chat").unwrap_or_default(),
        source: r.try_get("source_label").unwrap_or_default(),
        source_name: source_name_from_qmd_path(&qmd_path),
        kind,
        author,
        channel: r.try_get("channel").unwrap_or_default(),
        slack_link: r.try_get("slack_link").unwrap_or_default(),
        source_url: r.try_get("source_url").unwrap_or_default(),
        notion_page_uuid: r.try_get("notion_page_uuid").unwrap_or_default(),
        upstream_id: r.try_get("upstream_id").unwrap_or_default(),
        upstream_entity_kind: r.try_get("upstream_entity_kind").unwrap_or_default(),
        score: None,
    }
}

/// The configured source a rendered document belongs to: the first
/// segment of its data-root-relative path (`slack/rendered_md/x/all.md`
/// → `slack`). This is the same derivation `datalib-step` uses to name
/// a source from its declared outputs, and the same one `grid_index`
/// uses when it walks one directory per stanza — the stanza directory
/// name *is* the config-level name.
///
/// Empty for a path with no separator, which would mean a renderer wrote
/// outside its own tree.
fn source_name_from_qmd_path(qmd_path: &str) -> String {
    match qmd_path.split_once('/') {
        Some((first, _)) => first.to_string(),
        None => String::new(),
    }
}

impl DoltRepo {
    /// Wrap an existing index pool.
    pub fn from_pool(pool: SqlitePool, root: Arc<PathBuf>) -> Self {
        Self { pool, root }
    }

    /// Open the grid index for this data root, read-only in practice:
    /// the `grid_index` step is its only writer, and any number of
    /// readers may hold it open at once.
    pub async fn open(root: Arc<PathBuf>) -> Result<Self, sqlx::Error> {
        let pool = open_pool(&datalib_core::layout::grid_index_db(&root)).await?;
        Ok(Self::from_pool(pool, root))
    }

    /// The grid-index pool, for a test that wants to seed rows.
    pub fn index_pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl IndexRepo for DoltRepo {
    async fn search(&self, q: &ParsedQuery, limit: usize) -> Result<Vec<SearchRow>, RepoError> {
        let needle = q.free_text.to_lowercase();
        let (where_sql, params) = build_where(q, &needle);
        let sql = format!(
            "SELECT {SEARCH_ROW_COLUMNS} FROM grid_rows{} \
             ORDER BY when_ts_utc ASC, CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END, uuid \
             LIMIT ?",
            where_sql
        );

        // Audited for injection per sqlx 0.9's `SqlSafeStr` bound. Everything
        // interpolated into `sql` is a literal or comes from `build_where`,
        // which only ever splices `&'static str` column names returned by
        // `column_for_field`'s closed match — every user-supplied value
        // leaves as a `?` in `params`. Same reasoning for the other
        // `AssertSqlSafe` sites in this file, where the interpolated part is
        // a `?,?,?` run built from a count.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in &params {
            query = query.bind(p);
        }
        query = query.bind(limit as i64);

        let rows = match query.fetch_all(&self.pool).await {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, "grid_rows") => return Ok(Vec::new()),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };

        let mut out: Vec<SearchRow> = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(search_row_from(&r, &needle));
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
        let row = match sqlx::query(sql)
            .bind(markdown_uuid)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(row) => row,
            Err(e) if is_missing_table(&e, "grid_rows") => return Ok(None),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
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

    async fn list_docs(&self, limit: usize) -> Result<Vec<DocRow>, RepoError> {
        // Newest first, undated rows last — the picker leads with what
        // the user most recently ingested. `created_at` is a text
        // column of ISO-ish timestamps, so lexicographic DESC is
        // chronological enough.
        let sql = "SELECT markdown_uuid, title, kind, provider, created_at \
                   FROM markdowns \
                   ORDER BY created_at IS NULL, created_at DESC \
                   LIMIT ?";
        let rows = match sqlx::query(sql)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, "markdowns") => return Ok(Vec::new()),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        Ok(rows
            .into_iter()
            .map(|r| DocRow {
                markdown_uuid: r.try_get("markdown_uuid").unwrap_or_default(),
                title: r.try_get("title").ok().flatten(),
                kind: r.try_get("kind").unwrap_or_default(),
                provider: r.try_get("provider").unwrap_or_default(),
                created_at: r.try_get("created_at").ok().flatten(),
            })
            .collect())
    }

    async fn grid_row_refs(&self) -> Result<Vec<GridRowRef>, RepoError> {
        let rows = match sqlx::query(
            "SELECT uuid, kind, COALESCE(qmd_path, '') AS qmd_path, provider FROM grid_rows",
        )
        .fetch_all(&self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, "grid_rows") => return Ok(Vec::new()),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
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
        let sql = format!("SELECT {SEARCH_ROW_COLUMNS} FROM grid_rows{}", where_sql);
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in &params {
            query = query.bind(p);
        }
        let rows = match query.fetch_all(&self.pool).await {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, "grid_rows") => return Ok(Vec::new()),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        let mut by_uuid: std::collections::HashMap<String, SearchRow> =
            std::collections::HashMap::new();
        for r in rows {
            let row = search_row_from(&r, "");
            by_uuid.insert(row.uuid.clone(), row);
        }
        let mut out: Vec<SearchRow> = Vec::with_capacity(by_uuid.len());
        for u in uuids.iter().take(take) {
            if let Some(r) = by_uuid.remove(u) {
                out.push(r);
            }
        }
        Ok(out)
    }

    async fn outgoing_edges(&self, markdown_uuid: &str) -> Result<Vec<EdgeRowOut>, RepoError> {
        // LEFT JOIN so that an edge with a dangling FK (destination no
        // longer in `markdowns`) still surfaces — the UI can show the
        // raw uuid and the user at least learns the link exists.
        // The edges table may not exist on older data roots; treat any
        // SQL error as "no edges" so the chat endpoint doesn't blow up
        // mid-render.
        let sql = "SELECT e.edge_uuid, e.src_markdown_uuid, e.src_anchor_uuid, \
                          e.dst_markdown_uuid, e.dst_anchor_uuid, e.label, \
                          m.title AS dst_title \
                   FROM edges e \
                   LEFT JOIN markdowns m ON m.markdown_uuid = e.dst_markdown_uuid \
                   WHERE e.src_markdown_uuid = ?";
        let rows = match sqlx::query(sql)
            .bind(markdown_uuid)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rs) => rs,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out: Vec<EdgeRowOut> = Vec::with_capacity(rows.len());
        for r in rows {
            // Annotate the nullable columns with explicit `Option<String>`
            // so a SQL NULL maps to `None`. `try_get(...).ok()` against a
            // bare `String` collapses both NULL and lookup errors into
            // `None`; but it also turns a literal empty-string value into
            // `Some("")`, which the UI's `src_anchor_uuid === null`
            // filter then fails to match. Pinning the inferred type lifts
            // that ambiguity.
            let edge = EdgeRow {
                edge_uuid: r.try_get("edge_uuid").unwrap_or_default(),
                src_markdown_uuid: r.try_get("src_markdown_uuid").unwrap_or_default(),
                src_anchor_uuid: r
                    .try_get::<Option<String>, _>("src_anchor_uuid")
                    .unwrap_or_default(),
                dst_markdown_uuid: r.try_get("dst_markdown_uuid").unwrap_or_default(),
                dst_anchor_uuid: r
                    .try_get::<Option<String>, _>("dst_anchor_uuid")
                    .unwrap_or_default(),
                label: r.try_get::<Option<String>, _>("label").unwrap_or_default(),
            };
            out.push(EdgeRowOut {
                edge,
                dst_title: r
                    .try_get::<Option<String>, _>("dst_title")
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    async fn md_paths_for(
        &self,
        markdown_uuids: &[String],
    ) -> Result<std::collections::HashMap<String, PathBuf>, RepoError> {
        let mut out = std::collections::HashMap::with_capacity(markdown_uuids.len());
        // Chunked to stay under SQLite's bind-variable ceiling; the
        // grid asks about one batch per result set, not per row.
        for chunk in markdown_uuids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT markdown_uuid, md_path FROM markdowns \
                  WHERE md_path IS NOT NULL AND markdown_uuid IN ({placeholders})"
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for u in chunk {
                q = q.bind(u);
            }
            let rows = match q.fetch_all(&self.pool).await {
                Ok(rows) => rows,
                // A data root whose renderers have never run has no
                // `markdowns` table; that is "nothing rendered yet",
                // not a failure.
                Err(e) if is_missing_table(&e, "markdowns") => return Ok(out),
                Err(e) => return Err(RepoError::Internal(e.to_string())),
            };
            for row in rows {
                let uuid: String = match row.try_get("markdown_uuid") {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let rel: String = match row.try_get("md_path") {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                out.insert(uuid, self.root.as_ref().join(rel));
            }
        }
        Ok(out)
    }

    async fn qmd_path_for_markdown(
        &self,
        markdown_uuid: &str,
    ) -> Result<Option<PathBuf>, RepoError> {
        let row = match sqlx::query(
            "SELECT md_path FROM markdowns WHERE markdown_uuid = ? AND md_path IS NOT NULL LIMIT 1",
        )
        .bind(markdown_uuid)
        .fetch_optional(&self.pool)
        .await
        {
            Ok(row) => row,
            Err(e) if is_missing_table(&e, "markdowns") => return Ok(None),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        let Some(r) = row else { return Ok(None) };
        let rel: Option<String> = r.try_get("md_path").ok();
        Ok(rel.map(|p| self.root.as_ref().join(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stanza is the first path segment, matching how
    /// `datalib-step` names a source from its declared outputs and how
    /// `grid_index` names one from the directory it walked.
    #[test]
    fn source_name_is_the_first_path_segment() {
        assert_eq!(
            source_name_from_qmd_path("slack/rendered_md/abc/all.md"),
            "slack"
        );
        assert_eq!(
            source_name_from_qmd_path("claude-api/rendered_md/x/all.md"),
            "claude-api"
        );
        // Sharded renders nest deeper; the stanza is still segment one.
        assert_eq!(
            source_name_from_qmd_path("beeper/rendered_md/googlechat/x/2024-03.md"),
            "beeper"
        );
        // No separator means the renderer wrote outside its own tree —
        // report nothing rather than claim the filename is a source.
        assert_eq!(source_name_from_qmd_path("all.md"), "");
        assert_eq!(source_name_from_qmd_path(""), "");
    }
}
