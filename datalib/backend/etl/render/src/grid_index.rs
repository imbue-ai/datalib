//! The unified grid index: one table stacked from every source's render
//! store.
//!
//! Two entry points, which are the same write path from two sides.
//! [`apply_one`] writes one rendered document; [`build_grid_index`] stacks
//! every source's store into the unified document index, which is the `grid_index` DAG
//! step's whole job.
//!
//! A re-render replaces a document's rows rather than accumulating them,
//! writing only those that differ, and the search terms of what it wrote
//! in the same transaction (`search_terms`). The index is a plain SQLite
//! file in WAL mode, not a doltlite store: nothing reads its history, and
//! a reader's transaction already reads one state of it. Each transaction
//! that changes what a reader sees raises its generation
//! (`datalib_pin::generation`).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use datalib_contact_schema::NormalizedContact;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::StoreKind;
use datalib_etl::stop::StopFlag;
use datalib_photo_schema::NormalizedPhoto;
use datalib_schema::edges::{EdgeRow, DDL as EDGES_DDL};
use datalib_schema::grid_rows::{GridRow, DDL as GRID_ROWS_DDL, INDEXES as GRID_ROWS_INDEXES};
use datalib_schema::markdowns::DDL as MARKDOWNS_TABLE_DDL;
use datalib_schema::problems::{ProblemRow, Severity, DDL as PROBLEMS_DDL};
use datalib_schema::search_terms::{TERMS_DDL, TERMS_SHAPE};
use datalib_schema::source_contact_handles::DDL as SOURCE_CONTACT_HANDLES_DDL;
use datalib_schema::source_contacts::DDL as SOURCE_CONTACTS_DDL;
use datalib_schema::source_cursors::{SourceCursorRow, DDL as SOURCE_CURSORS_DDL};
use datalib_schema::source_photos::DDL as SOURCE_PHOTOS_DDL;
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tokio::sync::Mutex;

use crate::section::Section;

/// Serializes concurrent writers against one doltlite index pool, and
/// optionally batches every write into one transaction.
///
/// Doltlite runs one write at a time per file, so per-task pool connections
/// calling `apply_one` would queue for it and, past the busy timeout, fail
/// with `database is locked`. Batching matters as much: every transaction
/// rewrites each page it touched (docs/dev/doltlite.md#what-a-write-costs).
///
/// The counters answer "where is the time going": `total_wait` high against
/// wall time means writers are queuing, and `total_hold / acquisitions` is the
/// average per-doc write cost.
pub struct WriteLock {
    pool: SqlitePool,
    inner: Mutex<WriteLockInner>,
    total_wait_ns: AtomicU64,
    total_hold_ns: AtomicU64,
    acquisitions: AtomicU64,
    /// The documents this lock's run has written so far. A lock lives for
    /// one run of one writer, so this is what separates "two documents of
    /// this run minted one row uuid" from "a row moved here from a
    /// document an earlier run wrote" — see `put_grid_row`.
    written: std::sync::Mutex<std::collections::HashSet<String>>,
    /// In the grid index, the grid rows written or deleted since the
    /// search terms last followed them; `None` in a render store, which
    /// holds no terms.
    changed: Option<std::sync::Mutex<Vec<String>>>,
}

struct WriteLockInner {
    /// Held connection during an active batch. `None` outside a transaction,
    /// where `acquire` takes a fresh connection per call.
    tx_conn: Option<sqlx::pool::PoolConnection<sqlx::Sqlite>>,
}

#[derive(Debug, Clone, Copy)]
pub struct WriteLockMetrics {
    pub total_wait: Duration,
    pub total_hold: Duration,
    pub acquisitions: u64,
}

impl WriteLockMetrics {
    pub fn avg_wait(&self) -> Duration {
        if self.acquisitions == 0 {
            Duration::ZERO
        } else {
            self.total_wait / self.acquisitions as u32
        }
    }
    pub fn avg_hold(&self) -> Duration {
        if self.acquisitions == 0 {
            Duration::ZERO
        } else {
            self.total_hold / self.acquisitions as u32
        }
    }
}

impl WriteLock {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            inner: Mutex::new(WriteLockInner { tx_conn: None }),
            total_wait_ns: AtomicU64::new(0),
            total_hold_ns: AtomicU64::new(0),
            acquisitions: AtomicU64::new(0),
            written: std::sync::Mutex::new(std::collections::HashSet::new()),
            changed: None,
        }
    }

    /// The grid index's lock, which keeps the rows its writes change for
    /// the search terms to follow.
    pub fn for_index(pool: SqlitePool) -> Self {
        Self {
            changed: Some(std::sync::Mutex::new(Vec::new())),
            ..Self::new(pool)
        }
    }

    fn note_changed(&self, uuids: impl IntoIterator<Item = String>) {
        if let Some(changed) = &self.changed {
            changed.lock().unwrap().extend(uuids);
        }
    }

    fn changed_rows(&self) -> usize {
        self.changed.as_ref().map_or(0, |c| c.lock().unwrap().len())
    }

    fn take_changed(&self) -> Vec<String> {
        self.changed
            .as_ref()
            .map_or_else(Vec::new, |c| std::mem::take(&mut *c.lock().unwrap()))
    }

    fn note_written(&self, markdown_uuid: &str) {
        self.written
            .lock()
            .unwrap()
            .insert(markdown_uuid.to_string());
    }

    fn written_this_run(&self, markdown_uuid: &str) -> bool {
        self.written.lock().unwrap().contains(markdown_uuid)
    }

    pub fn new_arc(pool: SqlitePool) -> Arc<Self> {
        Arc::new(Self::new(pool))
    }

    pub async fn begin_transaction(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        assert!(
            inner.tx_conn.is_none(),
            "WriteLock: begin_transaction called twice without commit/rollback",
        );
        let mut conn = self
            .pool
            .acquire()
            .await
            .context("WriteLock: acquire conn for BEGIN")?;
        sqlx::query("BEGIN")
            .execute(&mut *conn)
            .await
            .context("WriteLock: BEGIN")?;
        inner.tx_conn = Some(conn);
        Ok(())
    }

    pub async fn in_transaction(&self) -> bool {
        self.inner.lock().await.tx_conn.is_some()
    }

    pub async fn commit_transaction(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let mut conn = inner
            .tx_conn
            .take()
            .expect("WriteLock: commit_transaction without begin");
        sqlx::query("COMMIT")
            .execute(&mut *conn)
            .await
            .context("WriteLock: COMMIT")?;
        Ok(())
    }

    pub async fn rollback_transaction(&self) -> Result<()> {
        // What the rolled-back writes changed was never written.
        self.take_changed();
        let mut inner = self.inner.lock().await;
        let Some(mut conn) = inner.tx_conn.take() else {
            return Ok(());
        };
        sqlx::query("ROLLBACK")
            .execute(&mut *conn)
            .await
            .context("WriteLock: ROLLBACK")
            .map(|_| ())
    }

    /// Acquire write access. Inside a transaction the guard hands out the
    /// held connection so statements accumulate in the batch; otherwise it
    /// takes a fresh pool connection that auto-commits at release.
    pub async fn acquire<'a>(&'a self) -> Result<WriteLockGuard<'a>> {
        let wait_start = Instant::now();
        let inner_guard = self.inner.lock().await;
        let waited = wait_start.elapsed().as_nanos() as u64;
        self.total_wait_ns.fetch_add(waited, Ordering::Relaxed);
        self.acquisitions.fetch_add(1, Ordering::Relaxed);

        let fresh_conn = if inner_guard.tx_conn.is_some() {
            None
        } else {
            Some(
                self.pool
                    .acquire()
                    .await
                    .context("WriteLock: acquire conn")?,
            )
        };

        Ok(WriteLockGuard {
            inner: inner_guard,
            fresh_conn,
            held_since: Instant::now(),
            owner: self,
        })
    }

    pub fn metrics(&self) -> WriteLockMetrics {
        WriteLockMetrics {
            total_wait: Duration::from_nanos(self.total_wait_ns.load(Ordering::Relaxed)),
            total_hold: Duration::from_nanos(self.total_hold_ns.load(Ordering::Relaxed)),
            acquisitions: self.acquisitions.load(Ordering::Relaxed),
        }
    }
}

/// Dropping the guard stamps the hold-time counter and, outside a
/// transaction, returns the connection to the pool.
pub struct WriteLockGuard<'a> {
    inner: tokio::sync::MutexGuard<'a, WriteLockInner>,
    fresh_conn: Option<sqlx::pool::PoolConnection<sqlx::Sqlite>>,
    held_since: Instant,
    owner: &'a WriteLock,
}

impl<'a> WriteLockGuard<'a> {
    /// The active write connection: the same one across every `acquire`
    /// while a transaction is open, a fresh per-call one otherwise.
    pub fn conn(&mut self) -> &mut sqlx::pool::PoolConnection<sqlx::Sqlite> {
        if let Some(c) = self.inner.tx_conn.as_mut() {
            return c;
        }
        self.fresh_conn
            .as_mut()
            .expect("WriteLockGuard: conn unexpectedly absent")
    }
}

impl Drop for WriteLockGuard<'_> {
    fn drop(&mut self) {
        let held = self.held_since.elapsed().as_nanos() as u64;
        self.owner.total_hold_ns.fetch_add(held, Ordering::Relaxed);
    }
}

/// Per-rendered-markdown metadata: one row per `.md` file.
///
/// `markdown_uuid` is the canonical addressing primitive for rendered output.
/// A sharded render (beeper writes one file per period) maps one upstream
/// conversation to N rows, so `conversation_uuid` is not unique here.
pub const MARKDOWNS_DDL: &str = MARKDOWNS_TABLE_DDL[0].1;

/// Stats emitted on every load run. Stable shape, so a web UI can poll or
/// stream it without per-provider branches.
#[derive(Debug, Default, Serialize)]
pub struct GridIndexSummary {
    pub markdowns_total: usize,
    pub markdowns_loaded: usize,
    pub rows_inserted: usize,
    /// Documents dropped because the source that owned them stopped holding
    /// them. Only a cursor-driven run can be non-zero here.
    pub markdowns_removed: usize,
    /// Problem rows copied in from the render stores this run read.
    pub problems_copied: usize,
    /// Search terms written for the rows this run changed.
    pub terms_written: usize,
    /// Transactions that changed what a reader sees.
    pub seals: usize,
    /// Sources whose render store this build could not read, left as the
    /// index already had them.
    pub sources_unreadable: Vec<String>,
    /// Sources whose read failed for any other reason, with the error.
    /// Also left as the index had them; the step fails on them once
    /// every other source is sealed.
    pub sources_failed: Vec<(String, String)>,
}

/// The scope key of the one problem the index records itself: a
/// source's render store it could not read. Every other row in the
/// index is a copy of a source's.
pub const UNREADABLE_STORE_KEY: &str = "render_store";

/// What a row the index recorded itself is about, in words; `None` for
/// any other key.
pub fn about(scope_key: &str) -> Option<String> {
    (scope_key == UNREADABLE_STORE_KEY).then(|| "this source's render store".to_string())
}

/// Counts by severity of the problems the index recorded itself, for
/// the step's report. The copies are counted by the steps that found
/// them; counting them here too would show each one twice.
pub async fn own_problem_counts(
    pool: &SqlitePool,
) -> Result<HashMap<datalib_schema::problems::Severity, i64>> {
    let rows = sqlx::query(
        "SELECT severity, COUNT(*) FROM problems \
         WHERE scope_kind = ? AND scope_key = ? GROUP BY severity",
    )
    .bind(datalib_schema::problems::ScopeKind::Entity.as_str())
    .bind(UNREADABLE_STORE_KEY)
    .fetch_all(pool)
    .await
    .context("count the index's own problems")?;
    let mut out = HashMap::new();
    for r in rows {
        let word: String = r.try_get(0)?;
        let severity = datalib_schema::problems::Severity::parse(&word)
            .with_context(|| format!("problems.severity: unknown spelling {word:?}"))?;
        out.insert(severity, r.try_get::<i64, _>(1)?);
    }
    Ok(out)
}

/// One source's `problems`, replaced whole when they differ from what the
/// index holds; `true` when they did. The stamps come through unchanged:
/// `first_seen_at_utc` is when the problem was first seen where it
/// happened, not when the index first copied it.
pub(crate) async fn replace_source_problems(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    source_id: &str,
    rows: &[ProblemRow],
) -> Result<bool> {
    let held = sqlx::query("SELECT * FROM problems WHERE source_id = ? ORDER BY problem_uuid")
        .bind(source_id)
        .fetch_all(&mut **conn)
        .await
        .context("read the source's problems")?
        .iter()
        .map(ProblemRow::from_row)
        .collect::<Result<Vec<_>>>()?;
    let mut incoming = rows.to_vec();
    incoming.sort_by(|a, b| a.problem_uuid.cmp(&b.problem_uuid));
    if held == incoming {
        return Ok(false);
    }
    sqlx::query("DELETE FROM problems WHERE source_id = ?")
        .bind(source_id)
        .execute(&mut **conn)
        .await
        .context("clear the source's problems")?;
    let sql = datalib_etl::bulk::insert_sql::<ProblemRow>();
    for row in rows {
        // Audited: `sql` is built from `ProblemRow`'s associated consts,
        // never from row data; all values bound.
        row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql.clone())))
            .execute(&mut **conn)
            .await
            .with_context(|| format!("insert problem {}", row.problem_uuid))?;
    }
    Ok(true)
}

/// A render store that lacks a table or column this build reads, though
/// its `_datalib_meta` names this shape: treated as one in another shape.
/// The render step rebuilds it the next time it runs.
fn written_in_another_shape(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<sqlx::Error>()
            .and_then(datalib_etl::pin::missing_schema)
            .is_some()
    })
}

/// What a source whose render store could not be read is filed under.
/// The next pass that reads the store replaces the source's problems
/// wholesale, which is what clears it.
fn unreadable_store_problem(
    source_id: &str,
    why: &str,
    severity: datalib_schema::problems::Severity,
) -> ProblemRow {
    use datalib_schema::problems::{Outcome, Problem, Reason, Scope, Stage};
    ProblemRow::new(
        source_id,
        Stage::Render,
        Scope::Entity(UNREADABLE_STORE_KEY),
        None,
        Outcome::Dropped,
        Problem::record(Reason::RenderFailed, why).severity(severity),
        None,
    )
}

const OLDER_SHAPE: &str = "its render store is in an older shape; sync this source to re-render it";

async fn record_unreadable_store(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    row: ProblemRow,
    now: &datalib_time::StoredStamp,
) -> Result<()> {
    let earlier = sqlx::query("SELECT * FROM problems WHERE problem_uuid = ?")
        .bind(&row.problem_uuid)
        .fetch_optional(&mut **conn)
        .await
        .context("read the unreadable-store warning")?
        .map(|r| ProblemRow::from_row(&r))
        .transpose()?;
    let row = row.stamped(earlier.as_ref(), &now.utc, now.tz_offset.as_deref());
    let row = &row;
    sqlx::query("DELETE FROM problems WHERE problem_uuid = ?")
        .bind(&row.problem_uuid)
        .execute(&mut **conn)
        .await
        .context("clear the unreadable-store warning")?;
    // Audited: `insert_sql` is built from `ProblemRow`'s associated
    // consts, never from row data; all values bound.
    row.bind_into(sqlx::query(sqlx::AssertSqlSafe(
        datalib_etl::bulk::insert_sql::<ProblemRow>(),
    )))
    .execute(&mut **conn)
    .await
    .with_context(|| format!("record that {}'s render store is unreadable", row.source_id))?;
    // A fan-in comes here once per producer checkpoint, for as long as the
    // store stays this way; the row says so meanwhile.
    if is_news(earlier.as_ref(), row) {
        tracing::warn!(
            source = %row.source_id,
            why = %row.sample,
            "this source's render store cannot be indexed; indexing the others \
             and leaving it as it was until that changes"
        );
        datalib_schema::problems::note_recorded([row]);
    }
    Ok(())
}

