//! `DoltRepo` — production [`IndexRepo`](crate::repo::IndexRepo) backed
//! by a `sqlx::SqlitePool` against the grid index on disk. Every request
//! reads at the commit the index was at when the request began.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::db::{build_where, datalib_source_id, snippet, ChatMeta};
use crate::qmd::GridRowRef;
use crate::query::ParsedQuery;
use crate::repo::{DocRow, EdgeRowOut, IndexRepo};
use crate::search::SearchRow;
use datalib_core::repo::RepoError;
use datalib_pin::{has_unpinnable_tables, head, is_missing_table, open_reader, Pin};
use datalib_schema::edges::EdgeRow;
use datalib_schema::problems::{ProblemRow, ScopeKind};

/// SQLite/doltlite-backed implementation of [`IndexRepo`].
pub struct DoltRepo {
    /// The grid index: `grid_rows`, `markdowns`, `edges`. The
    /// `grid_index` step is its only writer, and this handle cannot be
    /// a second one: it is opened `read_only`, and only once the file
    /// exists — a root that has never synced has none, and a reader must
    /// not be the thing that creates it. Filled on first use, and
    /// replaced when the step commits a table it cannot pin (see
    /// [`DoltRepo::pinned`]).
    pool: tokio::sync::Mutex<Option<SqlitePool>>,
    db_path: PathBuf,
    root: Arc<PathBuf>,
}

/// The tables one request reads, each at the request's pin.
struct At {
    pool: SqlitePool,
    grid_rows: String,
    markdowns: String,
    edges: String,
    problems: String,
}

/// The `grid_rows` columns every [`SearchRow`] is built from. One
/// constant because `search` and `search_by_uuids` select exactly the
/// same set through [`search_row_from`]; two hand-kept lists drifted for
/// as long as they existed.
const SEARCH_ROW_COLUMNS: &str =
    "uuid, provider, kind, source_label, created_at, modified_at, is_document, author, account, \
     project, org_uuid, org_name, channel, conversation_name, conversation_uuid, markdown_uuid, \
     message_index, entire_chat, text, slack_link, source_url, notion_page_uuid, upstream_id, \
     upstream_entity_kind, qmd_path, byte_size, item_count, diff_status, diff_changed_columns";

fn search_row_from(r: &sqlx::sqlite::SqliteRow, needle: &str) -> SearchRow {
    let kind: String = r.try_get("kind").unwrap_or_default();
    let author: String = r.try_get("author").unwrap_or_default();
    let text: String = r.try_get("text").unwrap_or_default();
    let qmd_path: String = r.try_get("qmd_path").unwrap_or_default();
    let provider: Option<String> = r.try_get("provider").ok().flatten();
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
        created_at: r.try_get::<Option<String>, _>("created_at").ok().flatten(),
        modified_at: r.try_get::<Option<String>, _>("modified_at").ok().flatten(),
        is_document: r.try_get::<bool, _>("is_document").unwrap_or(false),
        conversation_name: r.try_get("conversation_name").unwrap_or_default(),
        project: r.try_get("project").unwrap_or_default(),
        account: r.try_get("account").unwrap_or_default(),
        org_uuid: r.try_get("org_uuid").unwrap_or_default(),
        org_name: r.try_get("org_name").unwrap_or_default(),
        entire_chat: r.try_get("entire_chat").unwrap_or_default(),
        source: r.try_get("source_label").unwrap_or_default(),
        provider: provider.clone().unwrap_or_default(),
        provider_ref: None,
        source_ref: None,
        source_id: source_id_for(provider.as_deref(), &qmd_path),
        kind,
        author,
        channel: r.try_get("channel").unwrap_or_default(),
        slack_link: r.try_get("slack_link").unwrap_or_default(),
        source_url: r.try_get("source_url").unwrap_or_default(),
        notion_page_uuid: r.try_get("notion_page_uuid").unwrap_or_default(),
        upstream_id: r.try_get("upstream_id").unwrap_or_default(),
        upstream_entity_kind: r.try_get("upstream_entity_kind").unwrap_or_default(),
        byte_size: r.try_get::<Option<i64>, _>("byte_size").ok().flatten(),
        item_count: r.try_get::<Option<i64>, _>("item_count").ok().flatten(),
        diff_status: r.try_get::<Option<String>, _>("diff_status").ok().flatten(),
        diff_changed_columns: r
            .try_get::<Option<String>, _>("diff_changed_columns")
            .ok()
            .flatten(),
        score: None,
    }
}

