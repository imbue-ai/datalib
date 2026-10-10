//! `DoltRepo` — production [`IndexRepo`](crate::repo::IndexRepo) backed
//! by a `sqlx::SqlitePool` against the grid index on disk. Every request
//! reads inside one read transaction: one commit, and the plain tables'
//! indexes (`docs/dev/plans/paged_grids.md`, "Pinned and indexed").

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::db::{build_where, ChatMeta};
use crate::group::{
    group_sql, like_pattern, values_sql, where_within, GroupCount, Grouping, Within, MAX_GROUPS,
    MAX_VALUES,
};
use crate::problems::ProblemsQuery;
use crate::qmd::GridRowRef;
use crate::query::ParsedQuery;
use crate::repo::{DocRow, EdgeRowOut, IndexRepo, Listing, LocatedProblem, MapDocRow};
use crate::search::SearchRow;
use crate::sort::{default_order, order_by, Sort};
use crate::terms_keys::ATTACHED_AS;
use datalib_core::repo::RepoError;
use datalib_pin::{is_missing_table, open_reader};
use datalib_query::table::{Column, SearchTable};
use datalib_schema::edges::EdgeRow;
use datalib_schema::grid_rows::GridRowColumn;
use datalib_schema::problems::{ProblemRow, ProblemRowColumn, ScopeKind};

/// SQLite/doltlite-backed implementation of [`IndexRepo`].
pub struct DoltRepo {
    /// The grid index: `grid_rows`, `markdowns`, `edges`. The
    /// `grid_index` step is its only writer, and this handle cannot be
    /// a second one: it is opened `read_only`, and only once the file
    /// exists — a root that has never synced has none, and a reader must
    /// not be the thing that creates it. Filled on first use and kept: a
    /// table the step commits later is there in the next transaction.
    pool: tokio::sync::Mutex<Option<Reader>>,
    db_path: PathBuf,
    root: Arc<PathBuf>,
}

/// The one read-only connection, and the search terms file attached to
/// it, by inode: a file rebuilt under a new shape is a new file, and the
/// old one stays attached until it is noticed.
struct Reader {
    pool: SqlitePool,
    terms: Option<u64>,
}