/// Whether a stamped row says something the one before it did not: a row's
/// `changed_at_utc` moves only when what it says does.
fn is_news(earlier: Option<&ProblemRow>, row: &ProblemRow) -> bool {
    earlier.is_none_or(|e| e.changed_at_utc != row.changed_at_utc)
}

pub fn schema_hash() -> String {
    datalib_store_meta::schema_hash(
        index_ddl()
            .chain(GRID_ROWS_INDEXES.iter().map(|(_table, ddl)| *ddl))
            .chain(DOCUMENT_LOOKUP_INDEXES.iter().copied())
            // What the terms derive, which no DDL shows.
            .chain([TERMS_SHAPE]),
    )
}

/// What replacing one document looks its old rows up by, in every store
/// that holds documents. Without them each document's `DELETE` scans the
/// whole table, and a full render is quadratic in the store's size.
/// Apart from `GRID_ROWS_INDEXES`, which are the search bar's.
pub(crate) const DOCUMENT_LOOKUP_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS grid_rows_by_markdown ON grid_rows (markdown_uuid)",
    "CREATE INDEX IF NOT EXISTS edges_by_src_markdown ON edges (src_markdown_uuid)",
    // A chip asks who a handle is.
    "CREATE INDEX IF NOT EXISTS source_contact_handles_by_handle ON source_contact_handles (handle)",
    // The photos applet asks for one photo by its id.
    "CREATE INDEX IF NOT EXISTS source_photos_by_photo ON source_photos (photo_uuid)",
];

/// Tables an older build made in every store that holds documents.
/// `supplied_search_terms` kept each row's supplied terms apart from the
/// row, owned by document; a row that moved left them behind (#1202).
/// They are `grid_rows.supplied_terms` now.
pub(crate) const RETIRED_TABLES: &[&str] = &["DROP TABLE IF EXISTS supplied_search_terms"];

/// The grid index's tables, and the terms' with their indexes.
fn index_ddl() -> impl Iterator<Item = &'static str> {
    GRID_ROWS_DDL
        .iter()
        .map(|(_table, ddl)| *ddl)
        .chain(std::iter::once(MARKDOWNS_DDL))
        .chain(EDGES_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(SOURCE_CONTACTS_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(SOURCE_CONTACT_HANDLES_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(SOURCE_PHOTOS_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(PROBLEMS_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(SOURCE_CURSORS_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(std::iter::once(datalib_etl::pin::GENERATION_DDL))
        .chain(TERMS_DDL.iter().copied())
}

/// Create whatever of the index is missing: on a new file, all of it;
/// after a build stopped partway, the search indexes it left out. Only
/// here: every render store has a `grid_rows` too, and only the index the
/// grid reads wants to pay for the search indexes on every write.
pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
    let search = GRID_ROWS_INDEXES.iter().map(|(_table, ddl)| *ddl);
    for ddl in index_ddl()
        .chain(search)
        .chain(DOCUMENT_LOOKUP_INDEXES.iter().copied())
    {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .with_context(|| format!("create: {ddl}"))?;
    }
    Ok(())
}

/// The search bar's indexes are left out of a build into an empty index
/// and made once at its end, and remade around a source's rewrite of a
/// large share of the index. Kept up row by row, each changed row costs
/// a page in every one of them, more pages than SQLite's cache holds;
/// built at the end they are sorted and contiguous.
async fn drop_search_indexes(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    for (_table, ddl) in GRID_ROWS_INDEXES {
        let name = index_name(ddl).with_context(|| format!("no index name in {ddl}"))?;
        // Audited: `name` is parsed out of our own static index DDL.
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP INDEX IF EXISTS {name}")))
            .execute(&mut *conn)
            .await
            .with_context(|| format!("drop {name}"))?;
    }
    Ok(())
}

async fn create_search_indexes(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    for (_table, ddl) in GRID_ROWS_INDEXES {
        sqlx::query(*ddl)
            .execute(&mut *conn)
            .await
            .with_context(|| format!("create index: {ddl}"))?;
    }
    Ok(())
}

/// Whether the rest of a source's load should drop the search indexes
/// and remake them at its end: when the rows its first `applied` of
/// `applying` documents changed, projected over all of them, come to a
/// sixteenth of the rows the index holds. On an 800k-row index remaking
/// the indexes cost about what keeping them up cost for a fifteenth.
fn remakes_search_indexes(changed: usize, applied: usize, applying: usize, held_rows: i64) -> bool {
    let projected = changed as u128 * applying as u128;
    changed > 0 && projected * 16 >= held_rows.max(0) as u128 * applied as u128
}

/// Where a load decides about the search indexes: how many rows the
/// index held when it began.
struct RemakeProbe {
    held_rows: i64,
}

impl RemakeProbe {
    async fn start(write_lock: &WriteLock) -> Result<Self> {
        let mut guard = write_lock.acquire().await?;
        let held_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM grid_rows")
            .fetch_one(&mut **guard.conn())
            .await
            .context("measure the index before a large load")?;
        Ok(Self { held_rows })
    }

    /// Drop the search indexes if the load so far says to; whether it did.
    async fn decide(
        &self,
        write_lock: &WriteLock,
        applied: usize,
        applying: usize,
    ) -> Result<bool> {
        let changed = write_lock.changed_rows();
        if !remakes_search_indexes(changed, applied, applying, self.held_rows) {
            return Ok(false);
        }
        let mut guard = write_lock.acquire().await?;
        let conn = guard.conn();
        // Inside the source's transaction, so a reader keeps the indexed
        // state it had until the remade one commits.
        drop_search_indexes(conn).await?;
        // The dropped indexes were the last thing made, at the end of the
        // file; handing their pages back puts the remade ones at the end
        // again, in order. Left on the free list, the rest of the load and
        // the remake take them back in whatever order the list holds,
        // and every scan of a remade index afterwards reads a scattered
        // file.
        sqlx::query("PRAGMA incremental_vacuum")
            .execute(&mut **conn)
            .await
            .context("hand back the dropped indexes' pages")?;
        tracing::info!(
            applied,
            applying,
            changed,
            held_rows = self.held_rows,
            "this source changes much of the index; remaking its search indexes after it"
        );
        Ok(true)
    }
}

fn index_name(ddl: &str) -> Option<&str> {
    ddl.strip_prefix("CREATE INDEX IF NOT EXISTS ")?
        .split_whitespace()
        .next()
}

/// The `grid_index` step's handle on the index: the one way to open it
/// for writing. One connection, never recycled, as every pool here
/// (`etl/README.md` § "Connection pools"). A reader that looks before the
/// generation row is made reads the index as not built yet.
///
/// An index written in another shape is deleted and made new: every row
/// in it is a function of a render store, so a rebuild costs one local
/// read of them, where altering it in place would leave a new column
/// empty in the rows the cursors never revisit.
pub async fn open_index(db_path: &Path) -> Result<SqlitePool> {
    retire_old_files(db_path)?;
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut pool = connect(db_path).await?;
    match datalib_store_meta::read(&pool).await {
        Ok(Some(meta)) if meta.schema_hash != schema_hash() => {
            pool.close().await;
            tracing::info!(
                path = %db_path.display(),
                "the grid index was written in another shape; building it again"
            );
            remove_index_files(db_path)?;
            pool = connect(db_path).await?;
        }
        Ok(_) => {}
        Err(e) => {
            pool.close().await;
            return Err(e).context("read the grid index's shape");
        }
    }
    match set_up(&pool).await {
        Ok(()) => Ok(pool),
        Err(e) => {
            pool.close().await;
            Err(e)
        }
    }
}

async fn connect(db_path: &Path) -> Result<SqlitePool> {
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(datalib_core::plain_sqlite::uri(db_path))
        .create_if_missing(true)
        // A pass holds its transaction for as long as one source takes;
        // WAL lets the search applet read the last one meanwhile.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        // So the pages of dropped search indexes can be handed back
        // (`RemakeProbe::decide`). Takes effect only on a new file.
        .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
        // SQLite journals each statement that can stop halfway — an
        // upsert, a delete of several rows — so it can undo that one
        // statement; in a temp file that was most of a load's time.
        .pragma("temp_store", "memory")
        .busy_timeout(Duration::from_secs(30));
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open the grid index at {}", db_path.display()))
}

async fn set_up(pool: &SqlitePool) -> Result<()> {
    init_schema(pool).await?;
    // Its `problems` is built in this shape, so it starts at the shared
    // ladder's top.
    let versions = datalib_store_meta::Versions {
        schema: 0,
        shared: datalib_store_meta::ladder::top(datalib_etl::doltlite_raw::SHARED_LADDER),
    };
    datalib_store_meta::write(pool, StoreKind::GridIndex, &schema_hash(), versions)
        .await
        .context("write _datalib_meta for the grid index")?;
    sqlx::query(
        "INSERT INTO store_generation (id, file_id, generation) \
         VALUES (1, lower(hex(randomblob(8))), 0) ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .context("start the grid index's generation")?;
    Ok(())
}

/// The index file and its log. A `-wal` left beside a new file would be
/// read into it.
fn remove_index_files(db_path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", db_path.display()));
        match fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            other => other.with_context(|| format!("remove {}", path.display()))?,
        }
    }
    Ok(())
}

/// Delete what an older build kept beside the index: the doltlite store it
/// used to be, and the search terms file the index now holds. Both are
/// derived, and the doltlite store was the bulk of an upgraded root's disk.
fn retire_old_files(db_path: &Path) -> Result<()> {
    let Some(dir) = db_path.parent() else {
        return Ok(());
    };
    let (store, terms) = (
        datalib_core::layout::RETIRED_GRID_DB,
        datalib_core::layout::RETIRED_TERMS_DB,
    );
    for name in [
        store.to_string(),
        format!("{store}.lock"),
        format!(".{store}-lock"),
        terms.to_string(),
        format!("{terms}-wal"),
        format!("{terms}-shm"),
    ] {
        let path = dir.join(&name);
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        tracing::info!(
            path = %path.display(),
            bytes = meta.len(),
            "removed a file an older build kept beside the grid index"
        );
    }
    Ok(())
}

/// Copy the WAL back into the index and empty it, then close. Doltlite,
/// which our binaries link, never does this by itself
/// (`docs/dev/doltlite.md`), and while the search applet holds the file
/// open, closing the writer's connection does not either: the `-wal`
/// would keep every page any pass wrote.
pub async fn close_index(pool: SqlitePool) -> Result<()> {
    let checkpoint: Result<(i64, i64, i64), sqlx::Error> =
        sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&pool)
            .await;
    pool.close().await;
    match checkpoint.context("checkpoint the grid index's WAL")? {
        (0, _, _) => {}
        (_, log, copied) => tracing::warn!(
            log,
            copied,
            "a reader held the grid index's WAL past the busy timeout; the next pass empties it"
        ),
    }
    Ok(())
}

/// Raise the index's generation inside the transaction on `conn`, which
/// changed what a reader sees, and return it. This is the index's seal:
/// what a doltlite commit was.
async fn bump_generation(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) -> Result<i64> {
    sqlx::query_scalar(
        "UPDATE store_generation SET generation = generation + 1 WHERE id = 1 \
         RETURNING generation",
    )
    .fetch_one(&mut **conn)
    .await
    .context("raise the grid index's generation")
}

/// Write the terms of what `write_lock` changed outside a pass — with
/// [`apply_one`] and [`delete_markdown`] outside a transaction, as tests
/// and dev tools do — and raise the generation, so readers see it moved.
pub async fn seal(write_lock: &WriteLock) -> Result<i64> {
    let mut guard = write_lock.acquire().await?;
    let conn = guard.conn();
    crate::search_terms::write_rows(conn, &write_lock.take_changed()).await?;
    crate::search_terms::write_people(conn).await?;
    bump_generation(conn).await
}

/// Where the index is now, as its step reports it.
pub async fn version(pool: &SqlitePool) -> Result<Option<String>> {
    datalib_etl::pin::generation(pool)
        .await
        .context("read the grid index's generation")
}

/// The index side of `markdowns.renderer_version` (`"<index>.<render>"`).
/// Bump when the rendered `.md` layout changes for every provider at
/// once: every document's version then differs from its store's.
pub const RENDERER_VERSION: &str = "rust-v1";

/// Map a grid_rows display `kind` to the `documents.kind` enum. Anything
/// unlisted is a child row and shouldn't be the canonical document row, but
/// falls back to `"chat"` if it is the only candidate.
fn doc_kind_for(grid_kind: &str) -> &'static str {
    match grid_kind {
        "Chat" => "chat",
        "Slack Thread" => "thread",
        "GitHub PR" => "pr",
        "GitLab MR" => "mr",
        "Notion Page" | "Notion Database" => "page",
        "Notion Comment Thread" => "thread",
        // A PDF is a document, not a conversation, and the sync page shows
        // this string.
        "PDF Document" => "document",
        // Likewise a Claude Project: written context, not a conversation.
        "Project" => "document",
        // The per-source storage report: datalib describing a mirror,
        // not anything the upstream authored.
        "Source Size" => datalib_schema::measurements::DOC_KIND,
        "Photo" | "Video" => "photo",
        _ => "chat",
    }
}

/// One markdown's payload as handed from render to the indexer,
/// constructed once the md and its blobs are durably on disk so that render
/// and index commit per-document atomically.
#[derive(Debug, Clone)]
pub struct RenderedMarkdown {
    pub markdown_uuid: String,
    /// User-facing config name (e.g. `tiny-slack`), falling back to the
    /// provider string.
    pub source_id: String,
    /// A cheap probe the orchestrator can check *before* loading payloads to
    /// decide whether a markdown moved. Slack stamps each thread's
    /// `MAX(fetched_at_utc)`. None when the provider has nothing cheap.
    pub upstream_cursor: Option<String>,
    /// The bucket this document was rendered from — the same key the
    /// provider declares through `RenderCtx::declare_bucket`. `None`
    /// from a renderer that does not declare buckets yet.
    pub bucket_key: Option<String>,
    /// Absolute path to the rendered `.md`; `qmd_path` is this with the
    /// out-dir prefix stripped.
    pub md_path: PathBuf,
    pub render_version: u32,
    /// Empty means there is no document: the store drops it, `.md` and
    /// all, keeping only its `problems`.
    pub rows: Vec<GridRow>,
    /// The document piece by piece, in order, each piece keyed by the
    /// `data-section-uuid` it wraps or unkeyed when it wraps none —
    /// concatenated they are the `.md`'s bytes. Empty from a renderer
    /// that has not been taught sections, and when read back from the
    /// store, which never holds the markdown.
    pub sections: Vec<Section>,
    /// Outgoing edges (`src_markdown_uuid == markdown_uuid`). Empty for
    /// renderers that don't emit edges; the DELETE still runs, so stale rows
    /// from a previous render get cleaned up.
    pub edges: Vec<EdgeRow>,
    /// The people this document describes or mentions, as its source
    /// describes them; owned by the document like its edges.
    pub contacts: Vec<NormalizedContact>,
    /// The photos or videos this document is, as its source describes
    /// them; owned by the document like its contacts.
    pub photos: Vec<NormalizedPhoto>,
    /// What render could not do while producing this document: records
    /// dropped, fields nulled, lossy rules that fired. Travels with the
    /// document so the rows and the record of what was lost commit together.
    /// Empty when read back from the store, where they are already rows.
    pub problems: Vec<datalib_schema::problems::ProblemRow>,
}

/// Bumped when [`doc_md_path`] files documents somewhere else; every
/// source carries it in its render params, so a bump renders them all
/// again and each moved file's old copy is deleted as its document is
/// written.
pub const DOC_LAYOUT: u32 = 1;

/// Where a document's markdown goes: [`doc_file_rel`] under its source's
/// `render_markdown/`, so a search hit's path names its document.
///
/// [`doc_file_rel`]: datalib_schema::grid_rows::doc_file_rel
pub fn doc_md_path(render_root: &Path, markdown_uuid: &str) -> PathBuf {
    render_root.join(datalib_schema::grid_rows::doc_file_rel(markdown_uuid))
}

/// Why a document cannot be indexed where it was written: a file not
/// named for its id, or a row naming another file. `None` when it can.
fn misfiled(md: &RenderedMarkdown, qmd_path: &str) -> Option<String> {
    use datalib_schema::grid_rows::markdown_uuid_of_path;
    if markdown_uuid_of_path(qmd_path) != Some(md.markdown_uuid.as_str()) {
        return Some(format!(
            "document {} is written to {qmd_path}, not to the file its id names \
             (`datalib_etl_render::grid_index::doc_md_path`)",
            md.markdown_uuid
        ));
    }
    md.rows
        .iter()
        .find(|r| r.qmd_path.as_deref() != Some(qmd_path))
        .map(|r| {
            format!(
                "row {} of document {} names {:?} as its file, not {qmd_path}",
                r.uuid, md.markdown_uuid, r.qmd_path
            )
        })
}

/// Write one rendered document into the index unconditionally. `out_dir`
/// is stripped off `md_path` to produce a portable `qmd_path`.
pub async fn apply_one(
    write_lock: &WriteLock,
    out_dir: &Path,
    md: &RenderedMarkdown,
) -> Result<usize> {
    let qmd_rel = md
        .md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md.md_path)
        .to_string_lossy()
        .to_string();
    apply_markdown(write_lock, md, &qmd_rel).await
}