/// The id of the source a row is filed under, which is the source it
/// *belongs to* rather than the directory it happens to sit in.
///
/// For everything a provider rendered the two are the same, and the
/// answer is [`source_id_from_qmd_path`]. The storage rows are the
/// exception: each source's measurements are written into that source's
/// own `render_markdown/`, because that is the one tree the render step
/// may write, but a measurement is datalib describing the mirror rather
/// than part of it. So they are filed under datalib, which is what
/// their `provider` tag already says — see `Provider::Datalib`.
fn source_id_for(provider: Option<&str>, qmd_path: &str) -> String {
    if provider == Some(datalib_source_id()) {
        return datalib_source_id().to_string();
    }
    source_id_from_qmd_path(qmd_path)
}

/// The first segment of a document's data-root-relative path
/// (`slack/render_markdown/x/all.md` → `slack`). The stanza directory
/// *is* the group id, which is what `grid_index` relies on when it
/// walks one directory per group.
fn source_id_from_qmd_path(qmd_path: &str) -> String {
    match qmd_path.split_once('/') {
        Some((first, _)) => first.to_string(),
        None => String::new(),
    }
}

impl DoltRepo {
    /// The directory is created here, the file never is. The server's
    /// watcher arms its watch on this directory when it exists, and a
    /// grid that learns of the first `grid_index` pass from that watch
    /// needs it armed before the pass, not after.
    pub async fn open(root: Arc<PathBuf>) -> Result<Self, sqlx::Error> {
        let db_path = datalib_core::layout::grid_index_db(&root);
        if let Some(dir) = db_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Ok(Self {
            pool: tokio::sync::Mutex::new(None),
            db_path,
            root,
        })
    }

    /// The store at the commit it is at now, or `None` while the
    /// `grid_index` step has yet to create it or commit into it — the
    /// same answer every read gives for a missing table.
    ///
    /// A plain `SELECT` would read the working set, which under
    /// streaming holds the step's batch from its SQL `COMMIT` until its
    /// `dolt_commit` (`doltlite_two_process_test` measures the window).
    /// So each request resolves HEAD once and reads every table through
    /// `dolt_at_` at that hash. The handle is kept: doltlite registers
    /// the `dolt_at_` modules when a connection opens, so one opened
    /// before the step committed a table — the first pass on a fresh
    /// root — can never read that table pinned, and is replaced.
    async fn pinned(&self) -> Result<Option<At>, RepoError> {
        let internal = |what: &str, e: sqlx::Error| RepoError::Internal(format!("{what}: {e}"));
        if !self.db_path.is_file() {
            return Ok(None);
        }
        let mut slot = self.pool.lock().await;
        if let Some(pool) = slot.as_ref() {
            if has_unpinnable_tables(pool)
                .await
                .map_err(|e| internal("probe the grid index's modules", e))?
            {
                if let Some(old) = slot.take() {
                    old.close().await;
                }
            }
        }
        let pool = match slot.as_ref() {
            Some(pool) => pool.clone(),
            None => {
                let pool = open_reader(&self.db_path)
                    .await
                    .map_err(|e| internal("open the grid index read-only", e))?;
                *slot = Some(pool.clone());
                pool
            }
        };
        drop(slot);
        let Some(pin) = head(&pool)
            .await
            .map_err(|e| RepoError::Internal(format!("pin the grid index: {e}")))?
        else {
            return Ok(None);
        };
        Ok(Some(At::new(pool, &pin)))
    }
}

impl At {
    fn new(pool: SqlitePool, pin: &Pin) -> Self {
        At {
            pool,
            grid_rows: pin.table("grid_rows"),
            markdowns: pin.table("markdowns"),
            edges: pin.table("edges"),
            problems: pin.table("problems"),
        }
    }