/// One request's read of the index: a transaction on the read-only
/// connection, so every query in it sees the same commit. Dropping it
/// ends the transaction.
struct At {
    tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    commit: String,
    /// The search terms file is attached under [`ATTACHED_AS`].
    terms: bool,
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
            "SELECT {} FROM {} WHERE uuid IN ({placeholders})",
            search_row_select(),
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
const SEARCH_ROW_COLUMNS: &[GridRowColumn] = {
    use GridRowColumn as G;
    &[
        G::Uuid,
        G::Provider,
        G::Kind,
        G::SourceLabel,
        G::CreatedAt,
        G::ModifiedAt,
        G::TouchedAt,
        G::IsDocument,
        G::Author,
        G::AuthorHandle,
        G::Contact,
        G::Email,
        G::Phone,
        G::Account,
        G::Project,
        G::OrgUuid,
        G::OrgName,
        G::Channel,
        G::ConversationName,
        G::ConversationUuid,
        G::MarkdownUuid,
        G::MessageIndex,
        G::EntireChat,
        G::Preview,
        G::SourceUrl,
        G::NotionPageUuid,
        G::UpstreamId,
        G::UpstreamEntityKind,
        G::SourceId,
        G::ByteSize,
        G::ItemCount,
        G::DiffStatus,
        G::DiffChangedColumns,
    ]
};

fn search_row_select() -> String {
    let names: Vec<&str> = SEARCH_ROW_COLUMNS.iter().map(|c| c.as_str()).collect();
    names.join(", ")
}

/// What `ordered_uuids` runs, with its parameters.
pub fn listing_sql(q: &ParsedQuery, sort: &[Sort], within: &[Within]) -> (String, Vec<String>) {
    ordered_sql("grid_rows", q, sort, within)
}

/// The primary keys of the rows `q` keeps in `table` (the pinned name of
/// `C`'s table), in `sort`'s order or the table's own, with its
/// parameters.
fn ordered_sql<C: Column>(
    table: &str,
    q: &ParsedQuery<C>,
    sort: &[Sort<C>],
    within: &[Within<C>],
) -> (String, Vec<String>) {
    let (where_sql, params) = where_within(q, within);
    let order = order_by(sort).unwrap_or_else(default_order::<C::Table>);
    let key = <C::Table as SearchTable>::PRIMARY_KEY.as_str();
    (
        format!("SELECT {key} FROM {table}{where_sql} ORDER BY {order}"),
        params,
    )
}

/// Rows per `uuid IN (…)` lookup, well under SQLite's bound-variable
/// limit however many rows one page asks for.
const LOOKUP_CHUNK: usize = 10_000;

fn search_row_from(r: &sqlx::sqlite::SqliteRow) -> SearchRow {
    use GridRowColumn as G;
    let kind: String = r.try_get(G::Kind.as_str()).unwrap_or_default();
    let author: String = r.try_get(G::Author.as_str()).unwrap_or_default();
    let provider: Option<String> = r.try_get(G::Provider.as_str()).ok().flatten();
    SearchRow {
        uuid: r.try_get(G::Uuid.as_str()).unwrap_or_default(),
        conversation_uuid: r.try_get(G::ConversationUuid.as_str()).unwrap_or_default(),
        markdown_uuid: r
            .try_get::<Option<String>, _>("markdown_uuid")
            .ok()
            .flatten(),
        message_index: r
            .try_get::<Option<i64>, _>("message_index")
            .ok()
            .flatten()
            .map(|n| n as usize),
        snippet: r.try_get(G::Preview.as_str()).unwrap_or_default(),
        sender: author.clone(),
        created_at: r.try_get::<Option<String>, _>("created_at").ok().flatten(),
        modified_at: r.try_get::<Option<String>, _>("modified_at").ok().flatten(),
        touched_at: r.try_get::<Option<String>, _>("touched_at").ok().flatten(),
        is_document: r
            .try_get::<bool, _>(G::IsDocument.as_str())
            .unwrap_or(false),
        conversation_name: r.try_get(G::ConversationName.as_str()).unwrap_or_default(),
        project: r.try_get(G::Project.as_str()).unwrap_or_default(),
        account: r.try_get(G::Account.as_str()).unwrap_or_default(),
        org_uuid: r.try_get(G::OrgUuid.as_str()).unwrap_or_default(),
        org_name: r.try_get(G::OrgName.as_str()).unwrap_or_default(),
        entire_chat: r.try_get(G::EntireChat.as_str()).unwrap_or_default(),
        source: r.try_get(G::SourceLabel.as_str()).unwrap_or_default(),
        provider: provider.clone().unwrap_or_default(),
        source_ref: None,
        source_id: r
            .try_get::<Option<String>, _>("source_id")
            .ok()
            .flatten()
            .unwrap_or_default(),
        kind,
        author,
        author_handle: r
            .try_get::<Option<String>, _>(G::AuthorHandle.as_str())
            .ok()
            .flatten(),
        author_ref: None,
        contact: r
            .try_get::<Option<String>, _>(G::Contact.as_str())
            .ok()
            .flatten(),
        email: r
            .try_get::<Option<String>, _>(G::Email.as_str())
            .ok()
            .flatten(),
        phone: r
            .try_get::<Option<String>, _>(G::Phone.as_str())
            .ok()
            .flatten(),
        contact_ref: None,
        author_term: None,
        channel: r.try_get(G::Channel.as_str()).unwrap_or_default(),
        source_url: r.try_get(G::SourceUrl.as_str()).unwrap_or_default(),
        notion_page_uuid: r.try_get(G::NotionPageUuid.as_str()).unwrap_or_default(),
        upstream_id: r.try_get(G::UpstreamId.as_str()).unwrap_or_default(),
        upstream_entity_kind: r
            .try_get(G::UpstreamEntityKind.as_str())
            .unwrap_or_default(),
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
    /// A transaction on a read-only connection holds one commit and reads
    /// the plain tables, so their indexes serve it; `dolt_at_` would lose
    /// them, and a plain read outside a transaction can see `main` move
    /// between two statements
    /// (docs/dev/doltlite.md#three-ways-to-read-one-commit). The writer
    /// publishes by moving `main` in one step, so the transaction never
    /// sees a half-written batch.
    async fn pinned(&self) -> Result<Option<At>, RepoError> {
        let internal = |what: &str, e: sqlx::Error| RepoError::Internal(format!("{what}: {e}"));
        if !self.db_path.is_file() {
            return Ok(None);
        }
        let (pool, terms) = {
            let mut slot = self.pool.lock().await;
            if slot.is_none() {
                let pool = open_reader(&self.db_path)
                    .await
                    .map_err(|e| internal("open the grid index read-only", e))?;
                *slot = Some(Reader { pool, terms: None });
            }
            let reader = slot.as_mut().expect("opened above");
            attach_terms(reader, &self.root)
                .await
                .map_err(|e| internal("attach the search terms", e))?;
            (reader.pool.clone(), reader.terms.is_some())
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
            terms,
            grid_rows: "grid_rows",
            markdowns: "markdowns",
            edges: "edges",
            problems: "problems",
        }))
    }
}