/// Every source under the data root with a render store: the directory
/// name is the source's id. This is the dev tools' answer to "which
/// sources"; the step's answer is the graph, see [`build_grid_index_for`].
pub fn discover_sources(out_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            if entry.file_name() == datalib_core::layout::SYSTEM_DIR {
                continue;
            }
            let rendered_root = entry.path().join(datalib_etl::layout::RENDER_MARKDOWN_DIR);
            if crate::indexed_markdown::path_for(&rendered_root).is_file() {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

pub async fn build_grid_index(
    pool: &SqlitePool,
    out_dir: &Path,
    progress: impl Fn(&str),
    now_override: Option<&str>,
) -> Result<GridIndexSummary> {
    let sources = discover_sources(out_dir);
    build_grid_index_for(
        pool,
        out_dir,
        &sources,
        progress,
        now_override,
        &StopFlag::new(),
    )
    .await
}

/// Stack the render stores of exactly `sources` into the index — the
/// `grid_index` DAG step's whole job. The step passes the groups its
/// declared inputs name, so a source dropped from the config stops being
/// read on the next run even while its tree is still on disk, and its
/// documents leave the index — and a directory that is not in the config
/// is never read at all. A listed
/// source with no store yet is skipped: its render step has not produced
/// one.
///
/// **Each source is asked what changed, not read whole**, via `dolt_diff`
/// between the commit `source_cursors` last consumed and the store's HEAD,
/// so a document a source stopped holding can be named and deleted.
///
/// **Each source is its own transaction and its own seal**, its cursor
/// advancing inside it: a stop, a crash or a failure keeps every source
/// already indexed, and costs only the one in flight, which the next pass
/// reads again from the cursor it left.
pub async fn build_grid_index_for(
    pool: &SqlitePool,
    out_dir: &Path,
    sources: &[String],
    progress: impl Fn(&str),
    now_override: Option<&str>,
    stop: &StopFlag,
) -> Result<GridIndexSummary> {
    let write_lock = WriteLock::for_index(pool.clone());
    // Before any transaction opens, because the index pool is one
    // connection wide.
    let cursors = load_source_cursors(pool).await?;
    let indexed = load_markdown_uuids_by_source(pool).await?;
    let now = run_stamp(now_override);
    // A pass stopped before the end leaves them to the next open, whose
    // `init_schema` makes them.
    let building = indexed.is_empty();
    if building {
        drop_search_indexes(&mut *pool.acquire().await?).await?;
    }
    let held: usize = indexed.values().map(HashSet::len).sum();

    let mut stanzas: Vec<(String, PathBuf)> = Vec::new();
    let mut pass = PassCounts::default();
    let mut not_yet_rendered: Vec<&str> = Vec::new();
    for source in sources {
        let rendered_root = out_dir
            .join(source)
            .join(datalib_etl::layout::RENDER_MARKDOWN_DIR);
        if crate::indexed_markdown::path_for(&rendered_root).is_file() {
            stanzas.push((source.clone(), rendered_root));
        } else {
            pass.not_yet_rendered += 1;
            not_yet_rendered.push(source);
        }
    }
    // One line a pass: a fan-in takes a pass each time a render seals,
    // and a line per waiting source each time drowns the step's log.
    if !not_yet_rendered.is_empty() {
        tracing::debug!(
            sources = %not_yet_rendered.join(", "),
            "no render store yet for these sources; skipping them this pass"
        );
    }
    stanzas.sort();
    stanzas.dedup();

    let mut summary = GridIndexSummary::default();
    for (stanza, rendered_root) in stanzas {
        if stopped_before(stop, &stanza) {
            break;
        }
        let read = match read_source(
            &stanza,
            &rendered_root,
            out_dir,
            cursors.get(&stanza),
            indexed.get(&stanza),
            &mut pass,
        ) {
            Ok(Some(read)) => read,
            Ok(None) => continue,
            // One source's store must not keep every other source out of
            // the index.
            Err(e) => {
                let why = format!("{e:#}");
                tracing::error!(
                    source = %stanza,
                    error = %why,
                    "could not read this render store; indexing the other \
                     sources and leaving this one as it was"
                );
                SourceRead::Failed(why)
            }
        };
        if stopped_before(stop, &stanza) {
            break;
        }
        let applied = apply_source(
            &write_lock,
            out_dir,
            &stanza,
            read,
            (!building).then_some(held),
            &now,
            &progress,
            stop,
            &mut summary,
        )
        .await?;
        if !applied {
            tracing::info!(
                source = %stanza,
                "asked to stop; this source's changes are rolled back and the \
                 next pass reads them again"
            );
            break;
        }
    }
    if !stop.requested() {
        let named: HashSet<&str> = sources.iter().map(String::as_str).collect();
        let mut unnamed: Vec<&str> = indexed
            .keys()
            .chain(cursors.keys())
            .map(String::as_str)
            .filter(|s| !named.contains(s))
            .collect();
        unnamed.sort_unstable();
        unnamed.dedup();
        for source in unnamed {
            let documents = indexed.get(source).map_or(0, HashSet::len);
            retire_source(&write_lock, source, indexed.get(source)).await?;
            summary.markdowns_removed += documents;
            summary.seals += 1;
            tracing::info!(
                source,
                documents,
                "this source is no longer an input; its documents are out of the index"
            );
        }
    }
    if building && !stop.requested() {
        let started = Instant::now();
        create_search_indexes(&mut *pool.acquire().await?).await?;
        tracing::info!(
            took_ms = started.elapsed().as_millis() as u64,
            "built the search indexes over the new index"
        );
    }
    if summary.seals > 0 {
        refresh_people(&write_lock).await?;
    }
    tracing::debug!(
        sources = sources.len(),
        not_yet_rendered = pass.not_yet_rendered,
        read_whole = pass.read_whole,
        sources_changed = pass.sources_changed,
        documents_changed = pass.documents_changed,
        unreadable = summary.sources_unreadable.len(),
        failed = summary.sources_failed.len(),
        "read the render stores"
    );
    Ok(summary)
}

/// Take a source the graph no longer names out of the index, in one
/// transaction and one seal: its documents, its problems and its cursor,
/// so a source added back later is read whole.
async fn retire_source(
    write_lock: &WriteLock,
    source: &str,
    documents: Option<&HashSet<String>>,
) -> Result<()> {
    write_lock
        .begin_transaction()
        .await
        .with_context(|| format!("begin the transaction retiring {source}"))?;
    let res = async {
        let mut guard = write_lock.acquire().await?;
        let conn = guard.conn();
        let mut gone = Vec::new();
        for markdown_uuid in documents.into_iter().flatten() {
            gone.extend(delete_document_rows(conn, markdown_uuid).await?);
        }
        crate::search_terms::write_rows(conn, &gone).await?;
        replace_source_problems(conn, source, &[]).await?;
        sqlx::query("DELETE FROM source_cursors WHERE source_id = ?")
            .bind(source)
            .execute(&mut **conn)
            .await
            .with_context(|| format!("clear {source}'s cursor"))?;
        bump_generation(conn).await
    }
    .await;
    let generation = match res {
        Ok(generation) => generation,
        Err(e) => {
            let _ = write_lock.rollback_transaction().await;
            return Err(e);
        }
    };
    write_lock
        .commit_transaction()
        .await
        .with_context(|| format!("commit the transaction retiring {source}"))?;
    let removed = documents.map_or(0, HashSet::len);
    tracing::debug!(
        source,
        generation,
        counts = %SealCounts { read: 0, loaded: 0, removed, rows: 0 }.message(source),
        "sealed the index"
    );
    Ok(())
}

/// The people the terms name, read from every source at once: once a
/// pass that changed anything, in a seal of its own.
async fn refresh_people(write_lock: &WriteLock) -> Result<()> {
    write_lock.begin_transaction().await?;
    let res = async {
        let mut guard = write_lock.acquire().await?;
        crate::search_terms::write_people(guard.conn()).await?;
        bump_generation(guard.conn()).await
    }
    .await;
    match res {
        Ok(_) => write_lock.commit_transaction().await,
        Err(e) => {
            let _ = write_lock.rollback_transaction().await;
            Err(e).context("write the names the terms know people by")
        }
    }
}

fn stopped_before(stop: &StopFlag, source: &str) -> bool {
    if stop.requested() {
        tracing::info!(
            source,
            "asked to stop; leaving this source for the next pass"
        );
    }
    stop.requested()
}

/// One line per pass says what the pass found; the per-source lines are
/// `debug` unless a source moved, because a streaming pass runs on every
/// producer checkpoint and most sources moved on none.
#[derive(Default)]
struct PassCounts {
    not_yet_rendered: usize,
    read_whole: usize,
    sources_changed: usize,
    documents_changed: usize,
}

/// What one source's render store holds for the index this pass.
enum SourceRead {
    /// Written in a shape this build cannot read. Its rows and cursor stay
    /// as the index has them.
    Unreadable,
    /// The read failed; the error, for the source's problem row. Its rows
    /// and cursor stay as the index has them.
    Failed(String),
    Read {
        docs: Vec<RenderedMarkdown>,
        /// Documents the index holds for this source that the store no
        /// longer does.
        removed: Vec<String>,
        /// Its problems at the pin. The copy is wholesale: the pinned store
        /// is the complete truth about that source's problems.
        problems: Vec<ProblemRow>,
        /// The commit read and its tree. `None` when `dolt_log()` did not
        /// answer, and an unwritten cursor cold-starts the next pass — the
        /// safe direction.
        head: Option<(String, String)>,
    },
}

/// `None` when the store names no commit, so there is nothing committed
/// to index.
fn read_source(
    stanza: &str,
    rendered_root: &Path,
    out_dir: &Path,
    cursor: Option<&Cursor>,
    indexed: Option<&HashSet<String>>,
    pass: &mut PassCounts,
) -> Result<Option<SourceRead>> {
    // Read-only: the render step owns this store, and an ordinary open
    // would discard the renderer's in-flight rows and schema-commit into
    // it — writing to a file we do not own. Pinned at open: the diff below
    // and the rows behind it name one commit.
    let Some(store) =
        crate::indexed_markdown::IndexedMarkdownStore::open_for_reading(rendered_root, None)
            .with_context(|| format!("open render store for {stanza}"))?
    else {
        tracing::warn!(
            source = %stanza,
            "this render store names no commit, so there is nothing \
             committed to index; skipping it this pass"
        );
        return Ok(None);
    };
    let read = read_open_store(&store, stanza, out_dir, cursor, indexed, pass);
    store.close();
    match read {
        Err(e) if written_in_another_shape(&e) => {
            tracing::warn!(
                source = %stanza,
                error = %format!("{e:#}"),
                "this render store lacks a table or column this build reads; \
                 indexing the other sources and leaving this one as it was \
                 until it re-renders"
            );
            Ok(Some(SourceRead::Unreadable))
        }
        read => read.map(Some),
    }
}

fn read_open_store(
    store: &crate::indexed_markdown::IndexedMarkdownStore,
    stanza: &str,
    out_dir: &Path,
    cursor: Option<&Cursor>,
    indexed: Option<&HashSet<String>>,
    pass: &mut PassCounts,
) -> Result<SourceRead> {
    if !store
        .in_this_shape()
        .with_context(|| format!("read the shape of {stanza}'s render store"))?
    {
        tracing::debug!(
            source = %stanza,
            "this render store is in a shape this build does not read"
        );
        return Ok(SourceRead::Unreadable);
    }
    let pin = store.pin().expect("a reader is pinned at open").clone();
    let from = match cursor {
        Some(c) => store
            .resolve_commit(&c.commit, c.tree.as_deref())
            .with_context(|| format!("resolve {stanza}'s cursor"))?
            // Neither is there: the diff below fails on it and says so.
            .or_else(|| Some(c.commit.clone())),
        None => None,
    };
    let cursor = from.as_deref();
    let scan = store
        .changed_since(cursor, &pin)
        .with_context(|| format!("diff render store for {stanza}"))?;
    // Say which path was taken: a cold start that fires silently on every
    // run looks exactly like a fast one from the outside — it just does
    // more work and still gets the right answer.
    match (&scan.render, cursor) {
        (None, None) => {
            pass.read_whole += 1;
            tracing::info!(
                source = %stanza,
                "no cursor for this source; reading its whole render store"
            )
        }
        (None, Some(from)) => {
            pass.read_whole += 1;
            tracing::warn!(
                source = %stanza,
                from,
                "cursor unusable against this render store (reset, rebuilt, or \
                 no dolt_diff); falling back to reading it whole"
            )
        }
        (Some(changed), _) if changed.is_empty() => tracing::debug!(
            source = %stanza,
            scan_ms = scan.scan_elapsed.map(|d| d.as_millis() as u64),
            "no documents changed since the last index"
        ),
        (Some(changed), _) => {
            pass.sources_changed += 1;
            pass.documents_changed += changed.len();
            tracing::info!(
                source = %stanza,
                changed = changed.len(),
                scan_ms = scan.scan_elapsed.map(|d| d.as_millis() as u64),
                "documents changed since the last index"
            )
        }
    }
    let docs = store
        .documents_matching(out_dir, scan.render.as_ref(), &pin)
        .with_context(|| format!("read documents from {stanza}"))?;
    let problems = store
        .problems_at_pin()
        .with_context(|| format!("read problems from {stanza}"))?;
    let present: HashSet<&str> = docs.iter().map(|d| d.markdown_uuid.as_str()).collect();
    let removed = match &scan.render {
        // An id the diff named that the store no longer has is a deletion.
        Some(changed) => changed
            .iter()
            .filter(|u| !present.contains(u.as_str()))
            .cloned()
            .collect(),
        // The store was read whole, so it is the complete answer: a
        // document the index holds for this source that the store does not
        // is one the source no longer produces. A committed store with no
        // rows is an honest "nothing"; the store that could not be read
        // returned above.
        None => indexed
            .into_iter()
            .flatten()
            .filter(|u| !present.contains(u.as_str()))
            .cloned()
            .collect(),
    };
    let head = match scan.new_head {
        Some(commit) => {
            let tree = store
                .tree_of(&commit)
                .with_context(|| format!("read the tree of {stanza}'s render store"))?;
            Some((commit, tree))
        }
        None => None,
    };
    Ok(SourceRead::Read {
        docs,
        removed,
        problems,
        head,
    })
}

/// Write one source's changes, problems and cursor in one transaction
/// and seal it. `false` when the stop came first: the transaction is
/// rolled back and nothing of this source is written. `held` is how many
/// documents the index held when the pass began, `None` in a build that
/// makes the search indexes at its end.
#[allow(clippy::too_many_arguments)]
async fn apply_source(
    write_lock: &WriteLock,
    out_dir: &Path,
    stanza: &str,
    read: SourceRead,
    held: Option<usize>,
    now: &datalib_time::StoredStamp,
    progress: &impl Fn(&str),
    stop: &StopFlag,
    summary: &mut GridIndexSummary,
) -> Result<bool> {
    let before = SealCounts::of(summary);
    write_lock
        .begin_transaction()
        .await
        .with_context(|| format!("begin the index transaction for {stanza}"))?;
    let res = async {
        let (docs, removed, problems, head) = match read {
            SourceRead::Unreadable => {
                let row = unreadable_store_problem(stanza, OLDER_SHAPE, Severity::Warning);
                let mut guard = write_lock.acquire().await?;
                record_unreadable_store(guard.conn(), row, now).await?;
                summary.sources_unreadable.push(stanza.to_string());
                return Ok(Some(true));
            }
            SourceRead::Failed(why) => {
                let row = unreadable_store_problem(stanza, &why, Severity::Error);
                let mut guard = write_lock.acquire().await?;
                record_unreadable_store(guard.conn(), row, now).await?;
                summary.sources_failed.push((stanza.to_string(), why));
                return Ok(Some(true));
            }
            SourceRead::Read {
                docs,
                removed,
                problems,
                head,
            } => (docs, removed, problems, head),
        };
        summary.markdowns_total += docs.len();
        // Too few documents to change a sixteenth of the index, whatever
        // they hold, are not worth measuring.
        let probe = match held {
            Some(held) if docs.len() * 16 >= held && docs.len() > 1 => {
                Some(RemakeProbe::start(write_lock).await?)
            }
            _ => None,
        };
        for gone in &removed {
            delete_markdown(write_lock, gone)
                .await
                .with_context(|| format!("delete {gone} dropped by {stanza}"))?;
            summary.markdowns_removed += 1;
        }
        let loaded = load_source(
            write_lock,
            out_dir,
            stanza,
            &docs,
            probe.as_ref(),
            progress,
            stop,
        )
        .await?;
        let Some(Loaded {
            markdowns,
            rows,
            remade,
        }) = loaded
        else {
            return Ok(None);
        };
        summary.markdowns_loaded += markdowns;
        summary.rows_inserted += rows;
        let mut guard = write_lock.acquire().await?;
        let conn = guard.conn();
        if remade {
            let started = Instant::now();
            create_search_indexes(conn).await?;
            tracing::info!(
                source = %stanza,
                took_ms = started.elapsed().as_millis() as u64,
                "remade the search indexes"
            );
        }
        summary.terms_written +=
            crate::search_terms::write_rows(conn, &write_lock.take_changed()).await?;
        let problems_moved = replace_source_problems(conn, stanza, &problems)
            .await
            .with_context(|| format!("copy {stanza}'s problems into the index"))?;
        summary.problems_copied += problems.len();
        // Last and in the same transaction: a failure above rolls back to
        // both the old rows and the old cursor.
        if let Some((store_commit, store_tree)) = head {
            write_source_cursor(
                conn,
                &SourceCursorRow {
                    source_id: stanza.to_string(),
                    store_commit,
                    store_tree: Some(store_tree),
                    indexed_at_utc: now.utc.clone(),
                    tz_offset: now.tz_offset.clone(),
                    documents_applied: markdowns as i64,
                },
            )
            .await?;
        }
        Ok::<Option<bool>, anyhow::Error>(Some(
            !docs.is_empty() || !removed.is_empty() || problems_moved,
        ))
    }
    .await;
    let res = match res {
        Ok(Some(true)) => {
            let mut guard = write_lock.acquire().await?;
            bump_generation(guard.conn()).await.map(|g| Some(Some(g)))
        }
        Ok(Some(false)) => Ok(Some(None)),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    match res {
        Ok(Some(generation)) => {
            write_lock
                .commit_transaction()
                .await
                .with_context(|| format!("commit the index transaction for {stanza}"))?;
            if let Some(generation) = generation {
                summary.seals += 1;
                let counts = SealCounts::of(summary).since(&before).message(stanza);
                tracing::debug!(source = %stanza, generation, %counts, "sealed the index");
            }
            Ok(true)
        }
        Ok(None) => {
            write_lock.rollback_transaction().await?;
            Ok(false)
        }
        Err(e) => {
            // Best effort — the held connection rolls back on drop anyway.
            let _ = write_lock.rollback_transaction().await;
            Err(e)
        }
    }
}

/// The summary's counters at one moment, so a seal can say what its own
/// source added.
#[derive(Clone, Copy)]
struct SealCounts {
    read: usize,
    loaded: usize,
    removed: usize,
    rows: usize,
}

impl SealCounts {
    fn of(s: &GridIndexSummary) -> Self {
        Self {
            read: s.markdowns_total,
            loaded: s.markdowns_loaded,
            removed: s.markdowns_removed,
            rows: s.rows_inserted,
        }
    }

    fn since(self, before: &Self) -> Self {
        Self {
            read: self.read - before.read,
            loaded: self.loaded - before.loaded,
            removed: self.removed - before.removed,
            rows: self.rows - before.rows,
        }
    }

    fn message(self, source: &str) -> String {
        format!(
            "datalib-step grid_index: source={source} markdowns_read={} \
             markdowns_loaded={} markdowns_removed={} rows_inserted={}",
            self.read, self.loaded, self.removed, self.rows
        )
    }
}

struct Loaded {
    markdowns: usize,
    rows: usize,
    /// The search indexes were dropped partway, to be made again.
    remade: bool,
}

/// Apply one source's documents. `None` when the stop came first. With
/// a `probe`, a twentieth of the way in it decides whether the rest is
/// written without the search indexes.
async fn load_source(
    write_lock: &WriteLock,
    out_dir: &Path,
    stanza: &str,
    docs: &[RenderedMarkdown],
    probe: Option<&RemakeProbe>,
    progress: &impl Fn(&str),
    stop: &StopFlag,
) -> Result<Option<Loaded>> {
    let mut rows = 0;
    let mut remade = false;
    let probe_after = (docs.len() / 20).max(1);
    for (i, md) in docs.iter().enumerate() {
        if stop.requested() {
            return Ok(None);
        }
        // The stanza name is authoritative. Everything else comes through
        // from the store unchanged: every document the diff named is
        // applied, and the rows of it that differ are written.
        let md = RenderedMarkdown {
            source_id: stanza.to_string(),
            // Already rows in the store; re-applying would double-count.
            problems: Vec::new(),
            ..md.clone()
        };
        rows += apply_one(write_lock, out_dir, &md)
            .await
            .with_context(|| format!("load {} from {stanza}", md.markdown_uuid))?;
        progress(&format!("{stanza}: loaded {}/{}", i + 1, docs.len()));
        if let Some(probe) = probe.filter(|_| i + 1 == probe_after) {
            remade = probe.decide(write_lock, i + 1, docs.len()).await?;
        }
    }
    Ok(Some(Loaded {
        markdowns: docs.len(),
        rows,
        remade,
    }))
}

async fn load_markdown_uuids_by_source(
    pool: &SqlitePool,
) -> Result<HashMap<String, HashSet<String>>> {
    let rows = sqlx::query("SELECT source_id, markdown_uuid FROM markdowns")
        .fetch_all(pool)
        .await
        .context("load_markdown_uuids_by_source")?;
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for r in rows {
        out.entry(r.try_get("source_id")?)
            .or_default()
            .insert(r.try_get("markdown_uuid")?);
    }
    Ok(out)
}

/// Where the index left one source's render store: the commit it read,
/// and that commit's tree for when a fold has replaced the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub commit: String,
    pub tree: Option<String>,
}

pub async fn load_source_cursors(pool: &SqlitePool) -> Result<HashMap<String, Cursor>> {
    let rows = sqlx::query("SELECT source_id, store_commit, store_tree FROM source_cursors")
        .fetch_all(pool)
        .await
        .context("load_source_cursors")?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in rows {
        out.insert(
            r.try_get("source_id")?,
            Cursor {
                commit: r.try_get("store_commit")?,
                tree: r.try_get("store_tree")?,
            },
        );
    }
    Ok(out)
}

async fn write_source_cursor(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    row: &SourceCursorRow,
) -> Result<()> {
    sqlx::query("DELETE FROM source_cursors WHERE source_id = ?")
        .bind(&row.source_id)
        .execute(&mut **conn)
        .await
        .context("clear prior source cursor")?;
    let sql = datalib_etl::bulk::insert_sql::<SourceCursorRow>();
    // Audited: `sql` is built from `SourceCursorRow`'s associated consts.
    row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
        .execute(&mut **conn)
        .await
        .with_context(|| format!("write source cursor {}", row.source_id))?;
    Ok(())
}

/// Remove a document and everything hanging off it: the three deletes
/// `apply_markdown` runs before re-inserting, without the insert. Only
/// reachable because the index diffs rather than re-reads — a deleted
/// conversation used to stay in the grid forever.
pub async fn delete_markdown(write_lock: &WriteLock, markdown_uuid: &str) -> Result<()> {
    let mut guard = write_lock.acquire().await?;
    let gone = delete_document_rows(guard.conn(), markdown_uuid).await?;
    write_lock.note_changed(gone);
    Ok(())
}

/// The rows a document owns: its grid rows, its outgoing edges and its
/// `markdowns` row. Not its `problems`, which say why a document
/// is the way it is and outlive one that ends with nothing. Returns the
/// grid rows it deleted.
pub(crate) async fn delete_document_rows(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    markdown_uuid: &str,
) -> Result<Vec<String>> {
    let gone = sqlx::query_scalar("DELETE FROM grid_rows WHERE markdown_uuid = ? RETURNING uuid")
        .bind(markdown_uuid)
        .fetch_all(&mut **conn)
        .await
        .with_context(|| format!("delete {markdown_uuid}'s rows from the index"))?;
    for sql in [
        "DELETE FROM edges WHERE src_markdown_uuid = ?",
        "DELETE FROM source_contacts WHERE markdown_uuid = ?",
        "DELETE FROM source_contact_handles WHERE markdown_uuid = ?",
        "DELETE FROM source_photos WHERE markdown_uuid = ?",
        "DELETE FROM markdowns WHERE markdown_uuid = ?",
    ] {
        sqlx::query(sql)
            .bind(markdown_uuid)
            .execute(&mut **conn)
            .await
            .with_context(|| format!("delete {markdown_uuid} from the index"))?;
    }
    Ok(gone)
}

pub async fn load_cursors(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query(
        "SELECT markdown_uuid, upstream_cursor \
         FROM markdowns WHERE upstream_cursor IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .context("load_cursors")?;
    let mut out: HashMap<String, String> = HashMap::with_capacity(rows.len());
    for r in rows {
        let uuid: String = r.try_get("markdown_uuid")?;
        let cur: String = r.try_get("upstream_cursor")?;
        out.insert(uuid, cur);
    }
    Ok(out)
}

async fn apply_markdown(
    write_lock: &WriteLock,
    md: &RenderedMarkdown,
    qmd_path: &str,
) -> Result<usize> {
    // Inside `begin_transaction` every guard hands back the same connection,
    // so the per-doc statements accumulate in one batch; otherwise each takes
    // a fresh connection and auto-commits.
    let mut guard = write_lock.acquire().await?;
    let conn = guard.conn();

    // A document is its rows: the grid reaches it through them and
    // nothing else does. One that ends with none — every row rejected
    // by validation — is absent, and its `markdowns` row goes with the
    // rest, or a stale `bucket_key` and title would outlive the render
    // that replaced them. The problems recording why stay.
    if md.rows.is_empty() {
        let gone = delete_document_rows(conn, &md.markdown_uuid).await?;
        write_lock.note_changed(gone);
        tracing::info!(
            document = %md.markdown_uuid,
            "this document has no rows left; dropped it",
        );
        return Ok(0);
    }
    let canonical = document_row(&md.rows, &md.markdown_uuid)?;
    if let Some(why) = misfiled(md, qmd_path) {
        bail!("{why}");
    }

    let uuids: Vec<&str> = md.rows.iter().map(|r| r.uuid.as_str()).collect();
    if let Some(twice) = first_repeated(&uuids) {
        bail!(
            "document {}: two of its rows have the uuid {twice}",
            md.markdown_uuid
        );
    }
    // Only the rows it no longer has; the rest are written in place, and
    // one that did not change is not written at all.
    let gone = sqlx::query_scalar(
        "DELETE FROM grid_rows WHERE markdown_uuid = ? \
         AND uuid NOT IN (SELECT value FROM json_each(?)) RETURNING uuid",
    )
    .bind(&md.markdown_uuid)
    .bind(serde_json::to_string(&uuids)?)
    .fetch_all(&mut **conn)
    .await
    .context("delete the rows this document no longer has")?;
    write_lock.note_changed(gone);

    for row in &md.rows {
        put_grid_row(conn, write_lock, row).await?;
    }

    // Each markdown owns the edges whose `src_markdown_uuid` matches, so a
    // re-render replaces its outgoing set. Incoming edges belong to the other
    // markdown's row set and survive.
    sqlx::query("DELETE FROM edges WHERE src_markdown_uuid = ?")
        .bind(&md.markdown_uuid)
        .execute(&mut **conn)
        .await
        .context("delete prior edges")?;
    for edge in &md.edges {
        insert_edge(conn, edge).await?;
    }
    for sql in [
        "DELETE FROM source_contacts WHERE markdown_uuid = ?",
        "DELETE FROM source_contact_handles WHERE markdown_uuid = ?",
    ] {
        sqlx::query(sql)
            .bind(&md.markdown_uuid)
            .execute(&mut **conn)
            .await
            .context("delete prior source contacts")?;
    }
    for contact in &md.contacts {
        insert_source_contact(conn, &md.markdown_uuid, contact).await?;
    }
    sqlx::query("DELETE FROM source_photos WHERE markdown_uuid = ?")
        .bind(&md.markdown_uuid)
        .execute(&mut **conn)
        .await
        .context("delete prior source photos")?;
    for photo in &md.photos {
        insert_source_photo(conn, &md.markdown_uuid, photo).await?;
    }
    upsert_markdown(conn, md, canonical, qmd_path)
        .await
        .context("upsert markdowns")?;
    write_lock.note_written(&md.markdown_uuid);

    Ok(md.rows.len())
}

/// The one row that is the document — the chat/thread/PR/page row, the
/// renderer having said so with `is_document`. Anything but exactly one
/// is a renderer bug, and a bug here is one the grid cannot show, so it
/// fails the render rather than guessing which row was meant.
fn document_row<'a>(rows: &'a [GridRow], markdown_uuid: &str) -> Result<&'a GridRow> {
    let mut documents = rows.iter().filter(|r| r.is_document);
    let (first, second) = (documents.next(), documents.next());
    match (first, second) {
        (Some(row), None) => Ok(row),
        (None, _) => bail!(
            "document {markdown_uuid}: none of its {} rows is marked is_document",
            rows.len()
        ),
        (Some(a), Some(b)) => bail!(
            "document {markdown_uuid}: rows {} and {} are both marked is_document",
            a.uuid,
            b.uuid
        ),
    }
}

/// The run-pinned `--now` when there is one, else the clock, as the
/// tables keep it.
fn run_stamp(now_override: Option<&str>) -> datalib_time::StoredStamp {
    match now_override {
        Some(now) => datalib_time::split_stamp(now),
        None => {
            let (utc, offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
            datalib_time::StoredStamp {
                utc,
                tz_offset: Some(offset),
            }
        }
    }
}

async fn upsert_markdown(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    md: &RenderedMarkdown,
    canonical: &GridRow,
    qmd_path: &str,
) -> Result<()> {
    let kind = doc_kind_for(&canonical.kind);
    let version_str = format!("{RENDERER_VERSION}.{}", md.render_version);
    // Fall back to the canonical row's provider when build_grid_index
    // rebuilds from disk without the config-level name.
    let source_id = if md.source_id.is_empty() {
        canonical.provider.clone()
    } else {
        md.source_id.clone()
    };

    sqlx::query("DELETE FROM markdowns WHERE markdown_uuid = ?")
        .bind(&md.markdown_uuid)
        .execute(&mut **conn)
        .await
        .context("delete prior markdowns row")?;
    sqlx::query(
        "INSERT INTO markdowns \
         (markdown_uuid, source_id, provider, kind, title, created_at, modified_at, \
          item_count, md_path, upstream_cursor, renderer_version, bucket_key) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&md.markdown_uuid)
    .bind(&source_id)
    .bind(&canonical.provider)
    .bind(kind)
    .bind(&canonical.conversation_name)
    .bind(canonical.created_at.as_deref())
    .bind(canonical.modified_at.as_deref())
    .bind(canonical.item_count)
    .bind(qmd_path)
    .bind(md.upstream_cursor.as_deref())
    .bind(&version_str)
    .bind(&md.bucket_key)
    .execute(&mut **conn)
    .await
    .context("insert markdowns row")?;
    Ok(())
}

/// One source's account of a person, and each handle it ties to them,
/// under the document that carried it. Two accounts of one person in one
/// document are one row: the later wins.
async fn insert_source_contact(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    markdown_uuid: &str,
    contact: &NormalizedContact,
) -> Result<()> {
    let json = serde_json::to_string(contact).context("serialize a source contact")?;
    let seen = contact.seen.as_ref();
    sqlx::query(
        "INSERT OR REPLACE INTO source_contacts \
         (markdown_uuid, contact_key, source_id, name, seen_items, last_seen_at, contact_json) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(markdown_uuid)
    .bind(&contact.key)
    .bind(&contact.source_id)
    .bind(contact.name())
    .bind(seen.map_or(0, |s| s.items as i64))
    .bind(seen.and_then(|s| s.last_at.as_deref()))
    .bind(json)
    .execute(&mut **conn)
    .await
    .with_context(|| format!("insert source contact {}", contact.key))?;
    for handle in contact.handles.iter().filter_map(|h| h.handle.as_ref()) {
        sqlx::query(
            "INSERT OR IGNORE INTO source_contact_handles (markdown_uuid, contact_key, handle) \
             VALUES (?, ?, ?)",
        )
        .bind(markdown_uuid)
        .bind(&contact.key)
        .bind(handle.as_str())
        .execute(&mut **conn)
        .await
        .with_context(|| format!("insert handle {handle} of {}", contact.key))?;
    }
    Ok(())
}

async fn insert_source_photo(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    markdown_uuid: &str,
    photo: &NormalizedPhoto,
) -> Result<()> {
    let json = serde_json::to_string(photo).context("serialize a source photo")?;
    sqlx::query(
        "INSERT OR REPLACE INTO source_photos (markdown_uuid, photo_uuid, source_id, photo_json) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(markdown_uuid)
    .bind(&photo.uuid)
    .bind(&photo.source_id)
    .bind(json)
    .execute(&mut **conn)
    .await
    .with_context(|| format!("insert source photo {}", photo.uuid))?;
    Ok(())
}

async fn insert_edge(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    edge: &EdgeRow,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO edges \
         (edge_uuid, src_markdown_uuid, src_anchor_uuid, dst_markdown_uuid, dst_anchor_uuid, label) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&edge.edge_uuid)
    .bind(&edge.src_markdown_uuid)
    .bind(&edge.src_anchor_uuid)
    .bind(&edge.dst_markdown_uuid)
    .bind(&edge.dst_anchor_uuid)
    .bind(&edge.label)
    .execute(&mut **conn)
    .await
    .with_context(|| format!("insert edge {}", edge.edge_uuid))?;
    Ok(())
}

fn first_repeated<'a>(uuids: &[&'a str]) -> Option<&'a str> {
    let mut seen = HashSet::with_capacity(uuids.len());
    uuids.iter().copied().find(|u| !seen.insert(*u))
}

/// Insert one `GridRow`; on a uuid its own document already holds,
/// rewrite the row only where it differs. An unchanged row then costs no
/// index write and, in the grid index, no new search terms: a source read
/// whole again rewrites only what moved.
static PUT_GRID_ROW_SQL: LazyLock<String> = LazyLock::new(|| {
    let columns = GridRow::TYPED_COLUMNS;
    let list = |prefix: &str| -> String {
        columns
            .iter()
            .map(|c| format!("{prefix}{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let set: Vec<String> = columns
        .iter()
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    format!(
        "{} ON CONFLICT ({id}) DO UPDATE SET {} \
         WHERE {table}.markdown_uuid IS excluded.markdown_uuid AND ({}) IS NOT ({})",
        datalib_etl::bulk::insert_sql::<GridRow>(),
        set.join(", "),
        list(&format!("{}.", GridRow::TABLE)),
        list("excluded."),
        id = GridRow::ID_COLUMN,
        table = GridRow::TABLE,
    )
});

/// Write one row, and settle a uuid another document holds by *who*
/// wrote that document. One an earlier run wrote means the row has
/// moved — a message re-bucketed into another period, a chat whose
/// document id is minted from a name that changed — and the incoming
/// document takes it over; the old owner, when it re-renders, no longer
/// emits it. One *this* run wrote means two documents minted one id,
/// which is a finding and fails.
async fn put_grid_row(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    write_lock: &WriteLock,
    row: &GridRow,
) -> Result<()> {
    let written = row
        .bind_into(sqlx::query(PUT_GRID_ROW_SQL.as_str()))
        .execute(&mut **conn)
        .await
        .with_context(|| format!("write grid_row {}", row.uuid))?;
    if written.rows_affected() == 1 {
        write_lock.note_changed([row.uuid.clone()]);
        return Ok(());
    }

    // Nothing written: the row is here unchanged, or another document
    // holds its uuid.
    let (provider, md): (String, String) =
        sqlx::query_as("SELECT provider, IFNULL(markdown_uuid, '') FROM grid_rows WHERE uuid = ?")
            .bind(&row.uuid)
            .fetch_one(&mut **conn)
            .await
            .with_context(|| format!("find who holds grid_row {}", row.uuid))?;
    if md == row.markdown_uuid.as_deref().unwrap_or("") {
        return Ok(());
    }
    let moved = provider == row.provider && !write_lock.written_this_run(&md);
    if !moved {
        bail!(
            "insert grid_row {}: an existing {provider} row already holds that \
             uuid (markdown {md}, written this run); the incoming row is a {} \
             from markdown {}",
            row.uuid,
            row.provider,
            row.markdown_uuid.as_deref().unwrap_or("<none>"),
        );
    }
    tracing::info!(
        uuid = %row.uuid,
        from = %md,
        to = row.markdown_uuid.as_deref().unwrap_or("<none>"),
        "grid_row moved to another document"
    );
    sqlx::query("DELETE FROM grid_rows WHERE uuid = ?")
        .bind(&row.uuid)
        .execute(&mut **conn)
        .await
        .with_context(|| format!("release moved grid_row {}", row.uuid))?;
    // Audited: `insert_sql` is built from `GridRow`'s associated consts,
    // never from row data; every value is bound.
    row.bind_into(sqlx::query(sqlx::AssertSqlSafe(
        datalib_etl::bulk::insert_sql::<GridRow>(),
    )))
    .execute(&mut **conn)
    .await
    .with_context(|| format!("insert moved grid_row {}", row.uuid))?;
    write_lock.note_changed([row.uuid.clone()]);
    Ok(())
}

/// Fails unless SQLite plans every one of `statements` through an index.
#[cfg(test)]
pub(crate) async fn assert_searched_by_index(pool: &SqlitePool, statements: &[&'static str]) {
    for sql in statements {
        // Audited: `sql` is a static literal from the test.
        let mut explain = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")));
        for _ in 0..sql.matches('?').count() {
            explain = explain.bind("x");
        }
        let plan: Vec<String> = explain
            .fetch_all(pool)
            .await
            .unwrap_or_else(|e| panic!("explain {sql}: {e}"))
            .iter()
            .map(|r| r.get::<String, _>("detail"))
            .collect();
        assert!(
            plan.iter().all(|d| !d.starts_with("SCAN")),
            "{sql} scans its table: {plan:?}"
        );
    }
}

#[cfg(test)]
mod open_index_tests {
    use super::*;

    /// Guards the two warnings per pass, 88 in one sync, about a source
    /// that was turned off and so could not change: one that stays
    /// unreadable is said once.
    #[test]
    fn an_unreadable_store_is_news_once() {
        use datalib_schema::problems::Severity;
        let row = || unreadable_store_problem("s", OLDER_SHAPE, Severity::Warning);
        let first = row().stamped(None, "2026-11-04T10:00:00Z", None);
        assert!(is_news(None, &first));
        let again = row().stamped(Some(&first), "2026-11-04T10:05:00Z", None);
        assert!(!is_news(Some(&first), &again), "nothing it says changed");
        let other = unreadable_store_problem("s", "it will not open", Severity::Warning).stamped(
            Some(&first),
            "2026-11-04T10:10:00Z",
            None,
        );
        assert!(is_news(Some(&first), &other), "it says something else now");
    }

    /// Replacing a document looks its old rows up by index. Without one,
    /// each document's delete scanned the whole table, and a full render
    /// of a 20k-thread mailbox spent most of its time there.
    #[tokio::test]
    async fn replacing_a_document_finds_its_old_rows_by_index() {
        let td = tempfile::tempdir().unwrap();
        let pool = open_index(&td.path().join("db.sqlite"))
            .await
            .expect("open_index");
        assert_searched_by_index(
            &pool,
            &[
                "DELETE FROM grid_rows WHERE markdown_uuid = ?",
                "DELETE FROM edges WHERE src_markdown_uuid = ?",
            ],
        )
        .await;
        pool.close().await;
    }
}

#[cfg(test)]
mod insert_round_trip_tests {
    //! Every `GridRow` field has to actually reach the index. A field can be
    //! declared, populated, selected and given a grid column — and still be
    //! dropped silently on the way in, landing as a NULL while every layer
    //! reports success, which is how `org_uuid` / `org_name` shipped. So every
    //! column gets a distinct sentinel and nothing may read back NULL.
    use super::*;
    use datalib_schema::grid_rows::{content_hash, GridRow};
    use datalib_schema::providers::Provider;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::{Column, Row, ValueRef};
    use std::str::FromStr;
    use tempfile::tempdir;

    /// Every `Option` field `Some` and every scalar non-empty, so a NULL read
    /// back is a dropped binding rather than a genuinely absent value.
    fn fully_populated_row() -> GridRow {
        GridRow {
            uuid: "row-everything".into(),
            provider: Provider::Claude.as_str().into(),
            kind: "Chat".into(),
            source_label: "Claude".into(),
            // Offset-bearing and parseable, so the two `#[derived]` columns
            // are non-NULL too.
            created_at: Some("2026-06-02T13:00:00-07:00".into()),
            modified_at: Some("2026-06-03T09:30:00-07:00".into()),
            touched_at: Some("2026-06-03T09:30:00-07:00".into()),
            is_document: true,
            author: Some("Jean-Luc Picard".into()),
            author_handle: Some("email:picard@enterprise.org".into()),
            contact: Some("Jean-Luc Picard".into()),
            email: Some("picard@enterprise.org".into()),
            phone: Some("+1 202 555 0101".into()),
            account: Some("acct-1701".into()),
            project: Some("proj-1701".into()),
            org_uuid: Some("org-1701".into()),
            org_name: Some("Starfleet".into()),
            channel: Some("bridge".into()),
            conversation_name: Some("Klingon Diplomatic Greeting".into()),
            conversation_uuid: "conv-1701".into(),
            message_index: Some(3),
            entire_chat: "/chat/conv-1701".into(),
            preview: "Tea. Earl Grey. Hot.".into(),
            content_hash: content_hash("Tea. Earl Grey. Hot."),
            qmd_path: Some("chats/conv-1701.md".into()),
            source_url: Some("https://claude.ai/chat/conv-1701".into()),
            git_sha: Some("0123456789abcdef".into()),
            upstream_id: Some("upstream-1701".into()),
            upstream_entity_kind: Some("conversation".into()),
            upstream_account: Some("claude.ai".into()),
            notion_page_uuid: Some("notion-page-1701".into()),
            notion_block_uuid: Some("notion-block-1701".into()),
            markdown_uuid: Some("md-1701".into()),
            byte_size: Some(4_096),
            item_count: Some(17),
            diff_status: Some("modified".into()),
            diff_changed_columns: Some("text|author".into()),
            supplied_terms: Some(r#"[["to","email:troi@enterprise.org"]]"#.into()),
            image: Some("datalib:photo/u-1".into()),
            latitude: Some(37.8),
            longitude: Some(-122.4),
            h3: Some(0x8f28308280f18f2),
        }
    }

    #[tokio::test]
    async fn every_grid_row_column_survives_the_insert() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("round_trip.doltlite_db");
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .expect("open pool");
        init_schema(&pool).await.expect("init_schema");

        let row = fully_populated_row();
        let mut conn = pool.acquire().await.expect("acquire");
        let lock = WriteLock::new(pool.clone());
        put_grid_row(&mut conn, &lock, &row)
            .await
            .expect("put_grid_row");
        drop(conn);

        // `SELECT *` on purpose: the point is to see every column the DDL
        // declares, including ones this test predates.
        let read_back = sqlx::query("SELECT * FROM grid_rows WHERE uuid = ?")
            .bind(&row.uuid)
            .fetch_one(&pool)
            .await
            .expect("read grid_rows back");

        let dropped: Vec<&str> = read_back
            .columns()
            .iter()
            .filter(|c| {
                read_back
                    .try_get_raw(c.ordinal())
                    .map(|v| v.is_null())
                    .unwrap_or(false)
            })
            .map(|c| c.name())
            .collect();
        assert!(
            dropped.is_empty(),
            "every column of a fully-populated GridRow should come back \
             non-NULL, but these read back NULL: {dropped:?}. Each is \
             declared on GridRow (or derived in put_grid_row) but \
             missing from the INSERT's column list or its bindings."
        );

        // Spot-check the two that motivated this test, so a failure
        // names them rather than only counting NULLs.
        assert_eq!(
            read_back.try_get::<Option<String>, _>("org_uuid").unwrap(),
            row.org_uuid
        );
        assert_eq!(
            read_back.try_get::<Option<String>, _>("org_name").unwrap(),
            row.org_name
        );
    }
}

#[cfg(test)]
// Test diagnostics; cargo test captures and prints them per-test.
#[allow(clippy::disallowed_macros)]
mod write_lock_tests {
    //! Reproduces the production "(code 5) database is locked": several
    //! per-source render workers calling [`apply_one`] in parallel against one
    //! pool with `max_connections > 1`. Without the [`WriteLock`] each task
    //! gets its own connection, all queue for doltlite's one write at a time,
    //! and the losers time out. No artificial sleeps — the contention is real,
    //! from the same code path production uses.
    use super::*;
    use datalib_schema::grid_rows::{content_hash, doc_file_rel, GridRow};
    use datalib_schema::providers::Provider;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use std::sync::Arc as StdArc;
    use tempfile::tempdir;

    fn mk_md(task: usize, idx: usize) -> RenderedMarkdown {
        let uuid = format!("md-task{task:02}-doc{idx:04}");
        // One canonical chat row per markdown — enough to exercise
        // the DELETE + insert path. We don't care about content.
        let row = GridRow {
            uuid: uuid.clone(),
            provider: Provider::Claude.as_str().into(),
            kind: "Chat".into(),
            source_label: "Claude".into(),
            created_at: Some("2026-06-02T20:00:00+00:00".into()),
            modified_at: None,
            touched_at: None,
            is_document: true,
            author: None,
            author_handle: None,
            contact: None,
            email: None,
            phone: None,
            account: Some("acct-test".into()),
            project: None,
            org_uuid: None,
            org_name: None,
            channel: None,
            conversation_name: Some(format!("Conv {uuid}")),
            conversation_uuid: uuid.clone(),
            message_index: None,
            entire_chat: format!("/chat/{uuid}"),
            preview: format!("body for {uuid}"),
            content_hash: content_hash(&format!("body for {uuid}")),
            qmd_path: Some(format!("chats/{}", doc_file_rel(&uuid))),
            source_url: None,
            git_sha: None,
            upstream_id: None,
            upstream_entity_kind: None,
            upstream_account: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            markdown_uuid: Some(uuid.clone()),
            byte_size: None,
            item_count: None,
            diff_status: None,
            diff_changed_columns: None,
            supplied_terms: None,
            image: None,
            latitude: None,
            longitude: None,
            h3: None,
        };
        RenderedMarkdown {
            markdown_uuid: uuid.clone(),
            source_id: "test".into(),
            upstream_cursor: None,
            bucket_key: None,
            md_path: PathBuf::from("/tmp/chats").join(doc_file_rel(&uuid)),
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            edges: Vec::new(),
            contacts: Vec::new(),
            photos: Vec::new(),
            problems: Vec::new(),
        }
    }

    async fn open_pool(db: &Path, max_conn: u32) -> SqlitePool {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))
            .unwrap()
            .create_if_missing(true);
        SqlitePoolOptions::new()
            .max_connections(max_conn)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap()
    }

    async fn apply_in_parallel(write_lock: &StdArc<WriteLock>, n_tasks: usize, per_task: usize) {
        let out_dir = PathBuf::from("/tmp");
        let mut handles = Vec::with_capacity(n_tasks);
        for task in 0..n_tasks {
            let lock = write_lock.clone();
            let out_dir = out_dir.clone();
            handles.push(tokio::spawn(async move {
                for idx in 0..per_task {
                    let md = mk_md(task, idx);
                    apply_one(lock.as_ref(), &out_dir, &md)
                        .await
                        .unwrap_or_else(|e| panic!("apply_one task={task} idx={idx}: {e:#}"));
                }
            }));
        }
        for h in handles {
            h.await.expect("task join");
        }
    }

    /// Per-call auto-commit mode: N parallel tasks through `apply_one`, each
    /// writing K unique markdowns into one pool. `max_connections=8` so the
    /// pool *could* hand out enough connections for the busy-timeout race;
    /// with the lock, only one writer runs at a time.
    ///
    /// `#[ignore]`d because it dominates the suite's critical path (~26s for
    /// 480 serialized auto-commit dolt writes). It exists to characterize the
    /// order-of-magnitude gap against the transaction-batched test below. Run
    /// it with `--test_arg=--ignored` when changing the WriteLock,
    /// `apply_one`, or doltlite's auto-commit path.
    #[ignore = "slow (~26s) — perf characterization; run on demand"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_apply_one_serializes_writes_with_metrics() {
        const N_TASKS: usize = 16;
        const PER_TASK: usize = 30;
        const TOTAL: usize = N_TASKS * PER_TASK;

        let dir = tempdir().unwrap();
        let db = dir.path().join("contention.doltlite_db");
        let pool = open_pool(&db, 8).await;
        super::init_schema(&pool).await.expect("init_schema");

        let write_lock = WriteLock::new_arc(pool.clone());

        apply_in_parallel(&write_lock, N_TASKS, PER_TASK).await;

        let grid_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grid_rows")
            .fetch_one(&pool)
            .await
            .unwrap();
        let md_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM markdowns")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(grid_n as usize, TOTAL, "grid_rows row count");
        assert_eq!(md_n as usize, TOTAL, "markdowns row count");

        let m = write_lock.metrics();
        assert_eq!(m.acquisitions as usize, TOTAL, "acquisitions");
        assert!(m.total_hold > Duration::ZERO, "hold time must be > 0");
        eprintln!(
            "[write_lock test no-tx] N={N_TASKS} K={PER_TASK} total={TOTAL} \
             total_hold={:?} avg_hold={:?} total_wait={:?} avg_wait={:?}",
            m.total_hold,
            m.avg_hold(),
            m.total_wait,
            m.avg_wait(),
        );
    }

    /// One big transaction wrapping every write — the production mode.
    /// Asserts every write succeeds, the final COMMIT lands every row, and
    /// `avg_hold` is dramatically smaller than the auto-commit version above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_apply_one_inside_one_transaction_is_faster() {
        const N_TASKS: usize = 16;
        const PER_TASK: usize = 30;
        const TOTAL: usize = N_TASKS * PER_TASK;

        let dir = tempdir().unwrap();
        let db = dir.path().join("batched.doltlite_db");
        let pool = open_pool(&db, 8).await;
        super::init_schema(&pool).await.expect("init_schema");

        let write_lock = WriteLock::new_arc(pool.clone());

        // Every apply_one below reuses the held conn and accumulates into the
        // open transaction.
        write_lock.begin_transaction().await.expect("BEGIN");

        apply_in_parallel(&write_lock, N_TASKS, PER_TASK).await;

        // Before commit: rows aren't visible from a fresh connection
        // (other than the one holding the open tx).
        let pre_grid_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grid_rows")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            pre_grid_n, 0,
            "pre-COMMIT: other connections must not see uncommitted rows"
        );

        write_lock.commit_transaction().await.expect("COMMIT");

        let grid_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grid_rows")
            .fetch_one(&pool)
            .await
            .unwrap();
        let md_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM markdowns")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(grid_n as usize, TOTAL, "grid_rows row count after COMMIT");
        assert_eq!(md_n as usize, TOTAL, "markdowns row count after COMMIT");

        let m = write_lock.metrics();
        assert_eq!(m.acquisitions as usize, TOTAL, "acquisitions");
        eprintln!(
            "[write_lock test tx] N={N_TASKS} K={PER_TASK} total={TOTAL} \
             total_hold={:?} avg_hold={:?} total_wait={:?} avg_wait={:?}",
            m.total_hold,
            m.avg_hold(),
            m.total_wait,
            m.avg_wait(),
        );
    }

    /// `rollback_transaction` undoes every write in the batch.
    #[tokio::test]
    async fn rollback_undoes_batch() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("rb.doltlite_db"), 2).await;
        super::init_schema(&pool).await.expect("init_schema");

        let lock = WriteLock::new(pool.clone());
        let out_dir = PathBuf::from("/tmp");

        lock.begin_transaction().await.unwrap();
        for idx in 0..5 {
            apply_one(&lock, &out_dir, &mk_md(0, idx)).await.unwrap();
        }
        lock.rollback_transaction().await.unwrap();

        let grid_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grid_rows")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(grid_n, 0, "ROLLBACK must leave grid_rows untouched");
    }

    /// A document re-rendered with every row rejected keeps no
    /// `markdowns` row: before this, the old render's title, timestamps
    /// and `bucket_key` outlived the rows they described. Found by the
    /// contract harness on a gitlab merge request re-keyed under another
    /// bucket whose rows all failed `created_at`.
    #[tokio::test]
    async fn a_document_re_rendered_with_no_rows_loses_its_markdowns_row() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("empty.doltlite_db"), 2).await;
        super::init_schema(&pool).await.expect("init_schema");
        let lock = WriteLock::new(pool.clone());
        let out_dir = PathBuf::from("/tmp");

        let mut md = mk_md(0, 0);
        md.bucket_key = Some("mr!17".into());
        apply_one(&lock, &out_dir, &md).await.unwrap();

        let mut moved = mk_md(0, 0);
        moved.bucket_key = Some("mr!17~new".into());
        moved.rows.clear();
        let inserted = apply_one(&lock, &out_dir, &moved).await.unwrap();
        assert_eq!(inserted, 0);

        let buckets: Vec<Option<String>> =
            sqlx::query_scalar("SELECT bucket_key FROM markdowns WHERE markdown_uuid = ?")
                .bind(&md.markdown_uuid)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            buckets.is_empty(),
            "no markdowns row, old bucket or new: {buckets:?}"
        );
        let grid_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grid_rows")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(grid_n, 0);
    }

    #[tokio::test]
    async fn metrics_safe_when_never_acquired() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("m.doltlite_db"), 1).await;
        let lock = WriteLock::new(pool);
        let m = lock.metrics();
        assert_eq!(m.acquisitions, 0);
        assert_eq!(m.total_wait, Duration::ZERO);
        assert_eq!(m.total_hold, Duration::ZERO);
        assert_eq!(m.avg_wait(), Duration::ZERO);
        assert_eq!(m.avg_hold(), Duration::ZERO);
        let _ = StdArc::new(()).as_ref();
    }
}

