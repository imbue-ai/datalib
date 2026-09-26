//! `DoltRepo` — production [`IndexRepo`](crate::repo::IndexRepo) backed
//! by a `sqlx::SqlitePool` against the grid index on disk. Every request
//! reads inside one read transaction: one commit, and the plain tables'
//! indexes (`docs/dev/plans/paged_grids.md`, "Pinned and indexed").

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::db::{build_where, ChatMeta};
use crate::group::{group_sql, where_within, GroupCount, Grouping, Within, MAX_GROUPS};
use crate::qmd::GridRowRef;
use crate::query::ParsedQuery;
use crate::repo::{DocRow, EdgeRowOut, IndexRepo, Listing, MapDocRow};
use crate::search::SearchRow;
use crate::sort::{Sort, DEFAULT_ORDER};
use datalib_core::repo::RepoError;
use datalib_pin::{is_missing_table, open_reader};
use datalib_schema::edges::EdgeRow;
use datalib_schema::problems::{ProblemRow, ScopeKind};

/// SQLite/doltlite-backed implementation of [`IndexRepo`].
pub struct DoltRepo {
    /// The grid index: `grid_rows`, `markdowns`, `edges`. The
    /// `grid_index` step is its only writer, and this handle cannot be
    /// a second one: it is opened `read_only`, and only once the file
    /// exists — a root that has never synced has none, and a reader must
    /// not be the thing that creates it. Filled on first use and kept: a
    /// table the step commits later is there in the next transaction.
    pool: tokio::sync::Mutex<Option<SqlitePool>>,
    db_path: PathBuf,
    root: Arc<PathBuf>,
}

/// One request's read of the index: a transaction on the read-only
/// connection, so every query in it sees the same commit. Dropping it
/// ends the transaction.
struct At {
    tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    commit: String,
    grid_rows: &'static str,
    markdowns: &'static str,
    edges: &'static str,
    problems: &'static str,
}

/// The rows `uuids` name, in that order, read inside `at`'s snapshot; one
/// the snapshot lacks is left out.
async fn rows_in(at: &mut At, uuids: &[String]) -> Result<Vec<SearchRow>, RepoError> {
    let mut by_uuid: std::collections::HashMap<String, SearchRow> =
        std::collections::HashMap::with_capacity(uuids.len());
    for chunk in uuids.chunks(LOOKUP_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT {SEARCH_ROW_COLUMNS} FROM {} WHERE uuid IN ({placeholders})",
            at.grid_rows
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for u in chunk {
            query = query.bind(u);
        }
        let rows = match query.fetch_all(&mut *at.tx).await {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, "grid_rows") => return Ok(Vec::new()),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        for r in rows {
            let row = search_row_from(&r);
            by_uuid.insert(row.uuid.clone(), row);
        }
    }
    Ok(uuids.iter().filter_map(|u| by_uuid.remove(u)).collect())
}

/// The `grid_rows` columns every [`SearchRow`] is built from. One
/// constant because every read of a search's rows selects exactly the
/// same set through [`search_row_from`]; two hand-kept lists drifted for
/// as long as they existed.
const SEARCH_ROW_COLUMNS: &str =
    "uuid, provider, kind, source_label, created_at, modified_at, is_document, author, account, \
     project, org_uuid, org_name, channel, conversation_name, conversation_uuid, markdown_uuid, \
     message_index, entire_chat, preview, source_url, notion_page_uuid, upstream_id, \
     upstream_entity_kind, source_id, byte_size, item_count, diff_status, diff_changed_columns";

/// What `ordered_uuids` runs, with its parameters.
pub fn listing_sql(
    q: &ParsedQuery,
    sort: Option<Sort>,
    within: &[Within],
) -> (String, Vec<String>) {
    let (where_sql, params) = where_within(q, within);
    let order = sort
        .and_then(Sort::order_by)
        .unwrap_or_else(|| DEFAULT_ORDER.to_string());
    (
        format!("SELECT uuid FROM grid_rows{where_sql} ORDER BY {order}"),
        params,
    )
}

/// Rows per `uuid IN (…)` lookup, well under SQLite's bound-variable
/// limit however many rows one page asks for.
const LOOKUP_CHUNK: usize = 10_000;