/// The values the search terms of `kinds` hold, holding one bound `LIKE`
/// pattern (or, `by_name`, a handle seen under a name holding it, the
/// pattern bound again), among the rows `where_sql` keeps in `grid_rows`
/// (its values bound after the patterns): each with how many rows hold
/// it, most first.
pub fn term_values_sql(grid_rows: &str, where_sql: &str, kinds: &[u8], by_name: bool) -> String {
    let s = ATTACHED_AS;
    let names = if by_name {
        format!(
            " OR v.value IN (SELECT handle FROM {s}.names \
             WHERE LOWER(name) LIKE ? ESCAPE '\\')"
        )
    } else {
        String::new()
    };
    let codes: Vec<String> = kinds.iter().map(u8::to_string).collect();
    let among = if where_sql.is_empty() {
        String::new()
    } else {
        format!(
            " AND t.row_id IN (SELECT r.row_id FROM {s}.rows r \
             WHERE r.uuid IN (SELECT uuid FROM {grid_rows}{where_sql}))"
        )
    };
    format!(
        "SELECT v.value, count(DISTINCT t.row_id) FROM {s}.terms t \
         JOIN {s}.vals v ON v.val_id = t.val_id \
         WHERE t.kind IN ({}) AND (LOWER(v.value) LIKE ? ESCAPE '\\'{names}){among} \
         GROUP BY v.val_id ORDER BY 2 DESC, 1 LIMIT {MAX_VALUES}",
        codes.join(", ")
    )
}

/// Attaches the root's search terms file read-only under
/// [`ATTACHED_AS`], so a term on a terms key is a clause of the grid's own
/// query (`crate::terms_keys`). Outside any transaction, as SQLite
/// requires; the file has no WAL through doltlite (dolthub/doltlite#3740),
/// so a reader in a transaction holds off the terms writer for as long as
/// it reads, which each request's transaction keeps short.
async fn attach_terms(reader: &mut Reader, root: &Path) -> Result<(), sqlx::Error> {
    use std::os::unix::fs::MetadataExt;
    let path = datalib_runtime::layout::search_terms_db(root);
    let now = std::fs::metadata(&path).ok().map(|m| m.ino());
    if now == reader.terms {
        return Ok(());
    }
    let mut conn = reader.pool.acquire().await?;
    if reader.terms.is_some() {
        // Audited: a fixed schema name.
        let detach = format!("DETACH DATABASE {ATTACHED_AS}");
        sqlx::query(sqlx::AssertSqlSafe(detach))
            .execute(&mut *conn)
            .await?;
        reader.terms = None;
    }
    if now.is_some() {
        let uri = format!("{}&mode=ro", datalib_runtime::plain_sqlite::uri(&path));
        // Audited: the path comes from the data root's layout, escaped for
        // a URI, and any quote in it doubled for the SQL string.
        let attach = format!(
            "ATTACH DATABASE '{}' AS {ATTACHED_AS}",
            uri.replace('\'', "''")
        );
        sqlx::query(sqlx::AssertSqlSafe(attach))
            .execute(&mut *conn)
            .await?;
        reader.terms = now;
    }
    Ok(())
}

/// One group as `group_sql` counts it: its values, its count, and its
/// newest row's primary key.
type GroupKey = (Vec<Option<String>>, u64, String);

// Pairs each group with its sample, which the caller read in key order.
fn grouping<R>(
    keys: Vec<GroupKey>,
    samples: Vec<R>,
    truncated: bool,
    commit: String,
) -> Grouping<R> {
    let groups = keys
        .into_iter()
        .zip(samples)
        .map(|((values, count, _), sample)| GroupCount {
            values,
            count,
            sample,
        })
        .collect();
    Grouping {
        groups,
        truncated,
        at: Some(commit),
    }
}