#[cfg(test)]
mod shape_tests {
    //! An index in another shape must be rebuilt without a person deleting
    //! it, and one in this shape left alone. Both fail silently: kept, a
    //! statement naming a new column errors against an older root; thrown
    //! away on every open, the index is rebuilt each pass, and the only
    //! symptom is slowness.

    use std::path::Path;

    use sqlx::sqlite::SqlitePool;
    use tempfile::tempdir;

    use crate::grid_index::open_index;

    async fn holding_one_document(db: &Path) -> SqlitePool {
        let pool = open_index(db).await.unwrap();
        sqlx::query(
            "INSERT INTO markdowns (markdown_uuid, source_id, provider, kind) \
             VALUES ('md-1', 'claude_web', 'claude', 'Chat')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn documents(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM markdowns")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// An index another build wrote is made new, its cursors with it: a
    /// cursor kept would tell the next pass that nothing changed in a
    /// store, and the rebuilt index would stay empty.
    #[tokio::test]
    async fn an_index_in_another_shape_is_made_new() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        sqlx::query("UPDATE _datalib_meta SET value = 'older' WHERE key = 'schema_hash'")
            .execute(&pool)
            .await
            .unwrap();
        crate::grid_index::close_index(pool).await.unwrap();

        let pool = open_index(&db).await.unwrap();
        assert_eq!(documents(&pool).await, 0);
    }