    /// `SELECT * FROM problems` at the pin, with the caller's clause,
    /// as typed rows. An index built before the table existed reads as
    /// empty; a row this build cannot parse is an error.
    async fn problem_rows(
        &self,
        where_sql: &str,
        params: &[String],
        limit: usize,
    ) -> Result<Vec<ProblemRow>, RepoError> {
        let sql = format!(
            "SELECT * FROM {}{where_sql} ORDER BY last_seen_at_utc DESC, problem_uuid LIMIT ?",
            self.problems
        );
        // Audited: the table expression is `Pin::table`'s; `where_sql`
        // comes from `problems::parse`, which splices only the column
        // names of its closed `Key` match and binds every value.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            query = query.bind(p.clone());
        }
        let rows = match query.bind(limit as i64).fetch_all(&self.pool).await {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, "problems") => return Ok(Vec::new()),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        rows.iter()
            .map(ProblemRow::from_row)
            .collect::<anyhow::Result<Vec<_>>>()
            .map_err(|e| RepoError::Internal(format!("{e:#}")))
    }
}

#[async_trait]
impl IndexRepo for DoltRepo {
    async fn search(&self, q: &ParsedQuery, limit: usize) -> Result<Vec<SearchRow>, RepoError> {
        let needle = q.free_text.to_lowercase();
        let (where_sql, params) = build_where(q, &needle);
        let Some(at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT {SEARCH_ROW_COLUMNS} FROM {}{} \
             ORDER BY created_at_utc ASC, is_document DESC, uuid \
             LIMIT ?",
            at.grid_rows, where_sql
        );

        // Audited for injection per sqlx 0.9's `SqlSafeStr` bound. Everything
        // interpolated into `sql` is a literal, a table expression from
        // `Pin::table` (a literal name and a hash `Pin::at` checked is 40
        // hex characters), or comes from `build_where`, which only ever
        // splices `&'static str` column names returned by
        // `column_for_field`'s closed match — every user-supplied value
        // leaves as a `?` in `params`. Same reasoning for the other
        // `AssertSqlSafe` sites in this file, where the interpolated part is
        // a table expression or a `?,?,?` run built from a count.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in &params {
            query = query.bind(p);
        }
        query = query.bind(limit as i64);