fn search_row_from(r: &sqlx::sqlite::SqliteRow) -> SearchRow {
    let kind: String = r.try_get("kind").unwrap_or_default();
    let author: String = r.try_get("author").unwrap_or_default();
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
        snippet: r.try_get("preview").unwrap_or_default(),
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
        source_ref: None,
        source_id: r
            .try_get::<Option<String>, _>("source_id")
            .ok()
            .flatten()
            .unwrap_or_default(),
        kind,
        author,
        channel: r.try_get("channel").unwrap_or_default(),
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
    /// A transaction on a read-only connection holds one commit for as
    /// long as it is open, and reads the plain tables, so their indexes
    /// serve it. `dolt_at_` holds a commit too but uses no secondary
    /// index, and a plain read outside a transaction can see `main` move
    /// between two statements. The writer publishes by moving `main` in
    /// one step, so the transaction never sees a half-written batch;
    /// holding one costs the writer nothing
    /// (`a_held_read_transaction_is_a_snapshot_while_the_writer_seals`).
    async fn pinned(&self) -> Result<Option<At>, RepoError> {
        let internal = |what: &str, e: sqlx::Error| RepoError::Internal(format!("{what}: {e}"));
        if !self.db_path.is_file() {
            return Ok(None);
        }
        let pool = {
            let mut slot = self.pool.lock().await;
            match slot.as_ref() {
                Some(pool) => pool.clone(),
                None => {
                    let pool = open_reader(&self.db_path)
                        .await
                        .map_err(|e| internal("open the grid index read-only", e))?;
                    *slot = Some(pool.clone());
                    pool
                }
            }
        };
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| internal("begin a read of the grid index", e))?;
        // A table read first: a scalar function answers from the
        // connection's last view, and a table read is what loads the
        // commit this transaction reads (`datalib_pin::head`).
        let _: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master")
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| internal("read the grid index", e))?;
        let commit: Option<String> = sqlx::query_scalar("SELECT dolt_hashof('HEAD')")
            .fetch_optional(&mut *tx)
            .await
            .unwrap_or(None);
        let Some(commit) = commit else {
            return Ok(None);
        };
        Ok(Some(At {
            tx,
            commit,
            grid_rows: "grid_rows",
            markdowns: "markdowns",
            edges: "edges",
            problems: "problems",
        }))
    }
}

impl At {
    /// `SELECT * FROM problems` in this read, with the caller's clause,
    /// as typed rows. An index built before the table existed reads as
    /// empty; a row this build cannot parse is an error.
    async fn problem_rows(
        &mut self,
        where_sql: &str,
        params: &[String],
        limit: usize,
    ) -> Result<Vec<ProblemRow>, RepoError> {
        let sql = format!(
            "SELECT * FROM {}{where_sql} ORDER BY last_seen_at_utc DESC, problem_uuid LIMIT ?",
            self.problems
        );
        // Audited: the table name is a literal; `where_sql`
        // comes from `problems::parse`, which splices only the column
        // names of its closed `Key` match and binds every value.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            query = query.bind(p.clone());
        }
        let rows = match query.bind(limit as i64).fetch_all(&mut *self.tx).await {
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
    async fn head(&self) -> Result<Option<String>, RepoError> {
        Ok(self.pinned().await?.map(|at| at.commit))
    }

    async fn ordered_uuids(
        &self,
        q: &ParsedQuery,
        sort: Option<Sort>,
        within: &[Within],
    ) -> Result<Listing, RepoError> {
        let (sql, params) = listing_sql(q, sort, within);
        let Some(mut at) = self.pinned().await? else {
            return Ok(Listing::default());
        };
        // Audited for injection per sqlx 0.9's `SqlSafeStr` bound. Everything
        // interpolated into `sql` is a literal (the table names on `At`
        // included, and a `Sort`'s column, from its closed match), or comes
        // from `build_where`, which only ever splices `&'static str` column
        // names returned by `column_for_field`'s closed match, or from
        // `where_within` and `group_sql`, whose column names are the
        // `&'static str`s of `GridColumn::sql`'s closed match — every
        // user-supplied value leaves as a `?` in `params`. Same reasoning
        // for the other `AssertSqlSafe` sites in this file, where the
        // interpolated part is a literal table name or a `?,?,?` run built
        // from a count.
        let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
        for p in &params {
            query = query.bind(p);
        }
        let uuids = match query.fetch_all(&mut *at.tx).await {
            Ok(uuids) => uuids,
            Err(e) if is_missing_table(&e, "grid_rows") => Vec::new(),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        Ok(Listing {
            uuids,
            at: Some(at.commit),
        })
    }

    async fn filter_uuids(
        &self,
        q: &ParsedQuery,
        uuids: &[String],
        sort: Option<Sort>,
        within: &[Within],
    ) -> Result<Listing, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Listing::default());
        };
        let (where_sql, params) = where_within(q, within);
        let order = sort.and_then(Sort::order_by);
        // One statement: qmd hands over at most its ranking depth of hits,
        // far under SQLite's bound-variable limit.
        let placeholders = vec!["?"; uuids.len()].join(",");
        let joiner = if where_sql.is_empty() {
            " WHERE"
        } else {
            " AND"
        };
        let sql = format!(
            "SELECT uuid FROM {}{where_sql}{joiner} uuid IN ({placeholders}){}",
            at.grid_rows,
            order
                .as_deref()
                .map(|o| format!(" ORDER BY {o}"))
                .unwrap_or_default()
        );
        let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
        for p in params.iter().chain(uuids) {
            query = query.bind(p);
        }
        let kept = match query.fetch_all(&mut *at.tx).await {
            Ok(found) => found,
            Err(e) if is_missing_table(&e, "grid_rows") => Vec::new(),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        let uuids = match order {
            Some(_) => kept,
            None => {
                let kept: std::collections::HashSet<String> = kept.into_iter().collect();
                let mut ranked: Vec<String> = uuids
                    .iter()
                    .filter(|u| kept.contains(*u))
                    .cloned()
                    .collect();
                // A score sort is the ranking itself; ascending reads it
                // from the bottom.
                if sort.is_some_and(|s| !s.descending) {
                    ranked.reverse();
                }
                ranked
            }
        };
        Ok(Listing {
            uuids,
            at: Some(at.commit),
        })
    }