    #[tokio::test]
    async fn an_index_in_this_shape_is_kept() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        crate::grid_index::close_index(pool).await.unwrap();

        let pool = open_index(&db).await.unwrap();
        assert_eq!(documents(&pool).await, 1);
    }
}

#[cfg(test)]
mod remake_tests {
    use super::remakes_search_indexes;

    /// A load whose first twentieth changed nothing, or a sliver of a
    /// large index, keeps the indexes; one on course to change a sixteenth
    /// of the rows remakes them.
    #[test]
    fn only_a_load_on_course_to_change_a_sixteenth_of_the_index_remakes_it() {
        assert!(!remakes_search_indexes(0, 3_000, 60_000, 800_000));
        assert!(!remakes_search_indexes(100, 3_000, 60_000, 800_000));
        assert!(remakes_search_indexes(6_000, 3_000, 60_000, 800_000));
        assert!(remakes_search_indexes(2, 1, 4, 4));
    }
}

#[cfg(test)]
mod source_cursor_tests {
    //! What the cursor buys, and the trap in testing it.
    //!
    //! Before the cursor, a steady-state re-index still *read* every document
    //! and dropped the unchanged ones. Nothing was written either way, so
    //! "nothing was loaded" proves nothing. `markdowns_total`
    //! — documents actually read — is the field that separates the two, and
    //! every test here asserts on it.