        let rows = match query.fetch_all(&at.pool).await {
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
        let Some(at) = self.pinned().await? else {
            return Ok(None);
        };
        let sql = format!(
            "SELECT conversation_name, account, project, channel, created_at, source_label, \
                    COALESCE(source_url, slack_link) AS source_url_or_link \
             FROM {} \
             WHERE markdown_uuid = ? \
             ORDER BY CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END \
             LIMIT 1",
            at.grid_rows
        );
        let row = match sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(markdown_uuid)
            .fetch_optional(&at.pool)
            .await
        {
            Ok(row) => row,
            Err(e) if is_missing_table(&e, "grid_rows") => return Ok(None),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        let Some(r) = row else { return Ok(None) };
        // Decoded as `Option<String>`, so a SQL NULL is `None`: read into a
        // bare `String` the sqlite driver hands back `""` for NULL and the
        // header would show an empty project rather than none (#13).
        let text = |col: &str| r.try_get::<Option<String>, _>(col).ok().flatten();
        Ok(Some(ChatMeta {
            name: text("conversation_name"),
            account: text("account"),
            project: text("project"),
            channel: text("channel"),
            created_at: text("created_at"),
            source_label: text("source_label"),
            source_url: text("source_url_or_link"),
        }))
    }

    async fn problems(
        &self,
        query: &crate::problems::ProblemsQuery,
        limit: usize,
    ) -> Result<Vec<ProblemRow>, RepoError> {
        let Some(at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        at.problem_rows(&query.where_sql, &query.params, limit)
            .await
    }

    async fn document_problems(&self, markdown_uuid: &str) -> Result<Vec<ProblemRow>, RepoError> {
        let Some(at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        // The document's own rows, and any entity-scoped row about one
        // of its items — a parse failure knows the entity, not the
        // document, and reaches it through the item it would have been.
        let where_sql = format!(
            " WHERE (scope_kind = ? AND scope_key = ?) \
             OR (scope_kind = ? AND item_uuid IN (SELECT uuid FROM {} WHERE markdown_uuid = ?))",
            at.grid_rows
        );
        let params = vec![
            ScopeKind::Markdown.as_str().to_string(),
            markdown_uuid.to_string(),
            ScopeKind::Entity.as_str().to_string(),
            markdown_uuid.to_string(),
        ];
        at.problem_rows(&where_sql, &params, 10_000).await
    }

    async fn list_docs(&self, limit: usize) -> Result<Vec<DocRow>, RepoError> {
        // Newest first, undated rows last — the picker leads with what
        // the user most recently ingested. `created_at` is a text
        // column of ISO-ish timestamps, so lexicographic DESC is
        // chronological enough.
        let Some(at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT markdown_uuid, title, kind, provider, created_at \
             FROM {} \
             ORDER BY created_at IS NULL, created_at DESC \
             LIMIT ?",
            at.markdowns
        );
        let rows = match sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(limit as i64)
            .fetch_all(&at.pool)
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
        let Some(at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let rows = match sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT uuid, kind, COALESCE(qmd_path, '') AS qmd_path, provider, is_document \
             FROM {}",
            at.grid_rows
        )))
        .fetch_all(&at.pool)
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
                is_document: r.try_get("is_document").unwrap_or(false),
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
        let Some(at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT {SEARCH_ROW_COLUMNS} FROM {}{}",
            at.grid_rows, where_sql
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in &params {
            query = query.bind(p);
        }
        let rows = match query.fetch_all(&at.pool).await {
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
        let Some(at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT e.edge_uuid, e.src_markdown_uuid, e.src_anchor_uuid, \
                    e.dst_markdown_uuid, e.dst_anchor_uuid, e.label, \
                    m.title AS dst_title \
             FROM {} e \
             LEFT JOIN {} m ON m.markdown_uuid = e.dst_markdown_uuid \
             WHERE e.src_markdown_uuid = ?",
            at.edges, at.markdowns
        );
        let rows = match sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(markdown_uuid)
            .fetch_all(&at.pool)
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
        let Some(at) = self.pinned().await? else {
            return Ok(out);
        };
        // Chunked to stay under SQLite's bind-variable ceiling; the
        // grid asks about one batch per result set, not per row.
        for chunk in markdown_uuids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT markdown_uuid, md_path FROM {} \
                  WHERE md_path IS NOT NULL AND markdown_uuid IN ({placeholders})",
                at.markdowns
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for u in chunk {
                q = q.bind(u);
            }
            let rows = match q.fetch_all(&at.pool).await {
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
        let Some(at) = self.pinned().await? else {
            return Ok(None);
        };
        let row = match sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT md_path FROM {} WHERE markdown_uuid = ? AND md_path IS NOT NULL LIMIT 1",
            at.markdowns
        )))
        .bind(markdown_uuid)
        .fetch_optional(&at.pool)
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

    /// The SELECT list is hand-written and the DDL is derived from
    /// `GridRow`; a name here the table does not have fails every
    /// search with "no such column", but only once a search runs.
    #[test]
    fn every_selected_column_is_in_the_grid_rows_ddl() {
        let (_, ddl_columns) = datalib_schema::grid_rows::COLUMNS[0];
        for name in SEARCH_ROW_COLUMNS.split(',').map(str::trim) {
            assert!(
                ddl_columns.contains(&name),
                "SEARCH_ROW_COLUMNS names `{name}`, which grid_rows does not have"
            );
        }
    }

    /// The stanza is the first path segment, matching how
    /// `datalib-step` names a source from its declared outputs and how
    /// `grid_index` names one from the directory it walked.
    #[test]
    fn source_id_is_the_first_path_segment() {
        assert_eq!(
            source_id_from_qmd_path("slack/render_markdown/abc/all.md"),
            "slack"
        );
        assert_eq!(
            source_id_from_qmd_path("claude-api/render_markdown/x/all.md"),
            "claude-api"
        );
        // Sharded renders nest deeper; the stanza is still segment one.
        assert_eq!(
            source_id_from_qmd_path("beeper/render_markdown/googlechat/x/2024-03.md"),
            "beeper"
        );
        // No separator means the renderer wrote outside its own tree —
        // report nothing rather than claim the filename is a source.
        assert_eq!(source_id_from_qmd_path("all.md"), "");
        assert_eq!(source_id_from_qmd_path(""), "");
    }

    /// A storage row sits under the source it measures and is filed
    /// under datalib anyway, so the grid never shows a measurement in
    /// the same bucket as the data it describes.
    #[test]
    fn measurements_are_filed_under_datalib_not_the_measured_source() {
        let path = "claude-api/render_markdown/_datalib/storage.md";
        assert_eq!(source_id_for(Some("datalib"), path), "datalib");
        assert_eq!(source_id_for(Some("claude"), path), "claude-api");
        assert_eq!(source_id_for(None, path), "claude-api");
    }
}