    async fn group_counts(
        &self,
        q: &ParsedQuery,
        by: &[&'static str],
        among: Option<&[String]>,
    ) -> Result<Grouping, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Grouping::default());
        };
        let (mut where_sql, params) = where_within(q, &[]);
        if let Some(uuids) = among {
            // qmd's ranking: at most its depth, far under SQLite's
            // bound-variable limit.
            let joiner = if where_sql.is_empty() {
                " WHERE"
            } else {
                " AND"
            };
            let placeholders = vec!["?"; uuids.len()].join(",");
            where_sql = format!("{where_sql}{joiner} uuid IN ({placeholders})");
        }
        let sql = group_sql(at.grid_rows, &where_sql, by);
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params.iter().chain(among.unwrap_or_default()) {
            query = query.bind(p);
        }
        let rows = match query.fetch_all(&mut *at.tx).await {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, "grid_rows") => Vec::new(),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        let n = by.len();
        let truncated = rows.len() > MAX_GROUPS;
        let rows = &rows[..rows.len().min(MAX_GROUPS)];
        let sample_uuids: Vec<String> = rows.iter().map(|r| r.get::<String, _>(n + 1)).collect();
        // In the same snapshot as the counts, so every group's newest row
        // is there and the samples line up with the groups one for one.
        let samples = rows_in(&mut at, &sample_uuids).await?;
        let groups: Vec<GroupCount> = rows
            .iter()
            .zip(samples)
            .map(|(r, sample)| GroupCount {
                values: (0..n).map(|i| r.get::<Option<String>, _>(i)).collect(),
                count: r.get::<i64, _>(n) as u64,
                sample,
            })
            .collect();
        Ok(Grouping {
            groups,
            truncated,
            at: Some(at.commit),
        })
    }

    async fn rows_by_uuids(&self, uuids: &[String]) -> Result<Vec<SearchRow>, RepoError> {
        if uuids.is_empty() {
            return Ok(Vec::new());
        }
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        rows_in(&mut at, uuids).await
    }

    async fn chat_meta(&self, markdown_uuid: &str) -> Result<Option<ChatMeta>, RepoError> {
        // Project the per-markdown header fields out of any grid_row
        // that points at this markdown — they're denormalized identically
        // across the rows of a single markdown, so picking the canonical
        // (Chat / Slack Thread / per-provider top-level row) keeps the
        // result deterministic.
        let Some(mut at) = self.pinned().await? else {
            return Ok(None);
        };
        let sql = format!(
            "SELECT source_id, conversation_name, account, project, channel, \
                    created_at, source_label, source_url \
             FROM {} \
             WHERE markdown_uuid = ? \
             ORDER BY CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END \
             LIMIT 1",
            at.grid_rows
        );
        // Audited: `at.grid_rows` is a literal; the uuid is bound.
        let row = match sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(markdown_uuid)
            .fetch_optional(&mut *at.tx)
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
            source_id: text("source_id"),
            name: text("conversation_name"),
            account: text("account"),
            project: text("project"),
            channel: text("channel"),
            created_at: text("created_at"),
            source_label: text("source_label"),
            source_url: text("source_url"),
        }))
    }

    async fn problems(
        &self,
        query: &crate::problems::ProblemsQuery,
        limit: usize,
    ) -> Result<Vec<ProblemRow>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        at.problem_rows(&query.where_sql, &query.params, limit)
            .await
    }

    async fn document_problems(&self, markdown_uuid: &str) -> Result<Vec<ProblemRow>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
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
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT markdown_uuid, title, kind, provider, created_at \
             FROM {} \
             ORDER BY created_at IS NULL, created_at DESC \
             LIMIT ?",
            at.markdowns
        );
        // Audited: `at.markdowns` is a literal; the limit is bound.
        let rows = match sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(limit as i64)
            .fetch_all(&mut *at.tx)
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

    async fn document_rows(&self) -> Result<Vec<MapDocRow>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        // Audited: `at.grid_rows` is a literal; no values.
        let rows = match sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT markdown_uuid, qmd_path, source_id, conversation_name, provider, source_label, \
                    kind, created_at, account, channel \
               FROM {} \
              WHERE is_document = 1 AND markdown_uuid IS NOT NULL AND qmd_path IS NOT NULL",
            at.grid_rows
        )))
        .fetch_all(&mut *at.tx)
        .await
        {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, "grid_rows") => return Ok(Vec::new()),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        Ok(rows
            .into_iter()
            .map(|r| {
                let provider: Option<String> = r.try_get("provider").ok().flatten();
                let qmd_path: String = r.try_get("qmd_path").unwrap_or_default();
                MapDocRow {
                    markdown_uuid: r.try_get("markdown_uuid").unwrap_or_default(),
                    source_id: r
                        .try_get::<Option<String>, _>("source_id")
                        .ok()
                        .flatten()
                        .unwrap_or_default(),
                    qmd_path,
                    title: r.try_get("conversation_name").unwrap_or_default(),
                    provider: provider.unwrap_or_default(),
                    source_label: r.try_get("source_label").unwrap_or_default(),
                    kind: r.try_get("kind").unwrap_or_default(),
                    created_at: r.try_get("created_at").ok().flatten(),
                    account: r.try_get("account").unwrap_or_default(),
                    channel: r.try_get("channel").unwrap_or_default(),
                }
            })
            .collect())
    }

    async fn matching_documents(
        &self,
        q: &ParsedQuery,
    ) -> Result<std::collections::HashSet<String>, RepoError> {
        let (where_sql, params) = build_where(q);
        let Some(mut at) = self.pinned().await? else {
            return Ok(Default::default());
        };
        let clause = if where_sql.is_empty() {
            " WHERE markdown_uuid IS NOT NULL".to_string()
        } else {
            format!("{where_sql} AND markdown_uuid IS NOT NULL")
        };
        // Audited: as `search` — a literal table name and
        // `build_where`'s static column names; every value is bound.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT markdown_uuid FROM {}{clause}",
            at.grid_rows
        )));
        for p in &params {
            query = query.bind(p);
        }
        let rows = match query.fetch_all(&mut *at.tx).await {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, "grid_rows") => return Ok(Default::default()),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        Ok(rows
            .into_iter()
            .filter_map(|r| r.try_get("markdown_uuid").ok())
            .collect())
    }

    async fn grid_row_refs(&self) -> Result<Vec<GridRowRef>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        // Audited: `at.grid_rows` is a literal; no values.
        let rows = match sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT uuid, kind, COALESCE(qmd_path, '') AS qmd_path, provider, is_document \
             FROM {}",
            at.grid_rows
        )))
        .fetch_all(&mut *at.tx)
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

    async fn outgoing_edges(&self, markdown_uuid: &str) -> Result<Vec<EdgeRowOut>, RepoError> {
        // LEFT JOIN so that an edge with a dangling FK (destination no
        // longer in `markdowns`) still surfaces — the UI can show the
        // raw uuid and the user at least learns the link exists.
        // The edges table may not exist on older data roots; treat any
        // SQL error as "no edges" so the chat endpoint doesn't blow up
        // mid-render.
        let Some(mut at) = self.pinned().await? else {
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
        // Audited: `at.edges` and `at.markdowns` are literals; the uuid
        // is bound.
        let rows = match sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(markdown_uuid)
            .fetch_all(&mut *at.tx)
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
        let Some(mut at) = self.pinned().await? else {
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
            let rows = match q.fetch_all(&mut *at.tx).await {
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
        let Some(mut at) = self.pinned().await? else {
            return Ok(None);
        };
        // Audited: `at.markdowns` is a literal; the uuid is bound.
        let row = match sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT md_path FROM {} WHERE markdown_uuid = ? AND md_path IS NOT NULL LIMIT 1",
            at.markdowns
        )))
        .bind(markdown_uuid)
        .fetch_optional(&mut *at.tx)
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
}