    use std::path::Path;
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
    use tempfile::tempdir;

    use crate::grid_index::{
        build_grid_index, build_grid_index_for, discover_sources, doc_md_path, load_source_cursors,
        RenderedMarkdown,
    };
    use crate::indexed_markdown::IndexedMarkdownStore;
    use datalib_etl::stop::StopFlag;
    use datalib_schema::grid_rows::doc_file_rel;
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::providers::Provider;

    async fn index_pool(root: &Path) -> SqlitePool {
        crate::grid_index::open_index(&datalib_core::layout::grid_index_db(root))
            .await
            .unwrap()
    }

    fn doc(root: &Path, source: &str, uuid: &str, text: &str) -> RenderedMarkdown {
        let row = GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Test)
            .kind("Test")
            .source_label("Test")
            .conversation_uuid(uuid)
            .entire_chat(format!("/chat/{uuid}"))
            .body(text)
            .markdown_uuid(Some(uuid.to_string()))
            .qmd_path(Some(format!(
                "{source}/render_markdown/{}",
                doc_file_rel(uuid)
            )))
            .created_at(Some("2026-01-01T00:00:00+00:00".to_string()))
            .is_document(true)
            .item_count(Some(1))
            .build()
            .unwrap();
        RenderedMarkdown {
            markdown_uuid: uuid.to_string(),
            source_id: source.to_string(),
            // Fingerprint follows the text, the way a renderer's does.
            upstream_cursor: None,
            bucket_key: None,
            md_path: doc_md_path(&rendered_root(root, source), uuid),
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            edges: Vec::new(),
            contacts: Vec::new(),
            photos: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn rendered_root(root: &Path, source: &str) -> std::path::PathBuf {
        datalib_etl::layout::render_markdown_root(root, source)
    }

    fn render(root: &Path, source: &str, docs: &[RenderedMarkdown]) {
        let store = IndexedMarkdownStore::open(&rendered_root(root, source)).unwrap();
        for d in docs {
            store.put_document(root, d).unwrap();
        }
        store.commit("test render").unwrap();
        store.close();
    }

    fn unrender(root: &Path, source: &str, uuid: &str) {
        let store = IndexedMarkdownStore::open(&rendered_root(root, source)).unwrap();
        store.remove_document(root, uuid).unwrap();
        store.commit("test unrender").unwrap();
        store.close();
    }

    /// A document under a bucket other than itself, whose `.md` actually
    /// exists on disk — the shape a periodizing renderer produces, and
    /// the one the deletion path has to handle.
    fn doc_in_bucket(root: &Path, source: &str, uuid: &str, bucket: &str) -> RenderedMarkdown {
        let mut md = doc(root, source, uuid, "body");
        md.rows[0].conversation_uuid = bucket.to_string();
        md.bucket_key = Some(bucket.to_string());
        std::fs::create_dir_all(md.md_path.parent().unwrap()).unwrap();
        std::fs::write(&md.md_path, "# rendered\n").unwrap();
        md
    }

    /// The step reads the sources the graph names, nothing else: a tree
    /// left on disk by a source no longer in the config is not indexed,
    /// and a listed source with no store yet is simply skipped.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_listed_sources_are_read() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(root, "kept", &[doc(root, "kept", "md-k", "kept body")]);
        render(
            root,
            "dropped",
            &[doc(root, "dropped", "md-d", "dropped body")],
        );

        let listed = ["kept".to_string(), "not-rendered-yet".to_string()];
        build_grid_index_for(&pool, root, &listed, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert_eq!(index_row_count(&pool).await, 1, "only `kept`");

        // The scan is the dev tools' view and reads whatever is there.
        assert_eq!(
            discover_sources(root),
            vec!["dropped".to_string(), "kept".to_string()]
        );
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(index_row_count(&pool).await, 2);
    }

    /// A source taken out of the graph leaves the index with its cursor,
    /// so a renamed source does not show every document twice and one
    /// added back is read whole.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_no_longer_listed_leaves_the_index() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(root, "kept", &[doc(root, "kept", "md-k", "kept body")]);
        render(
            root,
            "dropped",
            &[doc(root, "dropped", "md-d", "dropped body")],
        );
        let both = ["dropped".to_string(), "kept".to_string()];
        build_grid_index_for(&pool, root, &both, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert_eq!(index_row_count(&pool).await, 2);

        let s = build_grid_index_for(
            &pool,
            root,
            &["kept".to_string()],
            |_| {},
            None,
            &StopFlag::new(),
        )
        .await
        .unwrap();
        assert_eq!(index_row_count(&pool).await, 1, "only `kept`");
        assert_eq!(s.markdowns_removed, 1);
        let cursors = load_source_cursors(&pool).await.unwrap();
        assert!(!cursors.contains_key("dropped"), "{cursors:?}");

        build_grid_index_for(&pool, root, &both, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert_eq!(index_row_count(&pool).await, 2, "read whole again");
    }

    /// One bucket, several rendered documents, all of them gone when the
    /// bucket is.
    ///
    /// The fan-out is the reason the store answers "which documents are
    /// under this bucket" rather than the renderer naming the document it
    /// wants dropped: slack, signal and beeper split one conversation
    /// across periods, and once the conversation is gone from the raw
    /// store nothing but this store still knows how many periods it had.
    #[tokio::test(flavor = "multi_thread")]
    async fn removing_a_bucket_takes_every_period_it_rendered_into() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let conv = "conv-1";
        let jan = doc_in_bucket(root, "src", "md-jan", conv);
        let feb = doc_in_bucket(root, "src", "md-feb", conv);
        let other = doc_in_bucket(root, "src", "md-other", "conv-2");
        let (jan_md, feb_md, other_md) = (
            jan.md_path.clone(),
            feb.md_path.clone(),
            other.md_path.clone(),
        );
        render(root, "src", &[jan, feb, other]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(index_row_count(&pool).await, 3);

        let store = IndexedMarkdownStore::open(&rendered_root(root, "src")).unwrap();
        let mut gone: Vec<String> = store
            .documents_for_buckets(&[conv])
            .unwrap()
            .into_iter()
            .map(|(_, uuid)| uuid)
            .collect();
        gone.sort();
        assert_eq!(
            gone,
            vec!["md-feb".to_string(), "md-jan".to_string()],
            "both of the bucket's periods, and only those"
        );
        for uuid in &gone {
            store.remove_document(root, uuid).unwrap();
        }
        store.commit("test: conversation gone upstream").unwrap();
        store.close();

        assert!(!jan_md.exists(), "the rendered markdown must go too");
        assert!(!feb_md.exists());
        assert!(
            other_md.exists(),
            "an untouched conversation keeps its file"
        );

        // And the index picks the removal up from the store's own diff,
        // which is the only way it ever learns about one.
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(
            index_row_count(&pool).await,
            1,
            "the grid must be left holding only the surviving conversation"
        );
    }

    /// Put a source's store back in the shape a build before
    /// `grid_rows.preview` wrote: the column is `text` there.
    async fn into_older_shape(root: &Path, source: &str) {
        alter_store(
            root,
            source,
            "ALTER TABLE grid_rows RENAME COLUMN preview TO text",
            "an older build's shape",
        )
        .await;
    }

    /// A render store an older build wrote, and whose source has not
    /// re-rendered since, left the whole grid empty: the index failed on
    /// it and indexed nothing. It must cost only its own source: the rest
    /// are indexed, its rows stay as they were, and a warning says why
    /// until it re-renders.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_in_an_older_shape_costs_only_its_own_source() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let sources = ["fresh".to_string(), "stale".to_string()];
        render(root, "fresh", &[doc(root, "fresh", "md-f", "fresh body")]);
        render(root, "stale", &[doc(root, "stale", "md-s", "stale body")]);
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();

        render(root, "fresh", &[doc(root, "fresh", "md-f2", "more")]);
        into_older_shape(root, "stale").await;
        // As after the index rebuilds itself for a new build: no cursors,
        // so every store is read whole.
        sqlx::query("DELETE FROM source_cursors")
            .execute(&pool)
            .await
            .unwrap();
        let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .expect("the other sources are indexed");
        assert_eq!(s.sources_unreadable, vec!["stale".to_string()]);
        assert_eq!(
            index_row_count(&pool).await,
            3,
            "fresh's new document lands and stale's stays"
        );
        let (severity, sample): (String, String) =
            sqlx::query_as("SELECT severity, sample FROM problems WHERE source_id = 'stale'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(severity, "warning");
        assert!(sample.contains("sync"), "{sample}");
        assert_eq!(
            super::own_problem_counts(&pool).await.unwrap(),
            std::collections::HashMap::from([(datalib_schema::problems::Severity::Warning, 1)]),
            "the index found this one itself, so its row counts it"
        );

        // Its next render rebuilds the store in the current shape.
        render(root, "stale", &[doc(root, "stale", "md-s", "stale body")]);
        let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert!(s.sources_unreadable.is_empty());
        let warnings: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM problems WHERE source_id = 'stale'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(warnings, 0, "the warning goes once the store reads");
    }

    /// One source whose diff failed stopped the whole pass, so a source
    /// added after it never reached the grid. It must cost only itself:
    /// the others are indexed, its rows stay, and an error names it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_that_fails_to_read_costs_only_its_own_source() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        // "broken" sorts first, so a pass that stops on it reaches nothing.
        let sources = ["broken".to_string(), "fresh".to_string()];
        render(
            root,
            "broken",
            &[doc(root, "broken", "md-b", "broken body")],
        );
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();

        render(root, "fresh", &[doc(root, "fresh", "md-f", "fresh body")]);
        let store = crate::indexed_markdown::path_for(&rendered_root(root, "broken"));
        std::fs::write(&store, b"not a doltlite store").unwrap();
        let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .expect("the other sources are indexed");
        assert_eq!(
            s.sources_failed
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["broken"]
        );
        assert_eq!(
            index_row_count(&pool).await,
            2,
            "fresh's document lands and broken's stays"
        );
        let severity: String =
            sqlx::query_scalar("SELECT severity FROM problems WHERE source_id = 'broken'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(severity, "error");

        std::fs::remove_file(&store).unwrap();
        render(
            root,
            "broken",
            &[doc(root, "broken", "md-b", "broken body")],
        );
        let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert!(s.sources_failed.is_empty());
        let errors: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM problems WHERE source_id = 'broken'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(errors, 0, "the error goes once the store reads");
    }

    /// A render step's store exists as an empty file from the moment its
    /// writer opens it until its first page is written. An index pass
    /// that ran in that window failed, and failed every request it
    /// served (a CI flake in data-sources-control.spec.ts).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_its_writer_has_only_just_created_is_not_yet_rendered() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let sources = ["born".to_string(), "fresh".to_string()];
        render(root, "fresh", &[doc(root, "fresh", "md-f", "fresh body")]);
        let store = crate::indexed_markdown::path_for(&rendered_root(root, "born"));
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, b"").unwrap();

        let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert!(s.sources_failed.is_empty(), "{:?}", s.sources_failed);
        assert_eq!(index_row_count(&pool).await, 1);
    }