impl At {
    /// Runs [`ordered_sql`]; a table the index does not have yet holds no
    /// rows.
    async fn ordered_keys<C: Column>(
        &mut self,
        table: &str,
        q: &ParsedQuery<C>,
        sort: &[Sort<C>],
        within: &[Within<C>],
    ) -> Result<Vec<String>, RepoError> {
        let (sql, params) = ordered_sql(table, q, sort, within);
        // Audited: the table name is a literal on `At`, every column name
        // a column enum's `as_str`, and every value bound.
        let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
        for p in &params {
            query = query.bind(p);
        }
        match query.fetch_all(&mut *self.tx).await {
            Ok(keys) => Ok(keys),
            Err(e) if is_missing_table(&e, <C::Table as SearchTable>::TABLE) => Ok(Vec::new()),
            Err(e) => Err(RepoError::Internal(e.to_string())),
        }
    }

    /// The groups of the rows `where_sql` keeps in `table`, at most
    /// [`MAX_GROUPS`] of them, and whether there were more.
    async fn group_keys<C: Column>(
        &mut self,
        table: &str,
        where_sql: &str,
        params: &[String],
        by: &[C],
    ) -> Result<(Vec<GroupKey>, bool), RepoError> {
        let sql = group_sql(table, where_sql, by);
        // Audited: as `ordered_keys`.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            query = query.bind(p);
        }
        let rows = match query.fetch_all(&mut *self.tx).await {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, <C::Table as SearchTable>::TABLE) => Vec::new(),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        let n = by.len();
        let truncated = rows.len() > MAX_GROUPS;
        let groups = rows[..rows.len().min(MAX_GROUPS)]
            .iter()
            .map(|r| {
                (
                    (0..n).map(|i| r.get::<Option<String>, _>(i)).collect(),
                    r.get::<i64, _>(n) as u64,
                    r.get::<String, _>(n + 1),
                )
            })
            .collect();
        Ok((groups, truncated))
    }

    /// The values `column` takes among the rows `where_sql` keeps in
    /// `table` that hold `typed`, most rows first.
    async fn value_counts<C: Column>(
        &mut self,
        table: &str,
        where_sql: &str,
        params: &[String],
        column: C,
        typed: &str,
    ) -> Result<Vec<(String, u64)>, RepoError> {
        let sql = values_sql(table, where_sql, column);
        // Audited: as `ordered_keys`; the column is a `&'static str`.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in params {
            query = query.bind(p);
        }
        let rows = match query
            .bind(like_pattern(typed))
            .fetch_all(&mut *self.tx)
            .await
        {
            Ok(rows) => rows,
            Err(e) if is_missing_table(&e, <C::Table as SearchTable>::TABLE) => Vec::new(),
            Err(e) => return Err(RepoError::Internal(e.to_string())),
        };
        Ok(rows
            .iter()
            .map(|r| (r.get::<String, _>(0), r.get::<i64, _>(1) as u64))
            .collect())
    }

    /// The problems `keys` name, in that order; one the snapshot lacks is
    /// left out.
    async fn problems_in(&mut self, keys: &[String]) -> Result<Vec<ProblemRow>, RepoError> {
        let mut by_key: std::collections::HashMap<String, ProblemRow> =
            std::collections::HashMap::with_capacity(keys.len());
        for chunk in keys.chunks(LOOKUP_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let where_sql = format!(" WHERE problem_uuid IN ({placeholders})");
            for row in self.problem_rows(&where_sql, chunk, chunk.len()).await? {
                by_key.insert(row.problem_uuid.clone(), row);
            }
        }
        Ok(keys.iter().filter_map(|k| by_key.remove(k)).collect())
    }

    /// `rows`, each with the document it is about.
    async fn located(&mut self, rows: Vec<ProblemRow>) -> Result<Vec<LocatedProblem>, RepoError> {
        let items: Vec<String> = rows
            .iter()
            .filter(|r| r.scope_kind == ScopeKind::Entity)
            .filter_map(|r| r.item_uuid.clone())
            .collect();
        let documents = self.documents_of(&items).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let markdown_uuid = match row.scope_kind {
                    ScopeKind::Markdown => Some(row.scope_key.clone()),
                    ScopeKind::Entity => row
                        .item_uuid
                        .as_ref()
                        .and_then(|item| documents.get(item).cloned()),
                };
                LocatedProblem { row, markdown_uuid }
            })
            .collect())
    }

    /// The document each of `uuids` is a row of, for the ones the
    /// snapshot holds.
    async fn documents_of(
        &mut self,
        uuids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, RepoError> {
        let mut out = std::collections::HashMap::with_capacity(uuids.len());
        for chunk in uuids.chunks(LOOKUP_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT uuid, markdown_uuid FROM {} \
                 WHERE uuid IN ({placeholders}) AND markdown_uuid IS NOT NULL",
                self.grid_rows
            );
            // Audited: the table name is a literal; a placeholder run
            // sized from the chunk, every uuid bound.
            let mut query = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
            for u in chunk {
                query = query.bind(u);
            }
            match query.fetch_all(&mut *self.tx).await {
                Ok(rows) => out.extend(rows),
                Err(e) if is_missing_table(&e, "grid_rows") => return Ok(out),
                Err(e) => return Err(RepoError::Internal(e.to_string())),
            }
        }
        Ok(out)
    }

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
            "SELECT * FROM {}{where_sql} ORDER BY changed_at_utc DESC, problem_uuid LIMIT ?",
            self.problems
        );
        // Audited: the table name is a literal; `where_sql` splices only
        // column names from a column enum's `as_str`, or literals, and
        // binds every value.
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
        sort: &[Sort],
        within: &[Within],
    ) -> Result<Listing, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Listing::default());
        };
        let uuids = at.ordered_keys("grid_rows", q, sort, within).await?;
        Ok(Listing {
            uuids,
            at: Some(at.commit),
        })
    }

    async fn filter_uuids(
        &self,
        q: &ParsedQuery,
        uuids: &[String],
        sort: &[Sort],
        within: &[Within],
    ) -> Result<Listing, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Listing::default());
        };
        let (where_sql, params) = where_within(q, within);
        let order = order_by(sort);
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
                // A score sort, which sorts alone, is the ranking itself;
                // ascending reads it from the bottom.
                if sort.first().is_some_and(|s| !s.descending) {
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
        by: &[GridRowColumn],
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
        let params: Vec<String> = params
            .into_iter()
            .chain(among.unwrap_or_default().iter().cloned())
            .collect();
        let (keys, truncated) = at.group_keys(at.grid_rows, &where_sql, &params, by).await?;
        let sample_uuids: Vec<String> = keys.iter().map(|(_, _, uuid)| uuid.clone()).collect();
        // In the same snapshot as the counts, so every group's newest row
        // is there and the samples line up with the groups one for one.
        let samples = rows_in(&mut at, &sample_uuids).await?;
        Ok(grouping(keys, samples, truncated, at.commit))
    }

    async fn value_counts(
        &self,
        q: &ParsedQuery,
        column: GridRowColumn,
        typed: &str,
    ) -> Result<Vec<(String, u64)>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let (where_sql, params) = where_within(q, &[]);
        at.value_counts(at.grid_rows, &where_sql, &params, column, typed)
            .await
    }

    async fn term_value_counts(
        &self,
        q: &ParsedQuery,
        kinds: &[u8],
        typed: &str,
        by_name: bool,
    ) -> Result<Vec<(String, u64)>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        if !at.terms || kinds.is_empty() {
            return Ok(Vec::new());
        }
        let (where_sql, params) = where_within(q, &[]);
        let sql = term_values_sql(at.grid_rows, &where_sql, kinds, by_name);
        // Audited: the kinds are the enum's codes, the table names are
        // `&'static str`, and every value is bound.
        let pattern = like_pattern(typed);
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&pattern);
        if by_name {
            query = query.bind(&pattern);
        }
        for p in &params {
            query = query.bind(p);
        }
        let rows = query
            .fetch_all(&mut *at.tx)
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<String, _>(0), r.get::<i64, _>(1) as u64))
            .collect())
    }

    async fn problem_value_counts(
        &self,
        q: &ProblemsQuery,
        column: ProblemRowColumn,
        typed: &str,
    ) -> Result<Vec<(String, u64)>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let (where_sql, params) = where_within(q, &[]);
        at.value_counts(at.problems, &where_sql, &params, column, typed)
            .await
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

    async fn problem_keys(
        &self,
        q: &ProblemsQuery,
        sort: &[Sort<ProblemRowColumn>],
        within: &[Within<ProblemRowColumn>],
    ) -> Result<Listing, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Listing::default());
        };
        let uuids = at.ordered_keys(at.problems, q, sort, within).await?;
        Ok(Listing {
            uuids,
            at: Some(at.commit),
        })
    }

    async fn problems_by_keys(&self, keys: &[String]) -> Result<Vec<LocatedProblem>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let rows = at.problems_in(keys).await?;
        at.located(rows).await
    }

    async fn problem_groups(
        &self,
        q: &ProblemsQuery,
        by: &[ProblemRowColumn],
    ) -> Result<Grouping<LocatedProblem>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Grouping::default());
        };
        let (where_sql, params) = where_within(q, &[]);
        let (keys, truncated) = at.group_keys(at.problems, &where_sql, &params, by).await?;
        let sample_keys: Vec<String> = keys.iter().map(|(_, _, key)| key.clone()).collect();
        let samples = at.problems_in(&sample_keys).await?;
        let samples = at.located(samples).await?;
        Ok(grouping(keys, samples, truncated, at.commit))
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
        rows.iter()
            .map(|r| r.try_get::<String, _>("markdown_uuid"))
            .collect::<Result<_, _>>()
            .map_err(|e| RepoError::Internal(format!("decode markdown_uuid: {e}")))
    }

    async fn matching_qmd_paths(
        &self,
        q: &ParsedQuery,
    ) -> Result<std::collections::HashSet<String>, RepoError> {
        let (where_sql, params) = build_where(q);
        let Some(mut at) = self.pinned().await? else {
            return Ok(Default::default());
        };
        let clause = if where_sql.is_empty() {
            " WHERE qmd_path IS NOT NULL".to_string()
        } else {
            format!("{where_sql} AND qmd_path IS NOT NULL")
        };
        // Audited: as `matching_documents`.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT qmd_path FROM {}{clause}",
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
        rows.iter()
            .map(|r| {
                r.try_get::<String, _>("qmd_path")
                    .map(|p| crate::qmd::mapping::norm_path(&p))
            })
            .collect::<Result<_, _>>()
            .map_err(|e| RepoError::Internal(format!("decode qmd_path: {e}")))
    }

    async fn grid_row_refs_for_hits(
        &self,
        hit_paths: &[String],
    ) -> Result<Vec<GridRowRef>, RepoError> {
        use datalib_schema::grid_rows::{qmd_path_key, QMD_PATH_KEY_SQL};
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let mut keys: Vec<String> = hit_paths.iter().map(|p| qmd_path_key(p)).collect();
        keys.sort();
        keys.dedup();
        let wanted = serde_json::to_string(&keys)
            .map_err(|e| RepoError::Internal(format!("encode hit paths: {e}")))?;
        // Audited: `at.grid_rows` and the key expression are literals; the
        // keys are bound.
        let rows = match sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT uuid, kind, COALESCE(qmd_path, '') AS qmd_path, provider, is_document \
             FROM {} WHERE {} IN (SELECT value FROM json_each(?))",
            at.grid_rows, QMD_PATH_KEY_SQL
        )))
        .bind(wanted)
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
        // The key finds every row a hit could name and a few more; the
        // exact path keeps the right ones.
        let named: std::collections::HashSet<String> = hit_paths
            .iter()
            .map(|p| crate::qmd::mapping::norm_path(p))
            .collect();
        out.retain(|r| named.contains(&crate::qmd::mapping::norm_path(&r.qmd_path)));
        Ok(out)
    }

    async fn people_for_handles(
        &self,
        handles: &[String],
    ) -> Result<Vec<crate::people::HandleRow>, RepoError> {
        let Some(mut at) = self.pinned().await? else {
            return Ok(Vec::new());
        };
        let wanted = serde_json::to_string(handles)
            .map_err(|e| RepoError::Internal(format!("encode handles: {e}")))?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT h.handle, c.contact_json \
               FROM source_contact_handles h \
               JOIN source_contacts c \
                 ON c.markdown_uuid = h.markdown_uuid AND c.contact_key = h.contact_key \
              WHERE h.handle IN (SELECT value FROM json_each(?))",
        )
        .bind(wanted)
        .fetch_all(&mut *at.tx)
        .await
        .map_err(|e| RepoError::Internal(format!("read people by handle: {e}")))?;
        rows.into_iter()
            .map(|(handle, json)| {
                let contact = serde_json::from_str(&json).map_err(|e| {
                    RepoError::Internal(format!("a source contact for {handle} will not read: {e}"))
                })?;
                Ok(crate::people::HandleRow { handle, contact })
            })
            .collect()
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
            // The nullable columns are read as `Option<String>`: a NULL read
            // as a bare `String` comes back `""`, which the UI's
            // `src_anchor_uuid === null` filter then fails to match.
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
        let rel: String = r
            .try_get("md_path")
            .map_err(|e| RepoError::Internal(format!("decode md_path: {e}")))?;
        Ok(Some(self.root.as_ref().join(rel)))
    }
}