    /// Run `sql` on a source's store as its owner would and commit it,
    /// leaving `_datalib_meta` as it was.
    async fn alter_store(root: &Path, source: &str, sql: &str, message: &str) {
        let path = crate::indexed_markdown::path_for(&rendered_root(root, source));
        let writer = datalib_etl::doltlite_raw::open_derived(
            &path,
            &[],
            datalib_etl::doltlite_raw::StoreKind::Render,
        )
        .await
        .unwrap();
        // Audited: test-only; every caller passes a literal or a table
        // name from the store's own DDL.
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&writer)
            .await
            .unwrap();
        datalib_etl::doltlite_raw::commit_run(&writer, message)
            .await
            .unwrap();
        writer.close().await;
    }

    async fn rows_of(pool: &SqlitePool, source: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT g.uuid FROM grid_rows g JOIN markdowns m USING (markdown_uuid) \
              WHERE m.source_id = ? ORDER BY g.uuid",
        )
        .bind(source)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// A store from a build before #962 lacked `source_contacts`, failed
    /// the diff, and kept every other source out of the grid. The next
    /// table added to the render DDL is the same store again, so this
    /// drops each table in turn: whichever it is, the pass indexes the
    /// other source and keeps this one's rows as they were.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_missing_any_one_table_costs_only_its_own_source() {
        for (table, _) in crate::indexed_markdown::store_tables() {
            let td = tempdir().unwrap();
            let root = td.path();
            let pool = index_pool(root).await;
            // "damaged" sorts first, so a pass that stops on it reaches nothing.
            let sources = ["damaged".to_string(), "fresh".to_string()];
            render(root, "damaged", &[doc(root, "damaged", "md-d", "kept")]);
            build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();

            render(root, "fresh", &[doc(root, "fresh", "md-f", "fresh body")]);
            render(root, "damaged", &[doc(root, "damaged", "md-d2", "more")]);
            alter_store(
                root,
                "damaged",
                &format!("DROP TABLE {table}"),
                "a build without this table",
            )
            .await;
            let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap_or_else(|e| panic!("without {table}: {e:#}"));
            assert!(
                s.sources_failed.is_empty(),
                "without {table}: {:?}",
                s.sources_failed
            );
            assert_eq!(rows_of(&pool, "fresh").await, ["md-f"], "without {table}");
            assert!(
                rows_of(&pool, "damaged")
                    .await
                    .contains(&"md-d".to_string()),
                "without {table}: the damaged source's rows stay"
            );
            pool.close().await;
        }
    }

    /// A store whose `_datalib_meta` names another shape is not read at
    /// all: one way for every table and column it may lack, and one
    /// warning. A store with no `_datalib_meta` predates every shape.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_whose_meta_names_another_shape_is_left_unread() {
        for change in [
            "UPDATE _datalib_meta SET value = 'an-older-shape' WHERE key = 'schema_hash'",
            "DROP TABLE _datalib_meta",
        ] {
            let td = tempdir().unwrap();
            let root = td.path();
            let pool = index_pool(root).await;
            let sources = ["old".to_string()];
            render(root, "old", &[doc(root, "old", "md-1", "one")]);
            build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();

            render(root, "old", &[doc(root, "old", "md-2", "two")]);
            alter_store(root, "old", change, "another build's shape").await;
            let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();
            assert_eq!(s.sources_unreadable, ["old"], "{change}");
            assert_eq!(s.markdowns_total, 0, "{change}: nothing was read");
            assert_eq!(rows_of(&pool, "old").await, ["md-1"], "{change}");

            // Its next render writes this build's shape, and it reads again.
            render(root, "old", &[doc(root, "old", "md-2", "two")]);
            let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();
            assert!(s.sources_unreadable.is_empty(), "{change}");
            assert_eq!(rows_of(&pool, "old").await, ["md-1", "md-2"], "{change}");
            pool.close().await;
        }
    }

    async fn index_row_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM grid_rows")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The headline claim: a second run over an unchanged source reads
    /// nothing at all. `markdowns_total == 0` is the whole assertion — the
    /// old read-then-compare behaviour also reported `markdowns_loaded: 0`.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unchanged_source_is_not_read_at_all_on_the_second_run() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(
            root,
            "src",
            &[doc(root, "src", "md-1", "a"), doc(root, "src", "md-2", "b")],
        );

        let first = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(first.markdowns_total, 2, "cold start reads the whole store");
        assert_eq!(first.markdowns_loaded, 2);
        assert_eq!(index_row_count(&pool).await, 2);

        let second = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(
            second.markdowns_total, 0,
            "nothing changed, so nothing should have been READ — a non-zero \
             count here means the cursor was ignored and the run fell back to \
             reading the whole store"
        );
        assert_eq!(second.markdowns_loaded, 0);
        assert_eq!(index_row_count(&pool).await, 2, "and the rows are intact");
    }

    async fn generation(pool: &SqlitePool) -> String {
        crate::grid_index::version(pool).await.unwrap().unwrap()
    }

    /// The generation is what tells the grid to fetch again. A pass that
    /// changed nothing a reader sees must leave it where it was, or every
    /// pass — one per producer checkpoint while a sync streams — would
    /// have the open grid fetch the same rows again; one that changed
    /// something must raise it.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_pass_that_changed_something_raises_the_generation() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let empty = generation(&pool).await;
        render(root, "src", &[doc(root, "src", "md-1", "a")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        let first = generation(&pool).await;
        assert_ne!(first, empty, "the first pass loaded a document");

        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(
            generation(&pool).await,
            first,
            "a pass that changed nothing"
        );

        render(
            root,
            "src",
            &[doc(root, "src", "md-1", "a"), doc(root, "src", "md-2", "b")],
        );
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_ne!(
            generation(&pool).await,
            first,
            "a pass that added a document"
        );
    }

    /// What a reader sees once a pass has returned: what its transactions
    /// committed, a rolled-back one leaving nothing.
    async fn sealed_cursors(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT source_id FROM source_cursors ORDER BY source_id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn missing_search_indexes(pool: &SqlitePool) -> Vec<&'static str> {
        let present: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'index'")
                .fetch_all(pool)
                .await
                .unwrap();
        datalib_schema::grid_rows::INDEXES
            .iter()
            .map(|(_table, ddl)| crate::grid_index::index_name(ddl).unwrap())
            .filter(|name| !present.iter().any(|p| p == name))
            .collect()
    }

    /// A build into an empty index makes the search indexes at its end
    /// rather than keeping them up row by row. Neither a finished build nor
    /// a stopped one may leave the grid without them: the stopped one's
    /// are made by the next open.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_build_leaves_every_search_index_in_place() {
        let td = tempdir().unwrap();
        let root = td.path();
        for source in ["a", "b"] {
            let docs: Vec<_> = (1..=3)
                .map(|i| doc(root, source, &format!("md-{source}{i}"), "body"))
                .collect();
            render(root, source, &docs);
        }
        let sources = ["a".to_string(), "b".to_string()];

        let pool = index_pool(root).await;
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert_eq!(missing_search_indexes(&pool).await, Vec::<&str>::new());
        pool.close().await;

        std::fs::remove_dir_all(datalib_core::layout::grid_index_db(root).parent().unwrap())
            .unwrap();
        let pool = index_pool(root).await;
        let stop = StopFlag::new();
        build_grid_index_for(
            &pool,
            root,
            &sources,
            |m| {
                if m.starts_with("b: loaded 1/") {
                    stop.request();
                }
            },
            None,
            &stop,
        )
        .await
        .unwrap();
        pool.close().await;
        let pool = index_pool(root).await;
        assert_eq!(
            missing_search_indexes(&pool).await,
            Vec::<&str>::new(),
            "the open after a stopped build"
        );
    }

    /// A source that rewrites every row of a live index drops the search
    /// indexes partway through its load and makes them again before it
    /// commits; the index must come out of that whole, rows and indexes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_load_that_changes_most_of_the_index_leaves_it_whole() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let docs = |text: &str| -> Vec<RenderedMarkdown> {
            (1..=4)
                .map(|i| doc(root, "src", &format!("md-{i}"), text))
                .collect()
        };
        render(root, "src", &docs("before"));
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();

        render(root, "src", &docs("after"));
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(missing_search_indexes(&pool).await, Vec::<&str>::new());
        let bodies: Vec<String> = sqlx::query_scalar("SELECT DISTINCT preview FROM grid_rows")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(bodies, vec!["after".to_string()]);
        // Incremental (2): the mode a file takes only when it is set before
        // its first table, and without which the remade indexes' pages
        // come from wherever the free list scattered them.
        let auto_vacuum: i64 = sqlx::query_scalar("PRAGMA auto_vacuum")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(auto_vacuum, 2);
    }

    /// A stop used to be ignored until the step's grace ran out and killed
    /// it, throwing away every source loaded so far with the one
    /// transaction they shared. A stop partway through the second source
    /// keeps the first sealed, cursor and all, and the next pass reads only
    /// the second.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_keeps_the_sources_already_sealed() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        for source in ["a", "b"] {
            let docs: Vec<_> = (1..=3)
                .map(|i| doc(root, source, &format!("md-{source}{i}"), "body"))
                .collect();
            render(root, source, &docs);
        }

        let stop = StopFlag::new();
        let sources = ["a".to_string(), "b".to_string()];
        let first = build_grid_index_for(
            &pool,
            root,
            &sources,
            |m| {
                if m.starts_with("b: loaded 1/") {
                    stop.request();
                }
            },
            None,
            &stop,
        )
        .await
        .unwrap();
        assert_eq!(first.markdowns_loaded, 3, "a's three, and none of b's");
        assert_eq!(
            sealed_cursors(&pool).await,
            vec!["a".to_string()],
            "a is sealed with its cursor; b's half-load is rolled back"
        );
        assert_eq!(
            index_row_count(&pool).await,
            3,
            "b's first row did not stay"
        );

        let second = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert_eq!(
            second.markdowns_total, 3,
            "b alone is read; a's cursor held"
        );
        assert_eq!(index_row_count(&pool).await, 6);
        assert_eq!(
            sealed_cursors(&pool).await,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    /// The point of pinning the index's reads. A document the renderer has
    /// written but not committed must not reach the grid — before the pin the
    /// index read the working set and would have taken it. That is harmless
    /// while render always finishes before the index starts, and becomes a
    /// correctness bug the moment a consumer is allowed to run early: the grid
    /// would publish rows from a render still in flight, which may yet change
    /// or vanish.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_the_renderer_has_not_committed_is_not_indexed() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(root, "src", &[doc(root, "src", "md-1", "a")]);

        // A second document left in the working set, the way a render that is
        // still running (or was killed) leaves one.
        let store = IndexedMarkdownStore::open(&rendered_root(root, "src")).unwrap();
        store
            .put_document(root, &doc(root, "src", "md-2", "b"))
            .unwrap();
        store.close();

        let summary = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(
            summary.markdowns_total, 1,
            "only the committed document should have been read"
        );
        assert_eq!(
            index_row_count(&pool).await,
            1,
            "the uncommitted document must not be in the grid"
        );
    }

    /// The cursor must be recorded, and must be the store's HEAD.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_cursor_lands_in_the_index_and_names_the_store_head() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(root, "src", &[doc(root, "src", "md-1", "a")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();

        let cursors = load_source_cursors(&pool).await.unwrap();
        let recorded = cursors.get("src").expect("a cursor for src").clone();

        let store = IndexedMarkdownStore::open_for_reading(&rendered_root(root, "src"), None)
            .unwrap()
            .expect("the store has commits");
        let pin = store.pin().unwrap().clone();
        let head = store.changed_since(None, &pin).unwrap().new_head.unwrap();
        let tree = store.tree_of(&head).unwrap();
        store.close();
        assert_eq!(recorded.commit, head, "the cursor is the store's HEAD");
        assert_eq!(recorded.tree, Some(tree), "and records HEAD's tree");
    }

    /// Folds a render store's history into one commit holding HEAD's tree,
    /// on the writer's branch and `main` both, and collects the rest.
    async fn fold(root: &Path, source: &str) {
        let path = crate::indexed_markdown::path_for(&rendered_root(root, source));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(
                SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display())).unwrap(),
            )
            .await
            .unwrap();
        sqlx::query("SELECT dolt_checkout('datalib_writer')")
            .execute(&pool)
            .await
            .unwrap();
        let first: String = sqlx::query_scalar(
            "SELECT commit_hash FROM dolt_log() WHERE message = 'Initialize data repository'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("SELECT dolt_reset('--soft', ?)")
            .bind(&first)
            .execute(&pool)
            .await
            .unwrap();
        datalib_etl::doltlite_raw::commit_run(&pool, "fold")
            .await
            .unwrap()
            .expect("a fold commit");
        sqlx::query("SELECT dolt_gc()")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    /// A fold replaces every commit of a render store, the one the index's
    /// cursor names included, with one holding the same tree. The index
    /// finds that commit by the tree it recorded and reads nothing; after
    /// the next render it reads only what moved.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_fold_of_the_render_store_costs_the_index_nothing() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(
            root,
            "src",
            &[doc(root, "src", "md-1", "a"), doc(root, "src", "md-2", "b")],
        );
        render(root, "src", &[doc(root, "src", "md-2", "b-again")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        let before = load_source_cursors(&pool).await.unwrap()["src"].clone();

        fold(root, "src").await;
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_total, 0, "the fold changed no document");
        let after = load_source_cursors(&pool).await.unwrap()["src"].clone();
        assert_ne!(after.commit, before.commit);
        assert_eq!(after.tree, before.tree);

        render(root, "src", &[doc(root, "src", "md-1", "a-changed")]);
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_total, 1, "one document read, not two");
    }

    /// Only what moved is read — the untouched document stays unread,
    /// not merely unwritten.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_changed_document_is_read() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(
            root,
            "src",
            &[doc(root, "src", "md-1", "a"), doc(root, "src", "md-2", "b")],
        );
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();

        // md-2 changes; md-1 does not.
        render(root, "src", &[doc(root, "src", "md-2", "b-changed")]);
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_total, 1, "one document read, not two");
        assert_eq!(s.markdowns_loaded, 1);

        let preview: String =
            sqlx::query_scalar("SELECT preview FROM grid_rows WHERE uuid = 'md-2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(preview, "b-changed");
    }

    /// A document a source stops holding is removed. Impossible without a
    /// diff: reading whole stores sees what is present, never what left.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_the_source_dropped_is_deleted_from_the_index() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(
            root,
            "src",
            &[doc(root, "src", "md-1", "a"), doc(root, "src", "md-2", "b")],
        );
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(index_row_count(&pool).await, 2);

        unrender(root, "src", "md-2");
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_removed, 1, "the dropped document is reported");
        assert_eq!(index_row_count(&pool).await, 1, "and its rows are gone");
        let left: String = sqlx::query_scalar("SELECT uuid FROM grid_rows")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, "md-1");
    }

    /// Problems flow downstream with the data: a source's `problems`
    /// are copied into the index whole, stamps included, and a render
    /// that fixes the document clears them there too — even on an
    /// incremental run that only read the changed document.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sources_problems_are_copied_into_the_index_and_cleared_when_fixed() {
        use datalib_schema::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Stage};
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let mut bad = doc(root, "src", "md-1", "a");
        bad.problems.push(ProblemRow::new(
            "src",
            Stage::GridRow,
            Scope::Markdown("md-1"),
            Some("md-1"),
            Outcome::Nulled,
            Problem::field("created_at", Reason::CoercionFailed, "yesterday"),
            Some(1),
        ));
        render(root, "src", &[bad, doc(root, "src", "md-2", "b")]);
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.problems_copied, 1);
        let (uuid, first_seen): (String, String) =
            sqlx::query_as("SELECT problem_uuid, first_seen_at_utc FROM problems")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            !first_seen.is_empty(),
            "the render store's stamp came through"
        );
        assert!(
            super::own_problem_counts(&pool).await.unwrap().is_empty(),
            "a copy is counted by the source's step, not the index"
        );

        // Fixed: the same document renders clean. The index only reads
        // md-1 this run (the cursor names the changed document), and
        // the copy is still whole-source, so the row goes.
        render(root, "src", &[doc(root, "src", "md-1", "a fixed")]);
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_loaded, 1, "only the changed document was read");
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM problems")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "{uuid} should be gone from the index");
    }

    /// An unusable cursor must fall back to reading the store whole, not to
    /// reading nothing. "I cannot tell what changed" and "nothing changed"
    /// are the same shape from outside — an empty result — and picking the
    /// wrong one leaves the index silently frozen.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unusable_cursor_falls_back_to_reading_everything() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(root, "src", &[doc(root, "src", "md-1", "a")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();

        // A commit and a tree from no history anyone has.
        sqlx::query(
            "UPDATE source_cursors SET store_commit = ?, store_tree = ? WHERE source_id = 'src'",
        )
        .bind("0123456789abcdef0123456789abcdef01234567")
        .bind("89abcdef0123456789abcdef0123456789abcdef")
        .execute(&pool)
        .await
        .unwrap();

        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(
            s.markdowns_total, 1,
            "an unusable cursor must re-read the store, not skip it"
        );
        assert_eq!(index_row_count(&pool).await, 1);
    }

    /// A source read whole again — its cursor lost, every document
    /// re-rendered — used to delete and reinsert every row, so the search
    /// terms rewrote all of them and every index took every row again.
    /// Only a row that differs is written now.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_read_whole_again_writes_only_the_rows_that_differ() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(
            root,
            "src",
            &[doc(root, "src", "md-1", "a"), doc(root, "src", "md-2", "b")],
        );
        let first = build_grid_index(&pool, root, |_| {}, None).await.unwrap();

        render(root, "src", &[doc(root, "src", "md-2", "b, changed")]);
        sqlx::query("DELETE FROM source_cursors")
            .execute(&pool)
            .await
            .unwrap();
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_total, 2, "read whole");
        assert!(first.terms_written > 0);
        assert_eq!(
            2 * s.terms_written,
            first.terms_written,
            "md-2's terms, and not md-1's"
        );
    }

    /// One document holding a uuid twice is a renderer bug; written in
    /// place, its second row would silently overwrite its first.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_with_one_uuid_twice_fails() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let mut md = doc(root, "src", "md-1", "a");
        let mut child = md.rows[0].clone();
        child.is_document = false;
        md.rows.push(child);
        let lock = crate::grid_index::WriteLock::new(pool.clone());
        let err = crate::grid_index::apply_one(&lock, root, &md)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("two of its rows have the uuid md-1"),
            "{err:#}"
        );
    }

    /// The cold path deletes too. A store read whole is the complete
    /// answer for its source, so a document the index still holds that
    /// the store does not is gone — this used to be the one path that
    /// could never remove anything, which is how a re-keyed uuid stayed
    /// in the grid beside its replacement.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_cold_path_prunes_what_the_store_no_longer_holds() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(
            root,
            "src",
            &[doc(root, "src", "md-1", "a"), doc(root, "src", "md-2", "b")],
        );
        render(root, "other", &[doc(root, "other", "md-3", "c")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(index_row_count(&pool).await, 3);

        unrender(root, "src", "md-2");
        // Lose the range, so the next pass reads the store whole.
        sqlx::query("DELETE FROM source_cursors WHERE source_id = 'src'")
            .execute(&pool)
            .await
            .unwrap();

        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_removed, 1, "the dropped document is reported");
        let mut left: Vec<String> = sqlx::query_scalar("SELECT uuid FROM grid_rows")
            .fetch_all(&pool)
            .await
            .unwrap();
        left.sort();
        assert_eq!(
            left,
            vec!["md-1".to_string(), "md-3".to_string()],
            "src's dropped document is gone and the other source is untouched"
        );
    }

    /// A message that moves to another document takes its search terms
    /// with it. With the terms in a table of their own, owned by document,
    /// the old document kept them when only the new one re-rendered, and
    /// every full read of the store then failed (#1202).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_that_moves_takes_its_search_terms_with_it() {
        use datalib_schema::search_terms::{SearchTerm, SearchTermKind};
        let td = tempdir().unwrap();
        let root = td.path();
        let holding = |doc_uuid: &str| {
            let mut md = doc(root, "src", doc_uuid, doc_uuid);
            let mut message = md.rows[0].clone();
            message.uuid = "msg-u".to_string();
            message.is_document = false;
            message.set_supplied_terms([SearchTerm::new(SearchTermKind::Mention, "slack:U1")]);
            md.rows.push(message);
            md
        };
        render(root, "src", &[holding("md-a")]);
        render(root, "src", &[holding("md-b")]);

        let pool = index_pool(root).await;
        build_grid_index(&pool, root, |_| {}, None)
            .await
            .expect("a store whose row moved still reads whole");
        let moved: (String, Option<String>) = sqlx::query_as(
            "SELECT markdown_uuid, supplied_terms FROM grid_rows WHERE uuid = 'msg-u'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            moved,
            (
                "md-b".to_string(),
                Some(r#"[["mention","slack:U1"]]"#.to_string())
            )
        );
    }
}
