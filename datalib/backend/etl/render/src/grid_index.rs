//! The unified grid index: one table stacked from every source's render
//! store.
//!
//! Two entry points, which are the same write path from two sides.
//! [`apply_one`] writes one rendered document; [`build_grid_index`] stacks
//! every source's store into the unified grid and search index, which is
//! the `unified_grid_and_search_index` DAG step's whole job.
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
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use datalib_contact_schema::NormalizedContact;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::StoreKind;
use datalib_etl::stop::StopFlag;
use datalib_photo_schema::NormalizedPhoto;
use datalib_schema::data_nodes::{
    DataNodeRow, BY_UUID_INDEX as DATA_NODES_BY_UUID_INDEX, DDL as DATA_NODES_DDL,
};
use datalib_schema::edges::{EdgeRow, DDL as EDGES_DDL};
use datalib_schema::filings::{FilingRow, DDL as FILINGS_DDL};
use datalib_schema::grid_rows::{GridRow, DDL as GRID_ROWS_DDL, INDEXES as GRID_ROWS_INDEXES};
use datalib_schema::markdowns::DDL as MARKDOWNS_TABLE_DDL;
use datalib_schema::nodes::{DDL as NODES_DDL, PLACE_INDEX, TREE_INDEXES};
use datalib_schema::problems::{ProblemRow, Severity, DDL as PROBLEMS_DDL};
use datalib_schema::search_terms::{TERMS_DDL, TERMS_SHAPE};
use datalib_schema::source_cursors::{SourceCursorRow, DDL as SOURCE_CURSORS_DDL};
use datalib_schema::source_photos::DDL as SOURCE_PHOTOS_DDL;
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tokio::sync::Mutex;

use crate::section::Section;
use datalib_sql::{sql, sql_id, Sql};

pub(crate) mod contact_records;
mod nodes;
mod page;
mod planner_ranks;

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
    /// document an earlier run wrote" — see `page::write_grid_rows`.
    written: std::sync::Mutex<std::collections::HashSet<String>>,
    /// What this run changed that the grid index derives from; `None` in a
    /// render store, which derives nothing.
    changed: Option<std::sync::Mutex<Changed>>,
    /// How a batch starts. The grid index's file has other writers (the
    /// search index's steps), and a batch that read before its first write
    /// would fail rather than wait if one of them committed meanwhile.
    begin: &'static str,
}

/// Which of the two stores a document is written to. They hold its people
/// differently: a render store keeps each one's whole record under the
/// document, so a diff of it names the document; the grid index keeps a
/// record once (`contact_records`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Store {
    Render,
    Index,
}

#[derive(Debug, Default)]
struct Changed {
    /// The grid rows written or deleted since the search terms last
    /// followed them.
    rows: Vec<String>,
    /// The nodes whose accounts changed, or whose grid row left its
    /// document, since `nodes` last followed them.
    nodes: std::collections::BTreeSet<String>,
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
            begin: "BEGIN",
        }
    }

    /// The grid index's lock, which keeps what its writes change for the
    /// nodes and the search terms to follow.
    pub fn for_index(pool: SqlitePool) -> Self {
        Self {
            changed: Some(Default::default()),
            begin: "BEGIN IMMEDIATE",
            ..Self::new(pool)
        }
    }

    fn note_changed(&self, rows: impl IntoIterator<Item = String>) {
        if let Some(changed) = &self.changed {
            changed.lock().unwrap().rows.extend(rows);
        }
    }

    fn touch_nodes(&self, uuids: impl IntoIterator<Item = String>) {
        if let Some(changed) = &self.changed {
            changed.lock().unwrap().nodes.extend(uuids);
        }
    }

    /// Grid rows that are gone from their document, or from the index: a
    /// row may have been the one that told of its node.
    fn note_gone(&self, rows: Vec<String>) {
        self.touch_nodes(rows.iter().cloned());
        self.note_changed(rows);
    }

    fn changed_rows(&self) -> usize {
        self.changed
            .as_ref()
            .map_or(0, |c| c.lock().unwrap().rows.len())
    }

    fn take_changed(&self) -> Vec<String> {
        self.changed
            .as_ref()
            .map_or_else(Vec::new, |c| std::mem::take(&mut c.lock().unwrap().rows))
    }

    fn take_touched_nodes(&self) -> Vec<String> {
        self.changed.as_ref().map_or_else(Vec::new, |c| {
            std::mem::take(&mut c.lock().unwrap().nodes)
                .into_iter()
                .collect()
        })
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

    fn store(&self) -> Store {
        match self.changed {
            Some(_) => Store::Index,
            None => Store::Render,
        }
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
        Sql::dynamic(sql_id!("etl.render.grid.begin_write_batch"), self.begin)
            .query()
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
        sql!("etl.render.grid.commit_write_batch", "COMMIT")
            .query()
            .execute(&mut *conn)
            .await
            .context("WriteLock: COMMIT")?;
        Ok(())
    }

    pub async fn rollback_transaction(&self) -> Result<()> {
        // What the rolled-back writes changed was never written.
        self.take_changed();
        self.take_touched_nodes();
        let mut inner = self.inner.lock().await;
        let Some(mut conn) = inner.tx_conn.take() else {
            return Ok(());
        };
        sql!("etl.render.grid.rollback_write_batch", "ROLLBACK")
            .query()
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
    /// Search terms written for the rows this run changed.
    pub terms_written: usize,
    /// Sources whose load or removal changed so much of the index that
    /// the search indexes were dropped and made again around it.
    pub search_indexes_remade: usize,
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

pub use datalib_schema::problems::INDEX_OWN_SCOPE_KEY as UNREADABLE_STORE_KEY;

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
    let rows = sql!(
        "etl.render.grid.count_unreadable_store_problems",
        "SELECT severity, COUNT(*) FROM problems \
         WHERE scope_kind = ? AND scope_key = ? GROUP BY severity"
    )
    .query()
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

/// Drop every row the index holds for `source_id`: its own row about a
/// render store it could not read, now read. Whether anything went.
pub(crate) async fn clear_source_problems(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    source_id: &str,
) -> Result<bool> {
    let gone = sql!(
        "etl.render.grid.delete_problems_of_source",
        "DELETE FROM problems WHERE source_id = ?"
    )
    .query()
    .bind(source_id)
    .execute(&mut **conn)
    .await
    .context("clear the source's problems")?
    .rows_affected();
    Ok(gone > 0)
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

const OTHER_SHAPE: &str = "its render store is in another shape; sync this source to re-render it";

async fn record_unreadable_store(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    row: ProblemRow,
    now: &datalib_time::StoredStamp,
) -> Result<()> {
    let earlier = sql!(
        "etl.render.grid.find_unreadable_store_problem",
        "SELECT * FROM problems WHERE problem_uuid = ?"
    )
    .query()
    .bind(&row.problem_uuid)
    .fetch_optional(&mut **conn)
    .await
    .context("read the unreadable-store warning")?
    .map(|r| ProblemRow::from_row(&r))
    .transpose()?;
    let row = row.stamped(earlier.as_ref(), &now.utc, now.tz_offset.as_deref());
    let row = &row;
    sql!(
        "etl.render.grid.delete_unreadable_store_problem",
        "DELETE FROM problems WHERE problem_uuid = ?"
    )
    .query()
    .bind(&row.problem_uuid)
    .execute(&mut **conn)
    .await
    .context("clear the unreadable-store warning")?;
    // Audited: `insert_sql` is built from `ProblemRow`'s associated
    // consts, never from row data; all values bound.
    row.bind_into(
        Sql::dynamic(
            sql_id!("etl.render.grid.insert_unreadable_store_problem"),
            datalib_etl::bulk::insert_sql::<ProblemRow>(),
        )
        .query(),
    )
    .execute(&mut **conn)
    .await
    .with_context(|| format!("record that {}'s render store is unreadable", row.source_id))?;
    // A fan-in comes here once per producer checkpoint, for as long as the
    // store stays this way; the row says so meanwhile.
    if row.is_news(earlier.as_ref()) {
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

/// The parts of the grid index's file whose tables move alone. A build
/// that changes one part's tables empties that part, and the step fills it
/// again from the render stores; the other part keeps its rows, and its
/// readers keep reading them. Each part records the hash of its tables in
/// the file's `_datalib_meta` beside the whole file's [`shape`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum Part {
    /// The rows, documents, edges, nodes, filings, photos, problems,
    /// cursors and terms. Emptying it empties [`Part::Contacts`] too: the
    /// people are kept per grid document, and a document the refill no
    /// longer finds would leave its people behind.
    Grid,
    /// The people the documents carry (`contact_records`), with a cursor
    /// per source of their own, so the step fills them alone.
    Contacts,
    /// The keyword index's `search_*` tables, which the step's keyword
    /// pass fills and keyword search reads (`datalib_search_index`).
    Keywords,
}

impl Part {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The hash of this part's tables, as its writer records it.
    pub fn tables(self) -> String {
        match self {
            Part::Grid => {
                datalib_store_meta::schema_hash(grid_ddl().chain([TERMS_SHAPE, nodes::NODES_SHAPE]))
            }
            Part::Contacts => datalib_store_meta::schema_hash(contact_records::DDL.iter().copied()),
            Part::Keywords => datalib_store_meta::schema_hash(datalib_search_index::KEYWORD_DDL),
        }
    }

    /// Whether `table` is this part's, by its name, an older build's
    /// included.
    fn holds(self, table: &str) -> bool {
        let part = if table.starts_with(datalib_search_index::TABLE_PREFIX) {
            Part::Keywords
        } else if contact_records::holds(table) {
            Part::Contacts
        } else {
            Part::Grid
        };
        part == self
    }

    /// The shape a reader of only this part requires: its tables, kept in
    /// line by the writer alone, with no ladder and no indexes apart.
    fn shape(self) -> datalib_store_meta::Shape {
        datalib_store_meta::Shape::of_tables(self.tables())
    }
}

fn parts() -> Vec<(&'static str, String)> {
    <Part as strum::VariantArray>::VARIANTS
        .iter()
        .map(|p| (p.as_str(), p.tables()))
        .collect()
}

/// What opening a file that records `meta` must do to bring it to this
/// build's tables.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refill {
    Nothing,
    /// Empty these parts, which the step then fills again.
    Parts(Vec<Part>),
    /// Delete the file: every part moved, or it records no parts (written
    /// before they were recorded, or by no build at all).
    Whole,
}

fn refill(meta: Option<&datalib_store_meta::Meta>) -> Refill {
    let Some(meta) = meta else {
        return Refill::Whole;
    };
    if meta.schema_hash == shape().tables {
        return Refill::Nothing;
    }
    let all = <Part as strum::VariantArray>::VARIANTS;
    let mut moved: Vec<Part> = all
        .iter()
        .copied()
        .filter(|p| meta.parts.get(p.as_str()) != Some(&p.tables()))
        .collect();
    if moved.contains(&Part::Grid) && !moved.contains(&Part::Contacts) {
        moved.push(Part::Contacts);
        moved.sort_by_key(|p| all.iter().position(|q| q == p));
    }
    // No part moved and yet the whole did: the parts it records do not
    // explain it, so nothing in it is kept.
    if moved.is_empty() || moved.len() == all.len() {
        return Refill::Whole;
    }
    Refill::Parts(moved)
}

/// The grid index's shape: what its writer records and every reader
/// requires before it reads a row. Its indexes are kept apart from its
/// tables, so adding one costs creating it rather than a rebuild.
pub fn shape() -> datalib_store_meta::Shape {
    datalib_store_meta::Shape {
        indexes: Some(datalib_store_meta::schema_hash(
            indexes().chain(planner_ranks::statements().iter().map(String::as_str)),
        )),
        ..shape_without_indexes()
    }
}

/// [`shape`] while a build into an empty index has its search indexes
/// dropped: the tables are there, the indexes are not, and a reader that
/// requires [`shape`] waits for them.
fn shape_without_indexes() -> datalib_store_meta::Shape {
    datalib_store_meta::Shape {
        tables: datalib_store_meta::schema_hash(
            index_ddl()
                // What the terms and the nodes derive, which no DDL shows.
                .chain([TERMS_SHAPE, nodes::NODES_SHAPE]),
        ),
        indexes: None,
        versions: datalib_store_meta::Versions {
            schema: 0,
            // Its `problems` is built in this shape, so it starts at the
            // shared ladder's top.
            shared: datalib_store_meta::ladder::top(datalib_etl::doltlite_raw::SHARED_LADDER),
        },
    }
}

/// Every index of the grid index's own tables.
fn indexes() -> impl Iterator<Item = &'static str> {
    read_indexes()
        .chain(
            DOCUMENT_LOOKUP_INDEXES
                .iter()
                .copied()
                .filter(|ddl| *ddl != HANDLES_BY_HANDLE),
        )
        .chain(contact_records::INDEXES.iter().copied())
        .chain([AUTHORS_INDEX, DATA_NODES_BY_UUID_INDEX])
        .chain(SEARCHABLE_DOCUMENTS_INDEXES.iter().copied())
}

async fn record_shape(pool: &SqlitePool, shape: &datalib_store_meta::Shape) -> Result<()> {
    datalib_store_meta::write(pool, StoreKind::GridIndex, shape)
        .await
        .context("write _datalib_meta for the grid index")?;
    datalib_store_meta::write_parts(pool, &parts())
        .await
        .context("write the grid index's parts to _datalib_meta")?;
    Ok(())
}

/// Only a render store's, and nothing reads it: it keeps its place among
/// the indexes below because a render store's recorded shape is a hash of
/// them in order.
const HANDLES_BY_HANDLE: &str =
    "CREATE INDEX IF NOT EXISTS source_contact_handles_by_handle ON source_contact_handles (handle)";

/// What replacing one document looks its old rows up by, in every store
/// that holds documents. Without them each document's `DELETE` scans the
/// whole table, and a full render is quadratic in the store's size.
/// Apart from `GRID_ROWS_INDEXES`, which are the search bar's.
pub(crate) const DOCUMENT_LOOKUP_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS grid_rows_by_markdown ON grid_rows (markdown_uuid)",
    "CREATE INDEX IF NOT EXISTS edges_by_src_markdown ON edges (src_markdown_uuid)",
    HANDLES_BY_HANDLE,
    // The photos applet asks for one photo by its id.
    "CREATE INDEX IF NOT EXISTS source_photos_by_photo ON source_photos (photo_uuid)",
];

/// What [`crate::search_terms::write_people`] reads the grid's authors
/// from, so a pass reads two short columns rather than every row's text
/// (#1803). Only the index's, and never dropped for a remake, since the
/// query names it.
pub(crate) const AUTHORS_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS grid_rows_by_author ON grid_rows (author_handle, author)";

/// For listing a search's searchable documents from indexes alone. No
/// statement reads by them now: they are kept for a count of the documents
/// search by meaning cannot reach, which no answer may wait for.
pub(crate) const SEARCHABLE_DOCUMENTS_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS markdowns_searchable ON markdowns (searchable, markdown_uuid)",
    "CREATE INDEX IF NOT EXISTS grid_rows_by_markdown_source ON grid_rows (markdown_uuid, source_id)",
];

/// The grid index's tables, and the terms' with their indexes, then the
/// contacts' and the keyword index's.
fn index_ddl() -> impl Iterator<Item = &'static str> {
    grid_ddl()
        .chain(contact_records::DDL.iter().copied())
        .chain(datalib_search_index::KEYWORD_DDL)
}

/// [`Part::Grid`]'s tables.
fn grid_ddl() -> impl Iterator<Item = &'static str> {
    GRID_ROWS_DDL
        .iter()
        .map(|(_table, ddl)| *ddl)
        .chain(std::iter::once(MARKDOWNS_DDL))
        .chain(EDGES_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(DATA_NODES_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(NODES_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(FILINGS_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(SOURCE_PHOTOS_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(PROBLEMS_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(SOURCE_CURSORS_DDL.iter().map(|(_table, ddl)| *ddl))
        .chain(std::iter::once(datalib_etl::pin::GENERATION_DDL))
        .chain(TERMS_DDL.iter().copied())
}

/// Create whatever of the index is missing: on a new file, all of it;
/// after a build stopped partway, the search indexes it left out; and drop
/// the indexes an earlier build made that this one does not. Only
/// here: every render store has a `grid_rows` too, and only the index the
/// grid reads wants to pay for the search indexes on every write.
pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
    drop_other_indexes(pool).await?;
    let conn = &mut *pool.acquire().await?;
    planner_ranks::write(conn).await?;
    for ddl in index_ddl().chain(indexes()) {
        Sql::dynamic(sql_id!("etl.render.grid.create_index_schema"), ddl)
            .query()
            .execute(&mut *conn)
            .await
            .with_context(|| format!("create: {ddl}"))?;
    }
    planner_ranks::load(conn).await
}

/// Drop each index of the file that this build does not make: one it
/// retired, which every write would go on paying for, or one it makes over
/// other columns, which `IF NOT EXISTS` would leave as it was.
async fn drop_other_indexes(pool: &SqlitePool) -> Result<()> {
    // SQLite keeps a statement as written, less its `IF NOT EXISTS`.
    let made: HashSet<String> = index_ddl()
        .chain(indexes())
        .map(|ddl| ddl.replacen("IF NOT EXISTS ", "", 1))
        .collect();
    let held: Vec<(String, String)> = sql!(
        "etl.render.grid.list_indexes",
        "SELECT name, sql FROM sqlite_master WHERE type = 'index' AND sql NOTNULL"
    )
    .query_as()
    .fetch_all(pool)
    .await
    .context("list the grid index's indexes")?;
    for (name, _) in held.iter().filter(|(_, sql)| !made.contains(sql)) {
        tracing::info!(index = %name, "dropping an index this build does not make");
        let quoted = datalib_etl::doltlite_raw::quote_ident(name);
        // Audited: the name is quoted.
        Sql::dynamic(
            sql_id!("etl.render.grid.drop_index_not_made"),
            format!("DROP INDEX {quoted}"),
        )
        .query()
        .execute(pool)
        .await
        .with_context(|| format!("drop {name}"))?;
    }
    Ok(())
}

/// The search bar's indexes, and those that read the tree.
fn read_indexes() -> impl Iterator<Item = &'static str> {
    GRID_ROWS_INDEXES
        .iter()
        .map(|(_table, ddl)| *ddl)
        .chain(TREE_INDEXES.iter().copied())
        .chain([PLACE_INDEX])
}

/// The read indexes, which no write looks anything up by, are left out of
/// a build into an empty index
/// and made once at its end, and remade around a source's rewrite of a
/// large share of the index. Kept up row by row, each changed row costs
/// a page in every one of them, more pages than SQLite's cache holds;
/// built at the end they are sorted and contiguous.
async fn drop_search_indexes(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    for ddl in read_indexes() {
        let name = index_name(ddl).with_context(|| format!("no index name in {ddl}"))?;
        // Audited: `name` is parsed out of our own static index DDL.
        Sql::dynamic(
            sql_id!("etl.render.grid.drop_read_index"),
            format!("DROP INDEX IF EXISTS {name}"),
        )
        .query()
        .execute(&mut *conn)
        .await
        .with_context(|| format!("drop {name}"))?;
    }
    Ok(())
}

async fn create_search_indexes(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    planner_ranks::write(conn).await?;
    for ddl in read_indexes() {
        Sql::dynamic(sql_id!("etl.render.grid.create_read_index"), ddl)
            .query()
            .execute(&mut *conn)
            .await
            .with_context(|| format!("create index: {ddl}"))?;
    }
    planner_ranks::load(conn).await
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
/// index held, and had changed, when it began.
struct RemakeProbe {
    held_rows: i64,
    changed_before: usize,
}

impl RemakeProbe {
    async fn start(write_lock: &WriteLock) -> Result<Self> {
        let mut guard = write_lock.acquire().await?;
        let held_rows: i64 = sql!(
            "etl.render.grid.count_rows_before_load",
            "SELECT count(*) FROM grid_rows"
        )
        .query_scalar()
        .fetch_one(&mut **guard.conn())
        .await
        .context("measure the index before a large load")?;
        Ok(Self {
            held_rows,
            changed_before: write_lock.changed_rows(),
        })
    }

    /// Drop the search indexes if the load so far says to; whether it did.
    async fn decide(
        &self,
        write_lock: &WriteLock,
        applied: usize,
        applying: usize,
    ) -> Result<bool> {
        let changed = write_lock.changed_rows() - self.changed_before;
        if !remakes_search_indexes(changed, applied, applying, self.held_rows) {
            return Ok(false);
        }
        self.drop_for_remake(write_lock, applied, applying, changed)
            .await
    }

    /// Drop the search indexes if removing `markdown_uuids` takes enough
    /// of the index's rows; whether it did. Removals are counted exactly,
    /// not projected: their rows are already in the index.
    async fn decide_removal(
        &self,
        write_lock: &WriteLock,
        markdown_uuids: &[String],
    ) -> Result<bool> {
        let rows: i64 = {
            let mut guard = write_lock.acquire().await?;
            sql!(
                "etl.render.grid.count_rows_of_documents",
                "SELECT count(*) FROM grid_rows \
                 WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
            )
            .query_scalar()
            .bind(serde_json::to_string(markdown_uuids)?)
            .fetch_one(&mut **guard.conn())
            .await
            .context("count the rows a removal takes")?
        };
        let removing = markdown_uuids.len();
        if !remakes_search_indexes(rows.max(0) as usize, removing, removing, self.held_rows) {
            return Ok(false);
        }
        self.drop_for_remake(write_lock, removing, removing, rows.max(0) as usize)
            .await
    }

    async fn drop_for_remake(
        &self,
        write_lock: &WriteLock,
        applied: usize,
        applying: usize,
        changed: usize,
    ) -> Result<bool> {
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
        sql!(
            "etl.render.grid.vacuum_dropped_index_pages",
            "PRAGMA incremental_vacuum"
        )
        .query()
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

/// The step that writes the grid index, and that a refusal names.
pub fn step() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_GRID_AND_SEARCH_INDEX_DIR,
        datalib_core::layout::INDEX_STEP_DIR
    )
}

/// Refuses the grid index on `conn` unless it records this build's
/// [`shape`], inside whatever read the caller holds on `conn`.
pub async fn require_shape(conn: &mut sqlx::SqliteConnection, db_path: &Path) -> Result<()> {
    let meta = datalib_store_meta::read_in(conn).await?;
    datalib_store_meta::require(db_path, Some(&step()), meta.as_ref(), &shape())
}

/// Refuses the keyword index's tables in the file on `conn` unless they
/// are this build's: all keyword search reads, so it waits on nothing the
/// grid's part does, such as a rebuild of the grid.
async fn require_keywords(conn: &mut sqlx::SqliteConnection, db_path: &Path) -> Result<()> {
    let meta = datalib_store_meta::read_in(conn).await?;
    let part = Part::Keywords;
    let have = meta
        .as_ref()
        .and_then(|m| m.parts.get(part.as_str()))
        .map(|tables| datalib_store_meta::Shape::of_tables(tables.clone()));
    datalib_store_meta::require_recorded(db_path, Some(&step()), have, &part.shape())
}

/// Whether the keyword index's tables can be read: the file is made and
/// they are in this build's shape, whatever the grid's part is doing.
/// `false` before the first pass; another shape is refused, saying what to
/// run.
pub async fn keywords_readable(db_path: &Path) -> Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }
    let pool = datalib_etl::pin::open_reader(db_path)
        .await
        .with_context(|| format!("open the grid index {}", db_path.display()))?;
    let checked = async {
        if datalib_etl::pin::generation(&pool).await?.is_none() {
            return anyhow::Ok(false);
        }
        require_keywords(&mut *pool.acquire().await?, db_path).await?;
        anyhow::Ok(true)
    }
    .await;
    pool.close().await;
    checked
}

/// The grid index read-only, once its first pass has made it and only in
/// this build's [`shape`]: the one way to open it for reading outside the
/// search applet's repo, which checks the same shape inside each read.
/// `None` before the first pass; a store in another shape is refused,
/// saying what to run.
pub async fn open_for_reading(db_path: &Path) -> Result<Option<SqlitePool>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let pool = datalib_etl::pin::open_reader(db_path)
        .await
        .with_context(|| format!("open the grid index {}", db_path.display()))?;
    let checked = async {
        if datalib_etl::pin::generation(&pool).await?.is_none() {
            return anyhow::Ok(false);
        }
        require_shape(&mut *pool.acquire().await?, db_path).await?;
        anyhow::Ok(true)
    }
    .await;
    match checked {
        Ok(true) => Ok(Some(pool)),
        other => {
            pool.close().await;
            other.map(|_| None)
        }
    }
}

/// The `unified_grid_and_search_index` step's handle on the index: the one way to open it
/// for writing. One connection, never recycled, as every pool here
/// (`etl/README.md` § "Connection pools"). A reader that looks before the
/// generation row is made reads the index as not built yet.
///
/// A [`Part`] written in another shape loses its tables and is made new:
/// every row in it is a function of a render store, so refilling it costs
/// one local read of them, where altering it in place would leave a new
/// column empty in the rows the cursors never revisit. Its cursors are its
/// own tables and go with it, so the step's next pass reads every source
/// whole for that part alone. When every part moved the file is deleted
/// instead, which is quicker than dropping each table.
pub async fn open_index(db_path: &Path) -> Result<SqlitePool> {
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let refill = refill_of(db_path).await?;
    if refill == Refill::Whole {
        remove_index_files(db_path)?;
    }
    let pool = connect(db_path).await?;
    let made = async {
        if let Refill::Parts(parts) = &refill {
            empty_parts(&pool, parts).await?;
        }
        set_up(&pool).await
    }
    .await;
    match made {
        Ok(()) => Ok(pool),
        Err(e) => {
            pool.close().await;
            Err(e)
        }
    }
}

/// Opens the index as its owner when some of its parts are in another
/// shape and the rest are this build's, which empties the moved parts and
/// keeps the rest readable while the step fills them again: what the
/// launch's prepare pass does before it asks for that run. Whether it did.
pub async fn empty_moved_parts(db_path: &Path) -> Result<bool> {
    if !matches!(refill_of(db_path).await?, Refill::Parts(_)) {
        return Ok(false);
    }
    close_index(open_index(db_path).await?).await?;
    Ok(true)
}

/// What [`open_index`] has to do to the file at `db_path`.
async fn refill_of(db_path: &Path) -> Result<Refill> {
    if !db_path.exists() {
        return Ok(Refill::Nothing);
    }
    let pool = datalib_etl::pin::open_reader(db_path)
        .await
        .with_context(|| format!("open the grid index {}", db_path.display()))?;
    let meta = datalib_store_meta::read(&pool).await;
    pool.close().await;
    Ok(refill(
        meta.context("read the grid index's shape")?.as_ref(),
    ))
}

/// Deletes the file, its write-ahead log and its shared-memory file. One
/// process writes the index at a time (the step, or its `--prepare` before
/// the loop starts); a reader with the old file open reads it until it
/// opens the new one.
fn remove_index_files(db_path: &Path) -> Result<()> {
    tracing::info!(
        file = %db_path.display(),
        "the grid index was written in another shape; deleting it to build it again"
    );
    // The log first: a log left beside a new file would be replayed into it.
    for suffix in ["-wal", "-shm", ""] {
        let mut path = db_path.as_os_str().to_owned();
        path.push(suffix);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("delete {}", Path::new(&path).display()))
            }
        }
    }
    Ok(())
}

/// Drops every table of `parts`, an older build's included, in one
/// transaction; [`set_up`] then makes them in this build's shape. Their
/// pages stay in the file, for the refill to take back: handing them to the
/// disk is what made dropping a whole index's tables take eleven minutes.
/// A grid kept whole has its generation raised, since what its keyword
/// filter reads went.
async fn empty_parts(pool: &SqlitePool, parts: &[Part]) -> Result<()> {
    let started = Instant::now();
    let mut conn = pool.acquire().await?;
    sql!("etl.render.grid.begin_emptying_parts", "BEGIN IMMEDIATE")
        .query()
        .execute(&mut *conn)
        .await
        .context("begin emptying the grid index's moved parts")?;
    let emptied = async {
        // Virtual tables first, so their shadow tables go with them.
        for virtual_only in [true, false] {
            let names: Vec<String> = sql!(
                "etl.render.grid.list_tables_to_empty",
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                   AND name NOT LIKE 'sqlite_%' AND name <> '_datalib_meta' \
                   AND (NOT ? OR sql LIKE 'CREATE VIRTUAL TABLE%')"
            )
            .query_scalar()
            .bind(virtual_only)
            .fetch_all(&mut *conn)
            .await
            .context("list the grid index's tables")?;
            for name in names.iter().filter(|n| parts.iter().any(|p| p.holds(n))) {
                let quoted = datalib_etl::doltlite_raw::quote_ident(name);
                // Audited: the name is quoted.
                Sql::dynamic(
                    sql_id!("etl.render.grid.drop_table_of_moved_part"),
                    format!("DROP TABLE IF EXISTS {quoted}"),
                )
                .query()
                .execute(&mut *conn)
                .await
                .with_context(|| format!("drop {name}"))?;
            }
        }
        if !parts.contains(&Part::Grid) {
            bump_generation(&mut conn).await?;
        }
        anyhow::Ok(())
    }
    .await;
    let end = match emptied {
        Ok(()) => sql!("etl.render.grid.commit_emptied_parts", "COMMIT"),
        Err(_) => sql!("etl.render.grid.roll_back_emptying_parts", "ROLLBACK"),
    };
    let ended = end.query().execute(&mut *conn).await;
    emptied?;
    ended.context("commit the emptied parts")?;
    tracing::info!(
        parts = %parts.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", "),
        took_ms = started.elapsed().as_millis() as u64,
        "the grid index's tables of these parts were in another shape; \
         emptied them for the step to fill again, keeping the rest"
    );
    Ok(())
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
        .pragma(
            "journal_size_limit",
            datalib_core::plain_sqlite::WAL_SIZE_LIMIT_BYTES,
        )
        // The step's keyword pass writes the search index's tables in this
        // file too, a batch of documents at a time.
        .busy_timeout(Duration::from_secs(5 * 60));
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
    record_shape(pool, &shape()).await?;
    sql!(
        "etl.render.grid.insert_initial_generation",
        "INSERT INTO store_generation (id, file_id, generation) \
         VALUES (1, lower(hex(randomblob(8))), 0) ON CONFLICT (id) DO NOTHING"
    )
    .query()
    .execute(pool)
    .await
    .context("start the grid index's generation")?;
    Ok(())
}

/// Copy the WAL back into the index and empty it, then close. A
/// checkpoint by itself leaves the `-wal` at the size its largest
/// transaction gave it (`docs/dev/doltlite.md`), and while the search
/// applet holds the file open, closing the writer's connection does not
/// empty it either.
pub async fn close_index(pool: SqlitePool) -> Result<()> {
    let checkpoint: Result<(i64, i64, i64), sqlx::Error> = sql!(
        "etl.render.grid.checkpoint_wal",
        "PRAGMA wal_checkpoint(TRUNCATE)"
    )
    .query_as()
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
/// changed what a reader sees, and return it. This is the index's seal,
/// in place of a doltlite commit.
async fn bump_generation(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) -> Result<i64> {
    sql!(
        "etl.render.grid.bump_generation",
        "UPDATE store_generation SET generation = generation + 1 WHERE id = 1 \
         RETURNING generation"
    )
    .query_scalar()
    .fetch_one(&mut **conn)
    .await
    .context("raise the grid index's generation")
}

/// Write the terms of what `write_lock` changed outside a pass — with
/// [`apply_one`] and [`delete_markdowns`] outside a transaction, as tests
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
        "Notion Table of Contents" => "document",
        // The per-source storage report: datalib describing a mirror,
        // not anything the upstream authored.
        "Source Size" => datalib_schema::measurements::DOC_KIND,
        "Photo" | "Video" => "photo",
        _ => "chat",
    }
}

/// One document as handed from render to the render store and the index.
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
    /// The render unit this document was rendered from — the same key the
    /// provider declares through `RenderCtx::declare_render_unit`. `None`
    /// from a renderer that does not declare render units yet.
    pub render_unit_key: Option<String>,
    pub render_version: u32,
    /// Empty means there is no document: the store drops it, keeping only
    /// its `problems`.
    pub rows: Vec<GridRow>,
    /// The document section by section, in order, each keyed by the
    /// `data-section-uuid` it wraps or unkeyed when it wraps none —
    /// concatenated they are the document's markdown. Empty when read
    /// back from the store for indexing.
    pub sections: Vec<Section>,
    /// Whether the search indexes it: false for a document kept for the
    /// grid alone (Notion's table of contents), which only repeats what
    /// other documents say at a size that would cost real embedding time.
    pub searchable: bool,
    /// The `content_hash` the store holds for this document, when it is
    /// read back; `None` from a renderer, whose sections give it.
    pub content_hash: Option<String>,
    /// Its front matter as facts: what its first section draws, kept as
    /// data (`markdowns.front`).
    pub front: crate::front_matter::Front,
    /// Outgoing edges (`src_markdown_uuid == markdown_uuid`). Empty for
    /// renderers that don't emit edges; the DELETE still runs, so stale rows
    /// from a previous render get cleaned up.
    pub edges: Vec<EdgeRow>,
    /// The containers this document's rows live in, from its own up to the
    /// top of the source; owned by the document like its contacts, so a
    /// channel named by many threads has a row under each.
    pub data_nodes: Vec<DataNodeRow>,
    /// What its rows and containers are filed under besides their parent.
    pub filings: Vec<FilingRow>,
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

/// Why a document cannot be indexed: it names no source, or it holds a
/// row of another document. `None` when it can.
fn misfiled(md: &RenderedMarkdown) -> Option<String> {
    if md.source_id.is_empty() {
        return Some(format!(
            "document {} names no source: its renderer must set \
             `RenderedMarkdown::source_id` to its group's id",
            md.markdown_uuid
        ));
    }
    md.rows
        .iter()
        .find(|r| r.markdown_uuid.as_deref() != Some(md.markdown_uuid.as_str()))
        .map(|r| {
            format!(
                "row {} of document {} names {:?} as its document",
                r.uuid, md.markdown_uuid, r.markdown_uuid
            )
        })
}

/// Write one rendered document into the index unconditionally. Every
/// document reaches a render store or the grid index through here, so the
/// names in it are cleaned and its rows filed under their source here
/// ([`crate::names`]).
pub async fn apply_one(write_lock: &WriteLock, md: &RenderedMarkdown) -> Result<usize> {
    apply_page(write_lock, std::slice::from_ref(md)).await
}

/// [`apply_one`] for a page of documents, each table's rows in a few
/// statements: what it writes is what applying them one after another, in
/// order, would. Returns how many grid rows the page holds.
pub async fn apply_page(write_lock: &WriteLock, page: &[RenderedMarkdown]) -> Result<usize> {
    page::apply_page(write_lock, page).await
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
/// `unified_grid_and_search_index` DAG step's whole job. The step passes the groups its
/// declared inputs name, so a source dropped from the config stops being
/// read on the next run even while its tree is still on disk, and its
/// documents leave the index — and a directory that is not in the config
/// is never read at all. A listed
/// source with no store yet is skipped: its render step has not produced
/// one.
///
/// **Each source's store is diffed, not read whole**, via `dolt_diff`
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
    let contact_cursors = contact_records::cursors(pool).await?;
    let indexed = load_markdown_uuids_by_source(pool).await?;
    let now = run_stamp(now_override);
    // A pass stopped before the end leaves them to the next open, whose
    // `init_schema` makes them.
    let building = indexed.is_empty();
    if building {
        record_shape(pool, &shape_without_indexes()).await?;
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
        let reading = Instant::now();
        let read = match read_source(
            &stanza,
            &rendered_root,
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
        // The load reads the store a page at a time; it is closed once the
        // source's transaction ends, however it ends.
        let open = match &read {
            SourceRead::Read { store, .. } => Some(std::sync::Arc::clone(store)),
            _ => None,
        };
        let applied = if stopped_before(stop, &stanza) {
            drop(read);
            Ok(false)
        } else {
            let read_ms = millis(reading.elapsed());
            // The contacts were made new while the grid kept this source:
            // the load fills them alone before it applies what changed.
            let contacts_owed = cursors
                .get(&stanza)
                .is_some_and(|c| contact_cursors.get(&stanza) != Some(&c.commit));
            apply_source(
                &write_lock,
                &stanza,
                read,
                contacts_owed,
                read_ms,
                (!building).then_some(held),
                &now,
                &progress,
                stop,
                &mut summary,
            )
            .await
        };
        if let Some(store) = open {
            close_store(store);
        }
        // A page the load could not read costs only its own source, as a
        // read that fails before the load does: its transaction rolled
        // back, the source keeps its rows and gets the problem.
        let applied = match applied {
            Err(e) if e.downcast_ref::<UnreadPage>().is_some() => {
                let why = format!("{e:#}");
                tracing::error!(
                    source = %stanza,
                    error = %why,
                    "could not read this render store; indexing the other \
                     sources and leaving this one as it was"
                );
                apply_source(
                    &write_lock,
                    &stanza,
                    SourceRead::Failed(why),
                    false,
                    0,
                    (!building).then_some(held),
                    &now,
                    &progress,
                    stop,
                    &mut summary,
                )
                .await?
            }
            other => other?,
        };
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
            let remade =
                retire_source(&write_lock, source, indexed.get(source), held, &progress).await?;
            summary.search_indexes_remade += usize::from(remade);
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
        record_shape(pool, &shape()).await?;
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
/// so a source added back later is read whole. `true` when the removal
/// dropped the search indexes and made them again.
async fn retire_source(
    write_lock: &WriteLock,
    source: &str,
    documents: Option<&HashSet<String>>,
    held: usize,
    progress: &impl Fn(&str),
) -> Result<bool> {
    write_lock
        .begin_transaction()
        .await
        .with_context(|| format!("begin the transaction retiring {source}"))?;
    let res = async {
        let documents: Vec<String> = documents.into_iter().flatten().cloned().collect();
        let remade = remove_documents(write_lock, source, &documents, Some(held), progress).await?;
        let mut guard = write_lock.acquire().await?;
        let conn = guard.conn();
        nodes::rewrite_touched(conn, write_lock)
            .await
            .with_context(|| format!("drop {source}'s nodes"))?;
        if remade {
            create_search_indexes(conn).await?;
        }
        crate::search_terms::write_rows(conn, &write_lock.take_changed()).await?;
        clear_source_problems(conn, source).await?;
        sql!(
            "etl.render.grid.delete_cursor_of_retired_source",
            "DELETE FROM source_cursors WHERE source_id = ?"
        )
        .query()
        .bind(source)
        .execute(&mut **conn)
        .await
        .with_context(|| format!("clear {source}'s cursor"))?;
        contact_records::set_cursor(conn, source, None).await?;
        Ok((bump_generation(conn).await?, remade))
    }
    .await;
    let (generation, remade) = match res {
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
    Ok(remade)
}

/// The people the terms name, read from every source at once: once a
/// pass that changed anything, in a seal of its own.
async fn refresh_people(write_lock: &WriteLock) -> Result<()> {
    let mut laps = Laps::start();
    write_lock.begin_transaction().await?;
    let res = async {
        let mut guard = write_lock.acquire().await?;
        let people = crate::search_terms::write_people(guard.conn()).await?;
        bump_generation(guard.conn()).await?;
        Ok::<_, anyhow::Error>(people)
    }
    .await;
    match res {
        Ok(people) => {
            let write_ms = laps.lap();
            write_lock.commit_transaction().await?;
            tracing::debug!(
                authors_ms = people.authors_ms,
                contacts_ms = people.contacts_ms,
                names = people.names,
                write_ms,
                commit_ms = laps.lap(),
                "wrote the names people are known by"
            );
            Ok(())
        }
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
        /// The store, open at the commit read, which the load reads a page
        /// of documents at a time.
        store: std::sync::Arc<crate::indexed_markdown::IndexedMarkdownStore>,
        /// The documents to read and write, in order.
        to_read: Vec<String>,
        /// Documents the index holds for this source that the store no
        /// longer does.
        removed: Vec<String>,
        /// The commit read and its tree. `None` when `dolt_log()` did not
        /// answer, and an unwritten cursor makes the next pass read the
        /// store whole — the safe direction.
        head: Option<(String, String)>,
        /// `to_read` is every document the store holds.
        whole: bool,
    },
}

/// `None` when the store names no commit, so there is nothing committed
/// to index. A store read is left open, for the load to page through.
fn read_source(
    stanza: &str,
    rendered_root: &Path,
    cursor: Option<&Cursor>,
    indexed: Option<&HashSet<String>>,
    pass: &mut PassCounts,
) -> Result<Option<SourceRead>> {
    // Read-only: the render step owns this store, and an ordinary open
    // would discard the renderer's in-flight rows and schema-commit into
    // it — writing to a file we do not own. Pinned at open: the diff below
    // and the rows behind it name one commit.
    let opened =
        crate::indexed_markdown::IndexedMarkdownStore::open_for_reading(rendered_root, None);
    let store = match opened {
        Ok(Some(store)) => store,
        Ok(None) => {
            tracing::warn!(
                source = %stanza,
                "this render store names no commit, so there is nothing \
                 committed to index; skipping it this pass"
            );
            return Ok(None);
        }
        Err(e) if datalib_store_meta::other_shape(&e).is_some() => {
            tracing::debug!(source = %stanza, error = %format!("{e:#}"), "left as the index had it");
            return Ok(Some(SourceRead::Unreadable));
        }
        Err(e) => return Err(e.context(format!("open render store for {stanza}"))),
    };
    let store = std::sync::Arc::new(store);
    match read_open_store(&store, stanza, cursor, indexed, pass) {
        Ok((to_read, removed, head, whole)) => Ok(Some(SourceRead::Read {
            store,
            to_read,
            removed,
            head,
            whole,
        })),
        Err(e) => {
            close_store(store);
            Err(e)
        }
    }
}

fn close_store(store: std::sync::Arc<crate::indexed_markdown::IndexedMarkdownStore>) {
    match std::sync::Arc::into_inner(store) {
        Some(store) => store.close(),
        None => tracing::warn!("a render store still in use was left to close on drop"),
    }
}

/// What a pass reads of `store`: the documents to load, in order, those
/// the store no longer holds, the commit read with its tree, and whether
/// the documents to load are all of them.
type ToRead = (Vec<String>, Vec<String>, Option<(String, String)>, bool);

fn read_open_store(
    store: &crate::indexed_markdown::IndexedMarkdownStore,
    stanza: &str,
    cursor: Option<&Cursor>,
    indexed: Option<&HashSet<String>>,
    pass: &mut PassCounts,
) -> Result<ToRead> {
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
    // Only the documents' ids here; the load reads them a page at a time.
    let mut present: Vec<String> = match &scan.render {
        Some(changed) if changed.is_empty() => Vec::new(),
        Some(changed) => store
            .markdown_rows_in(&changed.iter().cloned().collect::<Vec<_>>())
            .with_context(|| format!("read which changed documents {stanza} holds"))?
            .into_iter()
            .map(|m| m.markdown_uuid)
            .collect(),
        None => store
            .all_document_uuids()
            .with_context(|| format!("list the documents of {stanza}"))?,
    };
    present.sort_unstable();
    let held: HashSet<&str> = present.iter().map(String::as_str).collect();
    let mut removed: Vec<String> = match &scan.render {
        // An id the diff named that the store no longer has is a deletion.
        Some(changed) => changed
            .iter()
            .filter(|u| !held.contains(u.as_str()))
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
            .filter(|u| !held.contains(u.as_str()))
            .cloned()
            .collect(),
    };
    removed.sort_unstable();
    let head = match scan.new_head {
        Some(commit) => {
            let tree = store
                .tree_of(&commit)
                .with_context(|| format!("read the tree of {stanza}'s render store"))?;
            Some((commit, tree))
        }
        None => None,
    };
    Ok((present, removed, head, scan.render.is_none()))
}

/// Write one source's changes, problems and cursor in one transaction
/// and seal it. `false` when the stop came first: the transaction is
/// rolled back and nothing of this source is written. `held` is how many
/// documents the index held when the pass began, `None` in a build that
/// makes the search indexes at its end.
#[allow(clippy::too_many_arguments)]
async fn apply_source(
    write_lock: &WriteLock,
    stanza: &str,
    read: SourceRead,
    contacts_owed: bool,
    read_ms: u64,
    held: Option<usize>,
    now: &datalib_time::StoredStamp,
    progress: &impl Fn(&str),
    stop: &StopFlag,
    summary: &mut GridIndexSummary,
) -> Result<bool> {
    let before = SealCounts::of(summary);
    let mut laps = Laps::start();
    let mut took = SealTimes {
        read_ms,
        ..SealTimes::default()
    };
    write_lock
        .begin_transaction()
        .await
        .with_context(|| format!("begin the index transaction for {stanza}"))?;
    let res = async {
        let (docs, removed, head) = match read {
            SourceRead::Unreadable => {
                let row = unreadable_store_problem(stanza, OTHER_SHAPE, Severity::Warning);
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
                store,
                to_read,
                removed,
                head,
                whole,
            } => (Some((store, to_read, whole)), removed, head),
        };
        let (store, to_read, whole) = docs.expect("read above");
        summary.markdowns_total += to_read.len();
        let removed_remade = remove_documents(write_lock, stanza, &removed, held, progress)
            .await
            .with_context(|| format!("delete the documents {stanza} dropped"))?;
        took.remove_ms = laps.lap();
        summary.markdowns_removed += removed.len();
        // A whole read writes every document's people with the rest.
        let fill_contacts = contacts_owed && !whole;
        if fill_contacts {
            let Some(carried) = fill_contacts_of(write_lock, stanza, &store, stop).await? else {
                return Ok(None);
            };
            tracing::info!(
                source = %stanza,
                contacts = carried,
                "filled this source's contacts, which this build made new"
            );
        }
        // Too few documents to change a sixteenth of the index, whatever
        // they hold, are not worth measuring.
        let probe = match held {
            Some(held) if !removed_remade && to_read.len() * 16 >= held && to_read.len() > 1 => {
                Some(RemakeProbe::start(write_lock).await?)
            }
            _ => None,
        };
        let loaded = load_source(
            write_lock,
            stanza,
            &store,
            &to_read,
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
        took.load_ms = laps.lap();
        let remade = remade || removed_remade;
        summary.search_indexes_remade += usize::from(remade);
        summary.markdowns_loaded += markdowns;
        summary.rows_inserted += rows;
        let mut guard = write_lock.acquire().await?;
        let conn = guard.conn();
        if !to_read.is_empty() || !removed.is_empty() {
            let rewritten = nodes::rewrite_touched(conn, write_lock)
                .await
                .with_context(|| format!("settle {stanza}'s nodes"))?;
            tracing::debug!(
                source = %stanza,
                touched = rewritten.touched,
                changed = rewritten.changed,
                rows_kept = rewritten.rows_kept,
                rows_dropped = rewritten.rows_dropped,
                "nodes settled"
            );
        }
        took.nodes_ms = laps.lap();
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
        took.terms_ms = laps.lap();
        let problems_moved = clear_source_problems(conn, stanza)
            .await
            .with_context(|| format!("clear {stanza}'s problems from the index"))?;
        // Last and in the same transaction: a failure above rolls back to
        // both the old rows and the old cursor.
        if let Some((store_commit, store_tree)) = head {
            contact_records::set_cursor(conn, stanza, Some(&store_commit)).await?;
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
            !to_read.is_empty() || !removed.is_empty() || problems_moved || fill_contacts,
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
            took.seal_ms = laps.lap();
            if let Some(generation) = generation {
                summary.seals += 1;
                let counts = SealCounts::of(summary).since(&before).message(stanza);
                tracing::debug!(
                    source = %stanza,
                    generation,
                    %counts,
                    read_ms = took.read_ms,
                    remove_ms = took.remove_ms,
                    load_ms = took.load_ms,
                    nodes_ms = took.nodes_ms,
                    terms_ms = took.terms_ms,
                    seal_ms = took.seal_ms,
                    "sealed the index"
                );
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

/// How long each part of one source's pass took, so the seal's log line
/// says where a long source spent it. `seal_ms` runs from the terms to the
/// commit: the problems, the cursor, the generation and the commit itself.
#[derive(Default)]
struct SealTimes {
    read_ms: u64,
    remove_ms: u64,
    load_ms: u64,
    nodes_ms: u64,
    terms_ms: u64,
    seal_ms: u64,
}

/// Milliseconds since the last lap.
struct Laps(Instant);

impl Laps {
    fn start() -> Self {
        Self(Instant::now())
    }

    fn lap(&mut self) -> u64 {
        let now = Instant::now();
        let ms = millis(now - self.0);
        self.0 = now;
        ms
    }
}

fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
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
            "datalib-step unified_grid_and_search_index: source={source} markdowns_read={} \
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

/// Delete the documents a source no longer has, all at once. When they
/// hold a sixteenth of the index's rows the search indexes are dropped
/// first, as for a large load: kept up row by row, each deleted row
/// costs a page in every one of them. `true` when they were dropped; the
/// caller remakes them before its transaction commits. `held` is `None`
/// in a build that makes the search indexes at its end.
async fn remove_documents(
    write_lock: &WriteLock,
    stanza: &str,
    markdown_uuids: &[String],
    held: Option<usize>,
    progress: &impl Fn(&str),
) -> Result<bool> {
    if markdown_uuids.is_empty() {
        return Ok(false);
    }
    // Too few documents to hold a sixteenth of the index are not worth
    // measuring.
    let remade = match held {
        Some(held) if markdown_uuids.len() * 16 >= held => {
            RemakeProbe::start(write_lock)
                .await?
                .decide_removal(write_lock, markdown_uuids)
                .await?
        }
        _ => false,
    };
    progress(&format!(
        "{stanza}: removing {} document(s)",
        markdown_uuids.len()
    ));
    let started = Instant::now();
    delete_markdowns(write_lock, markdown_uuids).await?;
    tracing::info!(
        source = %stanza,
        documents = markdown_uuids.len(),
        took_ms = started.elapsed().as_millis() as u64,
        "removed the documents this source no longer has"
    );
    Ok(remade)
}

/// Replace the people of every document `store` holds with what it says
/// now, reading nothing else of them, a page at a time. How many it
/// carried; `None` when the stop came first.
async fn fill_contacts_of(
    write_lock: &WriteLock,
    stanza: &str,
    store: &crate::indexed_markdown::IndexedMarkdownStore,
    stop: &StopFlag,
) -> Result<Option<usize>> {
    let mut documents = store
        .all_document_uuids()
        .map_err(|e| UnreadPage(format!("list the documents of {stanza}: {e:#}")))?;
    documents.sort_unstable();
    let mut carried_in_all = 0;
    for page in documents.chunks(LOAD_PAGE) {
        if stop.requested() {
            return Ok(None);
        }
        let contacts = store
            .contacts_matching(page)
            .map_err(|e| UnreadPage(format!("read the contacts of {stanza}: {e:#}")))?;
        let carried: Vec<(&str, &NormalizedContact)> = page
            .iter()
            .flat_map(|md| {
                contacts
                    .get(md)
                    .into_iter()
                    .flatten()
                    .map(move |c| (md.as_str(), c))
            })
            .collect();
        carried_in_all += carried.len();
        let mut guard = write_lock.acquire().await?;
        page::replace_contacts(
            guard.conn(),
            Store::Index,
            &serde_json::to_string(page)?,
            &carried,
        )
        .await
        .with_context(|| format!("fill the contacts of {stanza}"))?;
    }
    Ok(Some(carried_in_all))
}

/// A page of a render store the load could not read.
#[derive(Debug)]
struct UnreadPage(String);

impl std::fmt::Display for UnreadPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UnreadPage {}

/// Documents read from a render store and written into the index together:
/// what one source costs in memory, however large it is.
const LOAD_PAGE: usize = 500;

/// Apply one source's documents, read from `store` a page at a time.
/// `None` when the stop came first. With a `probe`, a twentieth of the way
/// in it decides whether the rest is written without the search indexes.
#[allow(clippy::too_many_arguments)]
async fn load_source(
    write_lock: &WriteLock,
    stanza: &str,
    store: &crate::indexed_markdown::IndexedMarkdownStore,
    to_read: &[String],
    probe: Option<&RemakeProbe>,
    progress: &impl Fn(&str),
    stop: &StopFlag,
) -> Result<Option<Loaded>> {
    let pin = store.pin().expect("a reader is pinned at open").clone();
    let mut rows = 0;
    let mut remade = false;
    let mut applied = 0;
    let probe_after = (to_read.len() / 20).max(1);
    let mut probe = probe;
    for page in to_read.chunks(LOAD_PAGE) {
        if stop.requested() {
            return Ok(None);
        }
        let wanted: HashSet<String> = page.iter().cloned().collect();
        // The stanza name is authoritative. Everything else comes through
        // from the store unchanged: every document the diff named is
        // applied, and the rows of it that differ are written.
        let docs: Vec<RenderedMarkdown> = store
            .documents_matching(Some(&wanted), &pin)
            .map_err(|e| UnreadPage(format!("read documents from {stanza}: {e:#}")))?
            .into_iter()
            .map(|md| RenderedMarkdown {
                source_id: stanza.to_string(),
                // Already rows in the store; re-applying would double-count.
                problems: Vec::new(),
                ..md
            })
            .collect();
        rows += apply_page(write_lock, &docs)
            .await
            .with_context(|| format!("load {} documents from {stanza}", docs.len()))?;
        applied += page.len();
        progress(&format!("{stanza}: loaded {applied}/{}", to_read.len()));
        if stop.requested() {
            return Ok(None);
        }
        if let Some(p) = probe.filter(|_| applied >= probe_after) {
            remade = p.decide(write_lock, applied, to_read.len()).await?;
            probe = None;
        }
    }
    Ok(Some(Loaded {
        markdowns: to_read.len(),
        rows,
        remade,
    }))
}

/// Every document the index holds, read once a pass: the work grows with
/// the whole index, not with what changed.
const INDEXED_DOCUMENTS: Sql = sql!(
    "etl.render.grid.list_documents_by_source",
    "SELECT source_id, markdown_uuid FROM markdowns"
);

async fn load_markdown_uuids_by_source(
    pool: &SqlitePool,
) -> Result<HashMap<String, HashSet<String>>> {
    let rows = INDEXED_DOCUMENTS
        .query()
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
    let rows = sql!(
        "etl.render.grid.list_source_cursors",
        "SELECT source_id, store_commit, store_tree FROM source_cursors"
    )
    .query()
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
    sql!(
        "etl.render.grid.delete_source_cursor",
        "DELETE FROM source_cursors WHERE source_id = ?"
    )
    .query()
    .bind(&row.source_id)
    .execute(&mut **conn)
    .await
    .context("clear prior source cursor")?;
    let sql = Sql::dynamic(
        sql_id!("etl.render.grid.insert_source_cursor"),
        datalib_etl::bulk::insert_sql::<SourceCursorRow>(),
    );
    // Audited: `sql` is built from `SourceCursorRow`'s associated consts.
    row.bind_into(sql.query())
        .execute(&mut **conn)
        .await
        .with_context(|| format!("write source cursor {}", row.source_id))?;
    Ok(())
}

/// Remove documents and everything hanging off them: the deletes
/// `apply_page` runs before re-inserting, without the insert. Called for
/// the documents a source no longer has ([`remove_documents`]); without
/// it a deleted conversation would stay in the grid.
pub async fn delete_markdowns(write_lock: &WriteLock, markdown_uuids: &[String]) -> Result<()> {
    let mut guard = write_lock.acquire().await?;
    let removed = delete_document_rows(guard.conn(), write_lock.store(), markdown_uuids).await?;
    removed.note(write_lock);
    Ok(())
}

/// The statements that remove a set of documents' rows, the set bound
/// once as a JSON array. A statement per document instead costs a
/// round trip each, and a source that empties holds hundreds of
/// thousands of them.
pub(crate) const DELETE_DOCUMENT_GRID_ROWS: Sql = sql!(
    "etl.render.grid.delete_grid_rows_of_documents",
    "DELETE FROM grid_rows WHERE markdown_uuid IN (SELECT value FROM json_each(?)) RETURNING uuid"
);
const DELETE_DOCUMENT_NODES: Sql = sql!(
    "etl.render.grid.delete_nodes_of_documents",
    "DELETE FROM data_nodes WHERE markdown_uuid IN (SELECT value FROM json_each(?)) RETURNING uuid"
);
pub(crate) const DELETE_DOCUMENTS: &[Sql] = &[
    sql!(
        "etl.render.grid.delete_edges_of_documents",
        "DELETE FROM edges WHERE src_markdown_uuid IN (SELECT value FROM json_each(?))"
    ),
    sql!(
        "etl.render.grid.delete_filings_of_documents",
        "DELETE FROM filings WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
    ),
    sql!(
        "etl.render.grid.delete_photos_of_documents",
        "DELETE FROM source_photos WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
    ),
    sql!(
        "etl.render.grid.delete_markdowns_of_documents",
        "DELETE FROM markdowns WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
    ),
];

const RENDER_CONTACT_DELETES: &[Sql] = &[
    sql!(
        "etl.render.store.delete_contacts_of_documents",
        "DELETE FROM source_contacts WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
    ),
    sql!(
        "etl.render.store.delete_contact_handles_of_documents",
        "DELETE FROM source_contact_handles \
                     WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
    ),
];
const INDEX_CONTACT_DELETES: &[Sql] = &[sql!(
    "etl.render.grid.delete_contacts_of_documents",
    "DELETE FROM document_contacts WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
)];

impl Store {
    /// What removes the people a set of documents carries, as
    /// [`DELETE_DOCUMENTS`] removes the rest.
    pub(crate) fn delete_contacts(self) -> &'static [Sql] {
        match self {
            Store::Render => RENDER_CONTACT_DELETES,
            Store::Index => INDEX_CONTACT_DELETES,
        }
    }
}

/// What removing documents took with it, which the grid index derives
/// from.
#[derive(Debug, Default)]
pub(crate) struct Removed {
    rows: Vec<String>,
    /// The nodes the documents carried, each as often as it was.
    nodes: Vec<String>,
}

impl Removed {
    fn note(self, write_lock: &WriteLock) {
        write_lock.touch_nodes(self.nodes);
        write_lock.note_gone(self.rows);
    }
}

/// The rows the documents own: their grid rows, their outgoing edges and
/// their `markdowns` rows. Not their `problems`, which say why a document
/// is the way it is and outlive one that ends with nothing.
pub(crate) async fn delete_document_rows(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    store: Store,
    markdown_uuids: &[String],
) -> Result<Removed> {
    if markdown_uuids.is_empty() {
        return Ok(Removed::default());
    }
    let set = serde_json::to_string(markdown_uuids)?;
    let mut returned = [Vec::new(), Vec::new()];
    for (sql, into) in [DELETE_DOCUMENT_GRID_ROWS, DELETE_DOCUMENT_NODES]
        .into_iter()
        .zip(&mut returned)
    {
        *into = sql
            .query_scalar()
            .bind(&set)
            .fetch_all(&mut **conn)
            .await
            .with_context(|| format!("delete {} document(s): {sql}", markdown_uuids.len()))?;
    }
    for sql in DELETE_DOCUMENTS.iter().chain(store.delete_contacts()) {
        sql.query()
            .bind(&set)
            .execute(&mut **conn)
            .await
            .with_context(|| format!("delete {} document(s): {sql}", markdown_uuids.len()))?;
    }
    let [rows, nodes] = returned;
    Ok(Removed { rows, nodes })
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

fn first_repeated<'a>(uuids: &[&'a str]) -> Option<&'a str> {
    let mut seen = HashSet::with_capacity(uuids.len());
    uuids.iter().copied().find(|u| !seen.insert(*u))
}

/// `row` with a uuid in `account` or `project` lowercase with dashes,
/// however the source spelled it, so their keys compare it once. Both
/// are for display; `upstream_id` stays as the source wrote it, since a
/// row's uuid is minted from it, and `id:` reads its search term
/// (`search_terms_of`). The ids Datalib mints are written this way already.
fn uuids_one_way(row: &GridRow) -> std::borrow::Cow<'_, GridRow> {
    let respelled = |id: &Option<String>| {
        id.as_deref()
            .and_then(datalib_uuid_text::canonical)
            .filter(|c| Some(c.as_str()) != id.as_deref())
    };
    let (account, project) = (respelled(&row.account), respelled(&row.project));
    if account.is_none() && project.is_none() {
        return std::borrow::Cow::Borrowed(row);
    }
    let mut row = row.clone();
    row.account = account.or(row.account);
    row.project = project.or(row.project);
    std::borrow::Cow::Owned(row)
}

/// Fails unless SQLite plans every one of `statements` through an index.
#[cfg(test)]
pub(crate) async fn assert_searched_by_index(pool: &SqlitePool, statements: &[Sql]) {
    for sql in statements {
        // Audited: `sql` is a static literal from the test.
        let mut explain = Sql::dynamic(
            sql_id!("etl.render.grid.explain_query_plan"),
            format!("EXPLAIN QUERY PLAN {sql}"),
        )
        .query();
        for _ in 0..sql.as_str().matches('?').count() {
            explain = explain.bind("x");
        }
        let plan: Vec<String> = explain
            .fetch_all(pool)
            .await
            .unwrap_or_else(|e| panic!("explain {sql}: {e}"))
            .iter()
            .map(|r| r.get::<String, _>("detail"))
            .collect();
        // The set a statement matches is bound as a JSON array and read
        // whole; the table it looks the set up in must not be.
        assert!(
            plan.iter()
                .all(|d| !d.starts_with("SCAN") || d.starts_with("SCAN json_each")),
            "{sql} scans its table: {plan:?}"
        );
    }
}

#[cfg(test)]
mod open_index_tests {
    use super::*;

    use datalib_sql::test_sql;
    /// Guards the two warnings per pass, 88 in one sync, about a source
    /// that was turned off and so could not change: one that stays
    /// unreadable is said once.
    #[test]
    fn an_unreadable_store_is_news_once() {
        use datalib_schema::problems::Severity;
        let row = || unreadable_store_problem("s", OTHER_SHAPE, Severity::Warning);
        let first = row().stamped(None, "2026-11-04T10:00:00Z", None);
        assert!(first.is_news(None));
        let again = row().stamped(Some(&first), "2026-11-04T10:05:00Z", None);
        assert!(!again.is_news(Some(&first)), "nothing it says changed");
        let other = unreadable_store_problem("s", "it will not open", Severity::Warning).stamped(
            Some(&first),
            "2026-11-04T10:10:00Z",
            None,
        );
        assert!(other.is_news(Some(&first)), "it says something else now");
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
        let mut statements = vec![DELETE_DOCUMENT_GRID_ROWS, DELETE_DOCUMENT_NODES];
        statements.extend_from_slice(DELETE_DOCUMENTS);
        statements.extend_from_slice(Store::Index.delete_contacts());
        assert_searched_by_index(&pool, &statements).await;
        pool.close().await;
    }

    /// Every statement `nodes::rewrite_touched` runs reads each node by its uuid, also
    /// while a build has the read indexes dropped, which is when it runs.
    /// Read by anything wider it was seconds on each source of a large
    /// index (#1926), or quadratic in a first build.
    #[tokio::test]
    async fn the_nodes_are_rewritten_by_their_uuids_through_a_build() {
        let td = tempfile::tempdir().unwrap();
        let pool = open_index(&td.path().join("db.sqlite"))
            .await
            .expect("open_index");
        let mut conn = pool.acquire().await.unwrap();
        drop_search_indexes(&mut conn)
            .await
            .expect("drop the read indexes");
        drop(conn);
        assert_searched_by_index(
            &pool,
            &[
                nodes::WRITE_FROM_BEST_ACCOUNT.clone(),
                nodes::UNCARRIED,
                nodes::ORPHANED,
                nodes::WITHOUT_A_ROW,
            ],
        )
        .await;
        pool.close().await;
    }

    /// One pass loads a whole source in one transaction, and a checkpoint
    /// alone leaves the `-wal` that large; the limit cuts it back. A
    /// re-index left a live grid index's `-wal` at 4 GB.
    #[tokio::test]
    async fn the_grid_index_cuts_its_wal_back() {
        let td = tempfile::tempdir().unwrap();
        let pool = open_index(&td.path().join("db.sqlite"))
            .await
            .expect("open_index");
        let limit: i64 = test_sql!("PRAGMA journal_size_limit")
            .query_scalar()
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            limit.to_string(),
            datalib_core::plain_sqlite::WAL_SIZE_LIMIT_BYTES
        );
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

    use datalib_sql::test_sql;
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
            parent_uuid: Some("parent-everything".into()),
            sort_key: "2026-06-02T20:00:00Z".into(),
            read_uuid: Some("read-everything".into()),
            place_uuid: Some("place-everything".into()),
            message_index: Some(3),
            entire_chat: "/chat/conv-1701".into(),
            preview: "Tea. Earl Grey. Hot.".into(),
            content_hash: content_hash("Tea. Earl Grey. Hot."),
            source_id: Some("claude_web".into()),
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
        let lock = WriteLock::for_index(pool.clone());
        page::write_grid_rows(&mut conn, &lock, std::slice::from_ref(&row))
            .await
            .expect("write_grid_rows");
        drop(conn);

        // `SELECT *` on purpose: the point is to see every column the DDL
        // declares, including ones this test predates.
        let read_back = test_sql!("SELECT * FROM grid_rows WHERE uuid = ?")
            .query()
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
             declared on GridRow (or derived in write_grid_rows) but \
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
    use datalib_schema::grid_rows::{content_hash, GridRow};
    use datalib_schema::providers::Provider;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use std::sync::Arc as StdArc;
    use tempfile::tempdir;

    use datalib_sql::test_sql;
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
            parent_uuid: None,
            sort_key: String::new(),
            read_uuid: None,
            place_uuid: None,
            message_index: None,
            entire_chat: format!("/chat/{uuid}"),
            preview: format!("body for {uuid}"),
            content_hash: content_hash(&format!("body for {uuid}")),
            source_id: Some("test".into()),
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
            searchable: true,
            content_hash: None,
            markdown_uuid: uuid.clone(),
            source_id: "test".into(),
            upstream_cursor: None,
            render_unit_key: None,
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            front: Default::default(),
            edges: Vec::new(),
            contacts: Vec::new(),
            data_nodes: Vec::new(),
            filings: Vec::new(),
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
        let mut handles = Vec::with_capacity(n_tasks);
        for task in 0..n_tasks {
            let lock = write_lock.clone();
            handles.push(tokio::spawn(async move {
                for idx in 0..per_task {
                    let md = mk_md(task, idx);
                    apply_one(lock.as_ref(), &md)
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

        let write_lock = StdArc::new(WriteLock::for_index(pool.clone()));

        apply_in_parallel(&write_lock, N_TASKS, PER_TASK).await;

        let grid_n: i64 = test_sql!("SELECT COUNT(*) FROM grid_rows")
            .query_scalar()
            .fetch_one(&pool)
            .await
            .unwrap();
        let md_n: i64 = test_sql!("SELECT COUNT(*) FROM markdowns")
            .query_scalar()
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

        let write_lock = StdArc::new(WriteLock::for_index(pool.clone()));

        // Every apply_one below reuses the held conn and accumulates into the
        // open transaction.
        write_lock.begin_transaction().await.expect("BEGIN");

        apply_in_parallel(&write_lock, N_TASKS, PER_TASK).await;

        // Before commit: rows aren't visible from a fresh connection
        // (other than the one holding the open tx).
        let pre_grid_n: i64 = test_sql!("SELECT COUNT(*) FROM grid_rows")
            .query_scalar()
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            pre_grid_n, 0,
            "pre-COMMIT: other connections must not see uncommitted rows"
        );

        write_lock.commit_transaction().await.expect("COMMIT");

        let grid_n: i64 = test_sql!("SELECT COUNT(*) FROM grid_rows")
            .query_scalar()
            .fetch_one(&pool)
            .await
            .unwrap();
        let md_n: i64 = test_sql!("SELECT COUNT(*) FROM markdowns")
            .query_scalar()
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

        let lock = WriteLock::for_index(pool.clone());

        lock.begin_transaction().await.unwrap();
        for idx in 0..5 {
            apply_one(&lock, &mk_md(0, idx)).await.unwrap();
        }
        lock.rollback_transaction().await.unwrap();

        let grid_n: i64 = test_sql!("SELECT COUNT(*) FROM grid_rows")
            .query_scalar()
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(grid_n, 0, "ROLLBACK must leave grid_rows untouched");
    }

    /// A document re-rendered with every row rejected keeps no
    /// `markdowns` row: before this, the old render's title, timestamps
    /// and `render_unit_key` outlived the rows they described. Found by the
    /// contract harness on a gitlab merge request re-keyed under another
    /// render unit whose rows all failed `created_at`.
    #[tokio::test]
    async fn a_document_re_rendered_with_no_rows_loses_its_markdowns_row() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("empty.doltlite_db"), 2).await;
        super::init_schema(&pool).await.expect("init_schema");
        let lock = WriteLock::for_index(pool.clone());

        let mut md = mk_md(0, 0);
        md.render_unit_key = Some("mr!17".into());
        apply_one(&lock, &md).await.unwrap();

        let mut moved = mk_md(0, 0);
        moved.render_unit_key = Some("mr!17~new".into());
        moved.rows.clear();
        let inserted = apply_one(&lock, &moved).await.unwrap();
        assert_eq!(inserted, 0);

        let render_units: Vec<Option<String>> =
            test_sql!("SELECT render_unit_key FROM markdowns WHERE markdown_uuid = ?")
                .query_scalar()
                .bind(&md.markdown_uuid)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            render_units.is_empty(),
            "no markdowns row, old render unit or new: {render_units:?}"
        );
        let grid_n: i64 = test_sql!("SELECT COUNT(*) FROM grid_rows")
            .query_scalar()
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

    use datalib_sql::test_sql;
    async fn holding_one_document(db: &Path) -> SqlitePool {
        let pool = open_index(db).await.unwrap();
        test_sql!(
            "INSERT INTO markdowns (markdown_uuid, source_id, provider, kind, searchable) \
             VALUES ('md-1', 'claude_web', 'claude', 'Chat', 1)"
        )
        .query()
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn documents(pool: &SqlitePool) -> i64 {
        test_sql!("SELECT count(*) FROM markdowns")
            .query_scalar()
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
        test_sql!("UPDATE _datalib_meta SET value = 'older' WHERE key = 'schema_hash'")
            .query()
            .execute(&pool)
            .await
            .unwrap();
        crate::grid_index::close_index(pool).await.unwrap();

        let pool = open_index(&db).await.unwrap();
        assert_eq!(documents(&pool).await, 0);
    }

    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    }

    /// An index in another shape is replaced by a new file rather than
    /// emptied: dropping a 5.8 GB index's tables and vacuuming the space
    /// took eleven minutes, and deleting the file takes none. One in this
    /// shape keeps its file.
    #[tokio::test]
    async fn an_index_in_another_shape_is_a_new_file() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        crate::grid_index::close_index(pool).await.unwrap();
        let kept = inode(&db);
        let pool = open_index(&db).await.unwrap();
        assert_eq!(inode(&db), kept, "an index in this shape was replaced");

        test_sql!("UPDATE _datalib_meta SET value = 'older' WHERE key = 'schema_hash'")
            .query()
            .execute(&pool)
            .await
            .unwrap();
        crate::grid_index::close_index(pool).await.unwrap();
        let old = std::fs::File::open(&db).unwrap();
        let pool = open_index(&db).await.unwrap();
        assert_ne!(
            inode(&db),
            kept,
            "an index in another shape was emptied in place"
        );
        assert_eq!(documents(&pool).await, 0);
        drop(old);
        crate::grid_index::close_index(pool).await.unwrap();
    }

    fn search_doc(uuid: &str) -> datalib_search_index::IndexDoc {
        datalib_search_index::IndexDoc {
            source_id: "claude_web".into(),
            markdown_uuid: uuid.into(),
            content_hash: format!("hash-{uuid}"),
            title: "Ten Forward".into(),
            head: None,
            sections: vec![datalib_search_index::IndexSection {
                uuid: Some("m1".into()),
                text: "the warp core is stable".into(),
                lead: None,
            }],
        }
    }

    fn meta_with(whole: &str, parts: &[(&str, String)]) -> datalib_store_meta::Meta {
        datalib_store_meta::Meta {
            datalib_version: String::new(),
            git_hash: None,
            doltlite_version: None,
            schema_hash: whole.into(),
            index_hash: None,
            schema_version: 0,
            shared_schema_version: 0,
            store_kind: None,
            written_at_utc: String::new(),
            parts: parts
                .iter()
                .map(|(p, h)| (p.to_string(), h.clone()))
                .collect(),
        }
    }

    /// What an open empties, from what the file records: nothing when
    /// its tables are this build's, the parts that moved when others did
    /// not, the contacts with the grid, and the whole file when every part
    /// moved or none is recorded.
    #[test]
    fn an_open_empties_only_the_parts_that_moved() {
        use crate::grid_index::{refill, shape, Part, Refill};
        let whole = shape().tables;
        let all: Vec<(&str, String)> = [Part::Grid, Part::Contacts, Part::Keywords]
            .iter()
            .map(|p| (p.as_str(), p.tables()))
            .collect();
        let older = |moved: &[&str]| -> Vec<(&str, String)> {
            all.iter()
                .map(|(p, h)| {
                    (
                        *p,
                        if moved.contains(p) {
                            "older".into()
                        } else {
                            h.clone()
                        },
                    )
                })
                .collect()
        };
        assert_eq!(refill(None), Refill::Whole);
        assert_eq!(refill(Some(&meta_with(&whole, &all))), Refill::Nothing);
        assert_eq!(refill(Some(&meta_with(&whole, &[]))), Refill::Nothing);
        assert_eq!(refill(Some(&meta_with("older", &[]))), Refill::Whole);
        assert_eq!(refill(Some(&meta_with("older", &all))), Refill::Whole);
        for (moved, emptied) in [
            (&["keywords"][..], vec![Part::Keywords]),
            (&["contacts"][..], vec![Part::Contacts]),
            (&["grid"][..], vec![Part::Grid, Part::Contacts]),
            (&["grid", "contacts"][..], vec![Part::Grid, Part::Contacts]),
            (
                &["contacts", "keywords"][..],
                vec![Part::Contacts, Part::Keywords],
            ),
        ] {
            assert_eq!(
                refill(Some(&meta_with("older", &older(moved)))),
                Refill::Parts(emptied),
                "{moved:?}"
            );
        }
        assert_eq!(
            refill(Some(&meta_with("older", &older(&["grid", "keywords"])))),
            Refill::Whole
        );
    }

    /// The whole file's hash is still a hash over every statement, so a
    /// build that moves no part's tables keeps a file an earlier build
    /// wrote; and every table the file makes belongs to exactly one part.
    #[test]
    fn the_parts_split_the_whole() {
        use crate::grid_index::{contact_records, grid_ddl, index_ddl, Part};
        let keywords: Vec<&str> = datalib_search_index::KEYWORD_DDL.to_vec();
        let contacts: Vec<&str> = contact_records::DDL.to_vec();
        assert_eq!(
            index_ddl().collect::<Vec<_>>(),
            grid_ddl()
                .chain(contacts.iter().copied())
                .chain(keywords.iter().copied())
                .collect::<Vec<_>>()
        );
        let parts = [Part::Grid, Part::Contacts, Part::Keywords];
        for ddl in index_ddl() {
            let mut words = ddl.split_whitespace();
            if !words.by_ref().take(4).any(|w| w == "TABLE") {
                continue;
            }
            let Some(name) = words.find(|w| !matches!(*w, "IF" | "NOT" | "EXISTS")) else {
                continue;
            };
            let name = name.split('(').next().unwrap();
            let made_by = if keywords.contains(&ddl) {
                Part::Keywords
            } else if contacts.contains(&ddl) {
                Part::Contacts
            } else {
                Part::Grid
            };
            for part in parts {
                assert_eq!(part.holds(name), part == made_by, "{name} and {part:?}");
            }
        }
    }

    /// Every part moving takes the keyword index with the rest: the file
    /// is new, and every source's keyword pass indexes it again.
    #[tokio::test]
    async fn a_change_of_every_part_takes_the_keyword_index_with_it() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        let search = datalib_search_index::Index::open(&db).await.unwrap();
        search.put(&[search_doc("md-1")]).await.unwrap();
        search.close().await;
        recorded_by_another_build(&pool, &["", ".grid", ".contacts", ".keywords"]).await;
        crate::grid_index::close_index(pool).await.unwrap();

        let pool = open_index(&db).await.unwrap();
        assert_eq!(documents(&pool).await, 0);
        crate::grid_index::close_index(pool).await.unwrap();
        assert!(keyword_documents(&db).await.is_empty());
    }

    /// Records each of `keys` (`schema_hash` and a suffix) as another
    /// build's: what a file looks like to a build that changed those parts'
    /// tables.
    async fn recorded_by_another_build(pool: &SqlitePool, suffixes: &[&str]) {
        for suffix in suffixes {
            test_sql!("UPDATE _datalib_meta SET value = 'older' WHERE key = 'schema_hash' || ?")
                .query()
                .bind(suffix)
                .execute(pool)
                .await
                .unwrap();
        }
    }

    async fn keyword_documents(db: &Path) -> Vec<String> {
        let search = datalib_search_index::Index::open(db).await.unwrap();
        let held = search.indexed("claude_web").await.unwrap();
        search.close().await;
        held.into_keys().collect()
    }

    async fn generation(pool: &SqlitePool) -> i64 {
        test_sql!("SELECT generation FROM store_generation WHERE id = 1")
            .query_scalar()
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A build that changes only the keyword index's tables empties them
    /// and keeps every grid row, in the same file: the grid stays whole
    /// while every source's keyword pass fills them again. Its generation
    /// moves, since what a keyword filter in the grid reads went.
    #[tokio::test]
    async fn a_change_of_the_keyword_tables_keeps_the_grid() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        let search = datalib_search_index::Index::open(&db).await.unwrap();
        search.put(&[search_doc("md-1")]).await.unwrap();
        search.set_read_at("claude_web", Some("c1")).await.unwrap();
        search.close().await;
        let before = generation(&pool).await;
        recorded_by_another_build(&pool, &["", ".keywords"]).await;
        crate::grid_index::close_index(pool).await.unwrap();
        let kept = inode(&db);

        let pool = open_index(&db).await.unwrap();
        assert_eq!(inode(&db), kept, "the file was replaced");
        assert_eq!(documents(&pool).await, 1, "the grid's rows went");
        assert_eq!(generation(&pool).await, before + 1);
        let meta = datalib_store_meta::read(&pool).await.unwrap().unwrap();
        assert_eq!(meta.shape(), crate::grid_index::shape());
        crate::grid_index::close_index(pool).await.unwrap();
        assert!(keyword_documents(&db).await.is_empty());
        let search = datalib_search_index::Index::open(&db).await.unwrap();
        assert_eq!(search.read_at("claude_web").await.unwrap(), None);
        search.close().await;
    }

    /// A build that changes only the grid's tables empties them, cursors
    /// and all, and keeps the keyword index and where each source's keyword
    /// pass last read.
    #[tokio::test]
    async fn a_change_of_the_grid_tables_keeps_the_keywords() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        test_sql!(
            "INSERT INTO source_cursors (source_id, store_commit, indexed_at_utc, documents_applied) \
             VALUES ('claude_web', 'c1', '2026-11-26T00:00:00Z', 1)"
        )
        .query()
        .execute(&pool)
        .await
        .unwrap();
        let search = datalib_search_index::Index::open(&db).await.unwrap();
        search.put(&[search_doc("md-1")]).await.unwrap();
        search.set_read_at("claude_web", Some("c1")).await.unwrap();
        search.close().await;
        recorded_by_another_build(&pool, &["", ".grid"]).await;
        crate::grid_index::close_index(pool).await.unwrap();

        let pool = open_index(&db).await.unwrap();
        assert_eq!(documents(&pool).await, 0);
        let cursors: i64 = test_sql!("SELECT count(*) FROM source_cursors")
            .query_scalar()
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            cursors, 0,
            "a cursor kept would read nothing into the new grid"
        );
        crate::grid_index::close_index(pool).await.unwrap();
        assert_eq!(keyword_documents(&db).await, ["md-1"]);
        let search = datalib_search_index::Index::open(&db).await.unwrap();
        assert_eq!(
            search.read_at("claude_web").await.unwrap().as_deref(),
            Some("c1")
        );
        search.close().await;
    }

    /// A file written before the parts were recorded, in this build's
    /// tables, is kept and has its parts recorded: the same statements made
    /// it, part by part.
    #[tokio::test]
    async fn a_file_from_before_the_parts_in_this_shape_is_kept() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        test_sql!("DELETE FROM _datalib_meta WHERE key LIKE 'schema_hash.%'")
            .query()
            .execute(&pool)
            .await
            .unwrap();
        crate::grid_index::close_index(pool).await.unwrap();
        let kept = inode(&db);
        let pool = open_index(&db).await.unwrap();
        assert_eq!(inode(&db), kept);
        assert_eq!(documents(&pool).await, 1);
        let meta = datalib_store_meta::read(&pool).await.unwrap().unwrap();
        assert_eq!(meta.parts.len(), 3, "{:?}", meta.parts);
        crate::grid_index::close_index(pool).await.unwrap();
    }

    /// Keyword search reads while the grid is rebuilt: a build into an
    /// empty grid records the grid without its indexes, which a grid reader
    /// waits for and a keyword reader does not. Keyword tables in another
    /// shape are refused, naming the step.
    #[tokio::test]
    async fn a_keyword_reader_waits_only_for_the_keyword_tables() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        crate::grid_index::record_shape(&pool, &crate::grid_index::shape_without_indexes())
            .await
            .unwrap();
        assert!(crate::grid_index::keywords_readable(&db).await.unwrap());
        let grid = crate::grid_index::open_for_reading(&db).await.unwrap_err();
        assert!(datalib_store_meta::other_shape(&grid).is_some(), "{grid:#}");

        recorded_by_another_build(&pool, &[".keywords"]).await;
        let e = crate::grid_index::keywords_readable(&db).await.unwrap_err();
        let refused = datalib_store_meta::other_shape(&e).expect("refused for its shape");
        assert_eq!(
            refused.step.as_deref(),
            Some(crate::grid_index::step().as_str())
        );
        crate::grid_index::close_index(pool).await.unwrap();
    }

    /// A grid batch takes the file's write lock when it starts. One that
    /// took it at its first write would fail outright, not wait, when the
    /// search index committed a batch after the grid's batch first read.
    #[tokio::test]
    async fn a_grid_batch_holds_the_file_from_its_start() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        let search = datalib_search_index::Index::open(&db).await.unwrap();
        let lock = crate::grid_index::WriteLock::for_index(pool.clone());
        lock.begin_transaction().await.unwrap();
        {
            let mut guard = lock.acquire().await.unwrap();
            let n: i64 = test_sql!("SELECT count(*) FROM markdowns")
                .query_scalar()
                .fetch_one(&mut **guard.conn())
                .await
                .unwrap();
            assert_eq!(n, 1);
        }
        {
            let docs = [search_doc("md-2")];
            let put = search.put(&docs);
            tokio::pin!(put);
            let waited =
                tokio::time::timeout(std::time::Duration::from_millis(500), &mut put).await;
            assert!(
                waited.is_err(),
                "the search index wrote inside the grid's batch"
            );
            {
                let mut guard = lock.acquire().await.unwrap();
                test_sql!("DELETE FROM markdowns")
                    .query()
                    .execute(&mut **guard.conn())
                    .await
                    .unwrap();
            }
            lock.commit_transaction().await.unwrap();
            put.await.unwrap();
        }
        assert_eq!(documents(&pool).await, 0);
        assert_eq!(search.indexed("claude_web").await.unwrap().len(), 1);
        search.close().await;
        crate::grid_index::close_index(pool).await.unwrap();
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

    /// An index whose tables are this build's and whose indexes are not is
    /// kept and given them, not rebuilt: adding an index costs making it.
    /// One this build retired is dropped, and one it makes over other
    /// columns is made again; before, the first went on being written and
    /// the second stayed as it was under the shape that said otherwise. An
    /// index that records no shape at all is in none, and made new.
    #[tokio::test]
    async fn only_moved_indexes_are_made_and_no_recorded_shape_is_made_new() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let pool = holding_one_document(&db).await;
        let indexes_of = |pool: SqlitePool| async move {
            let made: Vec<(String, String)> = test_sql!(
                "SELECT name, sql FROM sqlite_master \
                 WHERE type = 'index' AND sql NOTNULL ORDER BY name"
            )
            .query_as()
            .fetch_all(&pool)
            .await
            .unwrap();
            made
        };
        let new = indexes_of(pool.clone()).await;
        for earlier in [
            test_sql!("DROP INDEX grid_rows_by_author"),
            test_sql!("DROP INDEX grid_rows_by_kind"),
            test_sql!("CREATE INDEX grid_rows_by_kind ON grid_rows(kind)"),
            test_sql!(
                "CREATE INDEX grid_rows_by_parent ON grid_rows (parent_uuid, sort_key, uuid)"
            ),
        ] {
            earlier.query().execute(&pool).await.unwrap();
        }
        assert_ne!(indexes_of(pool.clone()).await, new);
        test_sql!("UPDATE _datalib_meta SET value = 'fewer' WHERE key = 'index_hash'")
            .query()
            .execute(&pool)
            .await
            .unwrap();
        crate::grid_index::close_index(pool).await.unwrap();

        let pool = open_index(&db).await.unwrap();
        assert_eq!(documents(&pool).await, 1, "kept");
        let meta = datalib_store_meta::read(&pool).await.unwrap().unwrap();
        assert_eq!(meta.shape(), crate::grid_index::shape());
        assert_eq!(indexes_of(pool.clone()).await, new);

        test_sql!("DROP TABLE _datalib_meta")
            .query()
            .execute(&pool)
            .await
            .unwrap();
        crate::grid_index::close_index(pool).await.unwrap();
        let pool = open_index(&db).await.unwrap();
        assert_eq!(documents(&pool).await, 0, "made new");
        crate::grid_index::close_index(pool).await.unwrap();
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
        build_grid_index, build_grid_index_for, discover_sources, load_source_cursors, misfiled,
        RenderedMarkdown,
    };
    use crate::indexed_markdown::IndexedMarkdownStore;
    use datalib_etl::stop::StopFlag;
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::providers::Provider;

    use datalib_sql::{test_sql, test_sql_id, Sql};
    pub(super) async fn index_pool(root: &Path) -> SqlitePool {
        crate::grid_index::open_index(&datalib_core::layout::grid_index_db(root))
            .await
            .unwrap()
    }

    pub(super) fn doc(source: &str, uuid: &str, text: &str) -> RenderedMarkdown {
        let row = GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Test)
            .kind("Test")
            .source_label("Test")
            .conversation_uuid(uuid)
            .entire_chat(format!("/chat/{uuid}"))
            .body(text)
            .markdown_uuid(Some(uuid.to_string()))
            .created_at(Some("2026-01-01T00:00:00+00:00".to_string()))
            .is_document(true)
            .item_count(Some(1))
            .build()
            .unwrap();
        RenderedMarkdown {
            searchable: true,
            content_hash: None,
            markdown_uuid: uuid.to_string(),
            source_id: source.to_string(),
            // Fingerprint follows the text, the way a renderer's does.
            upstream_cursor: None,
            render_unit_key: None,
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            front: Default::default(),
            edges: Vec::new(),
            contacts: Vec::new(),
            data_nodes: Vec::new(),
            filings: Vec::new(),
            photos: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// A document must name its source and hold only its own rows; a
    /// renderer that forgets its group's id would otherwise file every row
    /// under nothing.
    #[test]
    fn misfiled_says_what_a_document_got_wrong() {
        let good = doc("enterprise", "d-1", "Engage.");
        assert_eq!(misfiled(&good), None);

        let nameless = RenderedMarkdown {
            source_id: String::new(),
            ..good.clone()
        };
        let why = misfiled(&nameless).expect("refused");
        assert!(why.contains("names no source"), "{why}");

        let mut stray = good.clone();
        stray.rows[0].markdown_uuid = Some("d-2".to_string());
        let why = misfiled(&stray).expect("refused");
        assert!(why.contains("as its document"), "{why}");
    }

    fn rendered_root(root: &Path, source: &str) -> std::path::PathBuf {
        datalib_etl::layout::render_markdown_root(root, source)
    }

    pub(super) fn render(root: &Path, source: &str, docs: &[RenderedMarkdown]) {
        let store = IndexedMarkdownStore::open(&rendered_root(root, source)).unwrap();
        for d in docs {
            store.put_document(d).unwrap();
        }
        store.commit("test render").unwrap();
        store.close();
    }

    pub(super) fn unrender(root: &Path, source: &str, uuid: &str) {
        let store = IndexedMarkdownStore::open(&rendered_root(root, source)).unwrap();
        store.remove_documents(&[uuid.to_string()]).unwrap();
        store.commit("test unrender").unwrap();
        store.close();
    }

    /// A document under a render unit other than itself: the shape the
    /// deletion path has to handle.
    fn doc_in_render_unit(source: &str, uuid: &str, render_unit: &str) -> RenderedMarkdown {
        let mut md = doc(source, uuid, "body");
        md.rows[0].conversation_uuid = render_unit.to_string();
        md.render_unit_key = Some(render_unit.to_string());
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
        render(root, "kept", &[doc("kept", "md-k", "kept body")]);
        render(root, "dropped", &[doc("dropped", "md-d", "dropped body")]);

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

    /// The hash of a document's text goes from the renderer through the
    /// render store to the index, for the search index to tell a changed
    /// document by.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_documents_hash_reaches_the_index() {
        use crate::section::{join, Section};
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let mut d = doc("ten-forward", "md-s", "two messages");
        d.sections = vec![
            Section::unkeyed("---\n---\n\n# Ten Forward\n\n".into()),
            Section::keyed(
                "m1",
                "<div data-section-uuid=\"m1\">\n\nTea, Earl Grey\n\n</div>\n\n".into(),
            ),
        ];
        render(root, "ten-forward", &[d.clone()]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        let hash: Option<String> =
            test_sql!("SELECT content_hash FROM markdowns WHERE markdown_uuid = 'md-s'")
                .query_scalar()
                .fetch_one(&pool)
                .await
                .unwrap();
        let text = join(&d.sections);
        assert_eq!(
            hash,
            Some(blake3::hash(text.as_bytes()).to_hex().to_string())
        );
    }

    /// A source taken out of the graph leaves the index with its cursor,
    /// so a renamed source does not show every document twice and one
    /// added back is read whole.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_no_longer_listed_leaves_the_index() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        render(root, "kept", &[doc("kept", "md-k", "kept body")]);
        render(root, "dropped", &[doc("dropped", "md-d", "dropped body")]);
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

    /// One render unit, several rendered documents, all of them gone when the
    /// render unit is.
    ///
    /// The fan-out is the reason the store answers "which documents are
    /// under this render unit" rather than the renderer naming the document it
    /// wants dropped: slack, signal and beeper split one conversation
    /// across periods, and once the conversation is gone from the raw
    /// store nothing but this store still knows how many periods it had.
    #[tokio::test(flavor = "multi_thread")]
    async fn removing_a_render_unit_takes_every_period_it_rendered_into() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let conv = "conv-1";
        let jan = doc_in_render_unit("src", "md-jan", conv);
        let feb = doc_in_render_unit("src", "md-feb", conv);
        let other = doc_in_render_unit("src", "md-other", "conv-2");
        render(root, "src", &[jan, feb, other]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(index_row_count(&pool).await, 3);

        let store = IndexedMarkdownStore::open(&rendered_root(root, "src")).unwrap();
        let mut gone: Vec<String> = store
            .documents_for_render_units(&[conv])
            .unwrap()
            .into_iter()
            .map(|(_, uuid)| uuid)
            .collect();
        gone.sort();
        assert_eq!(
            gone,
            vec!["md-feb".to_string(), "md-jan".to_string()],
            "both of the render unit's periods, and only those"
        );
        store.remove_documents(&gone).unwrap();
        store.commit("test: conversation gone upstream").unwrap();
        store.close();

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
    /// What an older build leaves: a column under another name, and the
    /// shape it records saying so.
    async fn into_older_shape(root: &Path, source: &str) {
        alter_store(
            root,
            source,
            test_sql!("ALTER TABLE grid_rows RENAME COLUMN preview TO text"),
            "an older build's shape",
        )
        .await;
        alter_store(
            root,
            source,
            test_sql!(
                "UPDATE _datalib_meta SET value = 'an-older-shape' WHERE key = 'schema_hash'"
            ),
            "the shape it records",
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
        render(root, "fresh", &[doc("fresh", "md-f", "fresh body")]);
        render(root, "stale", &[doc("stale", "md-s", "stale body")]);
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();

        render(root, "fresh", &[doc("fresh", "md-f2", "more")]);
        into_older_shape(root, "stale").await;
        // As after the index rebuilds itself for a new build: no cursors,
        // so every store is read whole.
        test_sql!("DELETE FROM source_cursors")
            .query()
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
            test_sql!("SELECT severity, sample FROM problems WHERE source_id = 'stale'")
                .query_as()
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
        render(root, "stale", &[doc("stale", "md-s", "stale body")]);
        let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert!(s.sources_unreadable.is_empty());
        let warnings: i64 = test_sql!("SELECT COUNT(*) FROM problems WHERE source_id = 'stale'")
            .query_scalar()
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
        render(root, "broken", &[doc("broken", "md-b", "broken body")]);
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();

        render(root, "fresh", &[doc("fresh", "md-f", "fresh body")]);
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
            test_sql!("SELECT severity FROM problems WHERE source_id = 'broken'")
                .query_scalar()
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(severity, "error");

        std::fs::remove_file(&store).unwrap();
        render(root, "broken", &[doc("broken", "md-b", "broken body")]);
        let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert!(s.sources_failed.is_empty());
        let errors: i64 = test_sql!("SELECT COUNT(*) FROM problems WHERE source_id = 'broken'")
            .query_scalar()
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
        render(root, "fresh", &[doc("fresh", "md-f", "fresh body")]);
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
    async fn alter_store(root: &Path, source: &str, sql: Sql, message: &str) {
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
        sql.query().execute(&writer).await.unwrap();
        datalib_etl::doltlite_raw::commit_run(&writer, message)
            .await
            .unwrap();
        writer.close().await;
    }

    async fn rows_of(pool: &SqlitePool, source: &str) -> Vec<String> {
        test_sql!(
            "SELECT g.uuid FROM grid_rows g JOIN markdowns m USING (markdown_uuid) \
              WHERE m.source_id = ? ORDER BY g.uuid"
        )
        .query_scalar()
        .bind(source)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// The grid holds each document's containers as the store does, and a
    /// re-render that names none takes them out of the grid too.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_grid_carries_each_documents_containers() {
        use datalib_schema::data_nodes::DataNodeRow;
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let sources = ["s".to_string()];
        let mut d = doc("s", "md-a", "body");
        d.data_nodes = vec![DataNodeRow::new("md-a", "md-a", "s", "Chat").read_together()];
        d.filings = vec![datalib_schema::filings::FilingRow::new(
            "md-a", "md-a", "label",
        )];
        render(root, "s", &[d]);
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        let count = |table: &'static str| {
            let pool = pool.clone();
            async move {
                Sql::dynamic(test_sql_id!(), format!("SELECT count(*) FROM {table}"))
                    .query_scalar::<i64>()
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!((count("data_nodes").await, count("filings").await), (1, 1));

        render(root, "s", &[doc("s", "md-a", "body again")]);
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert_eq!((count("data_nodes").await, count("filings").await), (0, 0));
    }

    /// A data node no document holds a row for (a channel, the workspace
    /// above it) gets a row of its own, from what its carriers say. The row
    /// stays while any document carries its node and goes with the last; a
    /// node that is a document's row gets no second one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_without_a_row_gets_one_while_a_document_carries_it() {
        use datalib_schema::data_nodes::{DataNodeRow, Lineage};
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let sources = ["s".to_string()];
        let carrying = |uuid: &str| {
            let mut d = doc("s", uuid, "body");
            let workspace = DataNodeRow::new(uuid, "ws", "s", "Workspace")
                .named(Some("Enterprise".to_string()))
                .place();
            let at = Lineage::top().below(&workspace);
            let channel = DataNodeRow::new(uuid, "chan", "s", "Channel")
                .named(Some("bridge".to_string()))
                .place()
                .under(&at);
            let at = at.below(&channel);
            let chat = DataNodeRow::new(uuid, uuid, "s", "Chat").read_together();
            d.data_nodes = vec![workspace, channel, chat.under(&at)];
            d
        };
        let build = || async {
            build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();
        };
        let node_rows = || async {
            test_sql!(
                "SELECT uuid, kind, conversation_name, parent_uuid, source_id FROM grid_rows \
                 WHERE markdown_uuid IS NULL ORDER BY uuid"
            )
            .query_as::<(
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
            )>()
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        let rows_of_md_a = || async {
            test_sql!("SELECT count(*) FROM grid_rows WHERE uuid = 'md-a'")
                .query_scalar::<i64>()
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        render(root, "s", &[carrying("md-a"), carrying("md-b")]);
        build().await;
        let s = || Some("s".to_string());
        assert_eq!(
            node_rows().await,
            [
                (
                    "chan".to_string(),
                    "Channel".to_string(),
                    Some("bridge".to_string()),
                    Some("ws".to_string()),
                    s()
                ),
                (
                    "ws".to_string(),
                    "Workspace".to_string(),
                    Some("Enterprise".to_string()),
                    None,
                    s()
                ),
            ]
        );
        assert_eq!(rows_of_md_a().await, 1);

        unrender(root, "s", "md-a");
        build().await;
        assert_eq!(node_rows().await.len(), 2, "md-b still carries both");

        unrender(root, "s", "md-b");
        build().await;
        assert_eq!(node_rows().await, []);
    }

    /// What an index says of every node and of the rows written for them.
    async fn nodes_and_their_rows(pool: &SqlitePool) -> Vec<String> {
        let nodes: Vec<datalib_schema::nodes::NodeRow> =
            test_sql!("SELECT * FROM nodes ORDER BY uuid")
                .query_as()
                .fetch_all(pool)
                .await
                .unwrap();
        type Row = (
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
        );
        let rows: Vec<Row> = test_sql!(
            "SELECT uuid, kind, conversation_name, parent_uuid, sort_key, markdown_uuid \
             FROM grid_rows ORDER BY uuid"
        )
        .query_as()
        .fetch_all(pool)
        .await
        .unwrap();
        nodes
            .iter()
            .map(|n| format!("{n:?}"))
            .chain(rows.iter().map(|r| format!("{r:?}")))
            .collect()
    }

    /// Only the nodes a pass touched are rewritten, so after each change the
    /// index holds what one built from nothing holds: a node is told of by
    /// the document that holds its row, else by its newest, whichever
    /// comes, changes or goes, and a page whose document goes while its
    /// subpages still name it becomes a row of its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn touched_nodes_are_rewritten_as_a_build_from_nothing_would() {
        use datalib_schema::data_nodes::{DataNodeRow, Lineage};
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let sources = ["s".to_string()];
        // A page under a book, as `told` by the document `by`.
        let page = |by: &str, uuid: &str, name: &str, at: &Lineage, key: &str| {
            DataNodeRow::new(by, uuid, "s", "Page")
                .named(Some(name.to_string()))
                .place()
                .sort_key(key.to_string())
                .under(at)
        };
        let book = |by: &str, name: &str| {
            DataNodeRow::new(by, "book", "s", "Book")
                .named(Some(name.to_string()))
                .place()
        };
        // The page "a" tells of itself; its subpages "b" and "c" name it
        // too, each otherwise.
        let a = |text: &str| {
            let mut d = doc("s", "a", text);
            let book = book("a", "Book");
            let at = Lineage::top().below(&book);
            d.data_nodes = vec![book, page("a", "a", "Title", &at, "1")];
            d
        };
        let sub = |uuid: &str, book_name: &str| {
            let mut d = doc("s", uuid, "body");
            let book = book(uuid, book_name);
            let at = Lineage::top().below(&book);
            let above = page(uuid, "a", &format!("Title by {uuid}"), &at, "2");
            let at = at.below(&above);
            d.data_nodes = vec![book, above, page(uuid, uuid, uuid, &at, "")];
            d
        };
        let build = || async {
            build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();
        };
        let name_of = |uuid: &'static str| {
            let pool = pool.clone();
            async move {
                test_sql!("SELECT name FROM nodes WHERE uuid = ?")
                    .query_scalar::<String>()
                    .bind(uuid)
                    .fetch_optional(&pool)
                    .await
                    .unwrap()
            }
        };
        let afresh = || async {
            let fresh = tempdir().unwrap();
            let db = fresh.path().join("db.sqlite");
            let pool = crate::grid_index::open_index(&db).await.unwrap();
            build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();
            let held = nodes_and_their_rows(&pool).await;
            crate::grid_index::close_index(pool).await.unwrap();
            held
        };

        render(root, "s", &[sub("b", "Book")]);
        build().await;
        assert_eq!(nodes_and_their_rows(&pool).await, afresh().await);
        assert_eq!(name_of("a").await.as_deref(), Some("Title by b"));

        // Its own document arrives, and then a newer document names it.
        render(root, "s", &[a("text")]);
        build().await;
        assert_eq!(nodes_and_their_rows(&pool).await, afresh().await);
        assert_eq!(name_of("a").await.as_deref(), Some("Title"));
        render(root, "s", &[sub("c", "Book, renamed")]);
        build().await;
        assert_eq!(nodes_and_their_rows(&pool).await, afresh().await);
        assert_eq!(name_of("a").await.as_deref(), Some("Title"));
        assert_eq!(name_of("book").await.as_deref(), Some("Book, renamed"));

        // Its document still tells of it but no longer holds its row, then
        // holds it again.
        let mut rowless = a("text");
        rowless.rows[0].uuid = "a-moved".to_string();
        render(root, "s", &[rowless]);
        build().await;
        assert_eq!(nodes_and_their_rows(&pool).await, afresh().await);
        assert_eq!(name_of("a").await.as_deref(), Some("Title by c"));
        render(root, "s", &[a("text")]);
        build().await;
        assert_eq!(nodes_and_their_rows(&pool).await, afresh().await);
        assert_eq!(name_of("a").await.as_deref(), Some("Title"));

        // A page whose own document says nothing of it: its row is all the
        // document holds of it, and when that goes it gets one here.
        let mut under_quiet = doc("s", "d", "body");
        under_quiet.data_nodes = vec![page("d", "quiet", "Quiet", &Lineage::top(), "3")];
        render(root, "s", &[doc("s", "quiet", "text"), under_quiet]);
        build().await;
        assert_eq!(nodes_and_their_rows(&pool).await, afresh().await);
        unrender(root, "s", "quiet");
        build().await;
        assert_eq!(nodes_and_their_rows(&pool).await, afresh().await);

        // A document that stays stops naming what it sat in, then names it
        // again.
        render(root, "s", &[doc("s", "c", "body")]);
        build().await;
        assert_eq!(nodes_and_their_rows(&pool).await, afresh().await);
        assert_eq!(name_of("book").await.as_deref(), Some("Book"));
        render(root, "s", &[sub("c", "Book, renamed")]);
        build().await;
        assert_eq!(nodes_and_their_rows(&pool).await, afresh().await);

        // Its own document goes, then its newest, then its last.
        for (gone, a_is, book_is) in [
            ("a", Some("Title by c"), Some("Book, renamed")),
            ("c", Some("Title by b"), Some("Book")),
            ("b", None, None),
        ] {
            unrender(root, "s", gone);
            build().await;
            assert_eq!(
                nodes_and_their_rows(&pool).await,
                afresh().await,
                "without {gone}"
            );
            assert_eq!(name_of("a").await.as_deref(), a_is, "without {gone}");
            assert_eq!(name_of("book").await.as_deref(), book_is, "without {gone}");
        }
    }

    /// A store that records this build's shape and lacks a table is
    /// damaged, not older: a read of the table fails loudly, naming it,
    /// and costs only its own source. A store from a build before #962 lacked
    /// `source_contacts` and kept every other source out of the grid, so
    /// this drops each table in turn: whichever it is, the pass indexes
    /// the other source and keeps this one's rows as they were.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_missing_any_one_table_costs_only_its_own_source() {
        for (table, _) in crate::indexed_markdown::store_tables() {
            let td = tempdir().unwrap();
            let root = td.path();
            let pool = index_pool(root).await;
            // "damaged" sorts first, so a pass that stops on it reaches nothing.
            let sources = ["damaged".to_string(), "fresh".to_string()];
            render(root, "damaged", &[doc("damaged", "md-d", "kept")]);
            build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();

            render(root, "fresh", &[doc("fresh", "md-f", "fresh body")]);
            render(root, "damaged", &[doc("damaged", "md-d2", "more")]);
            alter_store(
                root,
                "damaged",
                Sql::dynamic(test_sql_id!(), format!("DROP TABLE {table}")),
                "a build without this table",
            )
            .await;
            let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap_or_else(|e| panic!("without {table}: {e:#}"));
            // A table the pass does not read is not missed here; one it
            // does fails, saying which.
            assert!(
                s.sources_failed
                    .iter()
                    .all(|(source, why)| source == "damaged" && why.contains("no such table")),
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
            test_sql!(
                "UPDATE _datalib_meta SET value = 'an-older-shape' WHERE key = 'schema_hash'"
            ),
            test_sql!("DROP TABLE _datalib_meta"),
        ] {
            let td = tempdir().unwrap();
            let root = td.path();
            let pool = index_pool(root).await;
            let sources = ["old".to_string()];
            render(root, "old", &[doc("old", "md-1", "one")]);
            build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();

            render(root, "old", &[doc("old", "md-2", "two")]);
            alter_store(root, "old", change.clone(), "another build's shape").await;
            let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();
            assert_eq!(s.sources_unreadable, ["old"], "{change}");
            assert_eq!(s.markdowns_total, 0, "{change}: nothing was read");
            assert_eq!(rows_of(&pool, "old").await, ["md-1"], "{change}");

            // Its next render writes this build's shape, and it reads again.
            render(root, "old", &[doc("old", "md-2", "two")]);
            let s = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
                .await
                .unwrap();
            assert!(s.sources_unreadable.is_empty(), "{change}");
            assert_eq!(rows_of(&pool, "old").await, ["md-1", "md-2"], "{change}");
            pool.close().await;
        }
    }

    async fn index_row_count(pool: &SqlitePool) -> i64 {
        test_sql!("SELECT COUNT(*) FROM grid_rows")
            .query_scalar()
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
            &[doc("src", "md-1", "a"), doc("src", "md-2", "b")],
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
        render(root, "src", &[doc("src", "md-1", "a")]);
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
            &[doc("src", "md-1", "a"), doc("src", "md-2", "b")],
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
    /// Each document's people as the grid index holds them.
    async fn people(pool: &SqlitePool) -> Vec<(String, String, Option<i64>)> {
        test_sql!(
            "SELECT d.markdown_uuid, r.contact_json, d.seen_items FROM document_contacts d \
             JOIN contact_records r USING (record_id) ORDER BY 1, 2"
        )
        .query_as()
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// A build that changes only the contacts tables empties them on open,
    /// and the next pass fills them from each source's people alone: no
    /// document is read or written, the grid's rows are as they were, and
    /// the people are what a pass from nothing wrote.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_of_the_contacts_tables_fills_them_alone() {
        use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact};
        let td = tempdir().unwrap();
        let root = td.path();
        let mut carrying = doc("src", "md-1", "body");
        let mut riker = NormalizedContact::new("src", "U_RIKER", ContactKind::Person);
        riker.names = vec!["Will Riker".into()];
        riker.handles = vec![ContactHandle::email(None, "riker@enterprise.org")];
        carrying.contacts = vec![riker];
        render(root, "src", &[carrying, doc("src", "md-2", "other")]);
        let sources = ["src".to_string()];
        let pool = index_pool(root).await;
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        let (held, rows) = (people(&pool).await, index_row_count(&pool).await);
        assert_eq!(held.len(), 1);
        test_sql!(
            "UPDATE _datalib_meta SET value = 'older' \
             WHERE key IN ('schema_hash', 'schema_hash.contacts')"
        )
        .query()
        .execute(&pool)
        .await
        .unwrap();
        crate::grid_index::close_index(pool).await.unwrap();

        let pool = index_pool(root).await;
        assert!(people(&pool).await.is_empty(), "the open empties them");
        assert_eq!(index_row_count(&pool).await, rows, "and keeps the grid");
        let filled = build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert_eq!(
            (filled.markdowns_total, filled.markdowns_loaded),
            (0, 0),
            "{filled:?}"
        );
        assert_eq!(people(&pool).await, held);
        assert_eq!(index_row_count(&pool).await, rows);
        let cursors: Vec<(String, String)> = test_sql!(
            "SELECT c.source_id, c.store_commit FROM contact_cursors c \
             JOIN source_cursors s ON s.source_id = c.source_id AND s.store_commit = c.store_commit"
        )
        .query_as()
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(cursors.len(), 1, "the contacts' cursor is the grid's");
        crate::grid_index::close_index(pool).await.unwrap();
    }

    async fn sealed_cursors(pool: &SqlitePool) -> Vec<String> {
        test_sql!("SELECT source_id FROM source_cursors ORDER BY source_id")
            .query_scalar()
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// Those not made, or that the planner is told nothing of: dropping an
    /// index drops that too.
    async fn missing_search_indexes(pool: &SqlitePool) -> Vec<&'static str> {
        let present: Vec<String> = test_sql!(
            "SELECT name FROM sqlite_master JOIN sqlite_stat1 ON idx = name WHERE type = 'index'"
        )
        .query_scalar()
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
                .map(|i| doc(source, &format!("md-{source}{i}"), "body"))
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
                if m.starts_with("b: loaded ") {
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
                .map(|i| doc("src", &format!("md-{i}"), text))
                .collect()
        };
        render(root, "src", &docs("before"));
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();

        render(root, "src", &docs("after"));
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(missing_search_indexes(&pool).await, Vec::<&str>::new());
        let bodies: Vec<String> = test_sql!("SELECT DISTINCT preview FROM grid_rows")
            .query_scalar()
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(bodies, vec!["after".to_string()]);
        // Incremental (2): the mode a file takes only when it is set before
        // its first table, and without which the remade indexes' pages
        // come from wherever the free list scattered them.
        let auto_vacuum: i64 = test_sql!("PRAGMA auto_vacuum")
            .query_scalar()
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(auto_vacuum, 2);
    }

    /// Guards a reset of a source that held most of the index: its
    /// render emptied, and the index deleted its documents one at a time
    /// with every search index kept up row by row, since only documents
    /// loaded counted toward remaking them. A large removal, and a large
    /// source taken out of the graph, remake them like a large load.
    #[tokio::test(flavor = "multi_thread")]
    async fn removing_most_of_the_index_remakes_its_search_indexes() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let many: Vec<_> = (1..=20)
            .map(|i| doc("big", &format!("md-big{i}"), "big"))
            .collect();
        render(root, "big", &many);
        render(root, "small", &[doc("small", "md-small", "small")]);
        let both = ["big".to_string(), "small".to_string()];
        build_grid_index_for(&pool, root, &both, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert_eq!(index_row_count(&pool).await, 21);

        let store = IndexedMarkdownStore::open(&rendered_root(root, "big")).unwrap();
        let gone: Vec<String> = many.iter().map(|d| d.markdown_uuid.clone()).collect();
        store.remove_documents(&gone[..19]).unwrap();
        store.commit("test: most of big is gone").unwrap();
        store.close();
        let emptied = build_grid_index_for(&pool, root, &both, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        assert_eq!(emptied.markdowns_removed, 19);
        assert_eq!(emptied.search_indexes_remade, 1, "{emptied:?}");
        assert_eq!(missing_search_indexes(&pool).await, Vec::<&str>::new());
        assert_eq!(index_row_count(&pool).await, 2);

        let retired = build_grid_index_for(
            &pool,
            root,
            &["small".to_string()],
            |_| {},
            None,
            &StopFlag::new(),
        )
        .await
        .unwrap();
        assert_eq!(retired.markdowns_removed, 1);
        assert_eq!(retired.search_indexes_remade, 1, "{retired:?}");
        assert_eq!(missing_search_indexes(&pool).await, Vec::<&str>::new());
        assert_eq!(index_row_count(&pool).await, 1);
        let terms: i64 =
            test_sql!("SELECT count(*) FROM rows WHERE uuid NOT IN (SELECT uuid FROM grid_rows)")
                .query_scalar()
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(terms, 0, "no removed row keeps its search terms");
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
                .map(|i| doc(source, &format!("md-{source}{i}"), "body"))
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
                if m.starts_with("b: loaded ") {
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
        render(root, "src", &[doc("src", "md-1", "a")]);

        // A second document left in the working set, the way a render that is
        // still running (or was killed) leaves one.
        let store = IndexedMarkdownStore::open(&rendered_root(root, "src")).unwrap();
        store.put_document(&doc("src", "md-2", "b")).unwrap();
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
        render(root, "src", &[doc("src", "md-1", "a")]);
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
        test_sql!("SELECT dolt_checkout('datalib_writer')")
            .query()
            .execute(&pool)
            .await
            .unwrap();
        let first: String = test_sql!(
            "SELECT commit_hash FROM dolt_log() WHERE message LIKE 'Initialize data repository%'"
        )
        .query_scalar()
        .fetch_one(&pool)
        .await
        .unwrap();
        test_sql!("SELECT dolt_reset('--soft', ?)")
            .query()
            .bind(&first)
            .execute(&pool)
            .await
            .unwrap();
        datalib_etl::doltlite_raw::commit_run(&pool, "fold")
            .await
            .unwrap()
            .expect("a fold commit");
        test_sql!("SELECT dolt_gc()")
            .query()
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
            &[doc("src", "md-1", "a"), doc("src", "md-2", "b")],
        );
        render(root, "src", &[doc("src", "md-2", "b-again")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        let before = load_source_cursors(&pool).await.unwrap()["src"].clone();

        fold(root, "src").await;
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_total, 0, "the fold changed no document");
        let after = load_source_cursors(&pool).await.unwrap()["src"].clone();
        assert_ne!(after.commit, before.commit);
        assert_eq!(after.tree, before.tree);

        render(root, "src", &[doc("src", "md-1", "a-changed")]);
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
            &[doc("src", "md-1", "a"), doc("src", "md-2", "b")],
        );
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();

        // md-2 changes; md-1 does not.
        render(root, "src", &[doc("src", "md-2", "b-changed")]);
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_total, 1, "one document read, not two");
        assert_eq!(s.markdowns_loaded, 1);

        let preview: String = test_sql!("SELECT preview FROM grid_rows WHERE uuid = 'md-2'")
            .query_scalar()
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
            &[doc("src", "md-1", "a"), doc("src", "md-2", "b")],
        );
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(index_row_count(&pool).await, 2);

        unrender(root, "src", "md-2");
        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_removed, 1, "the dropped document is reported");
        assert_eq!(index_row_count(&pool).await, 1, "and its rows are gone");
        let left: String = test_sql!("SELECT uuid FROM grid_rows")
            .query_scalar()
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, "md-1");
    }

    /// The index keeps no copy of a source's problems: a reader finds
    /// them in the render store itself. A copy an older build left goes
    /// on the next pass that reads the source.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_index_keeps_no_copy_of_a_sources_problems() {
        use datalib_etl::bulk::BulkUpsertable as _;
        use datalib_schema::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Stage};
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let problem = ProblemRow::new(
            "src",
            Stage::GridRow,
            Scope::Markdown("md-1"),
            Some("md-1"),
            Outcome::Nulled,
            Problem::field("created_at", Reason::CoercionFailed, "yesterday"),
            Some(1),
        );
        let mut bad = doc("src", "md-1", "a");
        bad.problems.push(problem.clone());
        render(root, "src", &[bad, doc("src", "md-2", "b")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        let count = || async {
            test_sql!("SELECT COUNT(*) FROM problems")
                .query_scalar::<i64>()
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        assert_eq!(count().await, 0, "the render store's row was not copied");

        problem
            .stamped(None, "2370-01-01T00:00:00.000000+00:00", None)
            .bind_into(
                Sql::dynamic(
                    test_sql_id!(),
                    datalib_etl::bulk::insert_sql::<ProblemRow>(),
                )
                .query(),
            )
            .execute(&pool)
            .await
            .unwrap();
        render(root, "src", &[doc("src", "md-2", "b again")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(count().await, 0, "an older build's copy went");
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
        render(root, "src", &[doc("src", "md-1", "a")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();

        // A commit and a tree from no history anyone has.
        test_sql!(
            "UPDATE source_cursors SET store_commit = ?, store_tree = ? WHERE source_id = 'src'"
        )
        .query()
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
            &[doc("src", "md-1", "a"), doc("src", "md-2", "b")],
        );
        let first = build_grid_index(&pool, root, |_| {}, None).await.unwrap();

        render(root, "src", &[doc("src", "md-2", "b, changed")]);
        test_sql!("DELETE FROM source_cursors")
            .query()
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
        let mut md = doc("src", "md-1", "a");
        let mut child = md.rows[0].clone();
        child.is_document = false;
        md.rows.push(child);
        let lock = crate::grid_index::WriteLock::for_index(pool.clone());
        let err = crate::grid_index::apply_one(&lock, &md).await.unwrap_err();
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
            &[doc("src", "md-1", "a"), doc("src", "md-2", "b")],
        );
        render(root, "other", &[doc("other", "md-3", "c")]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(index_row_count(&pool).await, 3);

        unrender(root, "src", "md-2");
        // Lose the range, so the next pass reads the store whole.
        test_sql!("DELETE FROM source_cursors WHERE source_id = 'src'")
            .query()
            .execute(&pool)
            .await
            .unwrap();

        let s = build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(s.markdowns_removed, 1, "the dropped document is reported");
        let mut left: Vec<String> = test_sql!("SELECT uuid FROM grid_rows")
            .query_scalar()
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
            let mut md = doc("src", doc_uuid, doc_uuid);
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
        let moved: (String, Option<String>) =
            test_sql!("SELECT markdown_uuid, supplied_terms FROM grid_rows WHERE uuid = 'msg-u'")
                .query_as()
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

#[cfg(test)]
mod work_tests {
    //! A pass that re-reads one small source does the same work whatever
    //! else the index holds. Every index bug we have shipped broke this: a
    //! lookup that read every document in the index once per source
    //! (#1926), or once per document. Work is counted in SQLite's VM steps
    //! (`datalib_sql_work`), not seconds, so a loaded machine cannot fail
    //! this nor a fast one pass it. The large source is not read, so the
    //! steps a large source costs cannot hide in a plan that looks fine:
    //! #1926's statement scanned no table.

    use std::collections::BTreeMap;
    use std::path::Path;

    use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact, Seen};
    use datalib_etl::stop::StopFlag;
    use datalib_schema::data_nodes::{DataNodeRow, Lineage};
    use tempfile::tempdir;

    use super::source_cursor_tests::{doc, index_pool, render, unrender};
    use super::{build_grid_index_for, RenderedMarkdown, INDEXED_DOCUMENTS};

    use datalib_sql::Sql;
    const SMALL: usize = 40;
    const LARGE: usize = 2_000;

    /// How much more a statement may do beside the large source. An
    /// index lookup's steps do not grow with the table at all; a statement
    /// that reads the large source does some fifty times more.
    const FACTOR: u64 = 2;
    /// Steps any statement may add whatever it did alone.
    const SLACK: u64 = 50;

    /// Statements that read the whole index once a pass by design, not
    /// once per source or per document.
    const ONCE_A_PASS: &[Sql] = &[INDEXED_DOCUMENTS];

    /// A document filed under its source's workspace and a channel, which
    /// need node rows of their own, and is itself a chat, which has its
    /// row; and the one person who wrote it, as in every document of its
    /// source.
    fn carrying(source: &str, uuid: &str, text: &str) -> RenderedMarkdown {
        let mut d = doc(source, uuid, text);
        let workspace = DataNodeRow::new(uuid, format!("{source}-ws"), source, "Workspace")
            .named(Some(format!("{source} workspace")))
            .place();
        let at = Lineage::top().below(&workspace);
        let channel = DataNodeRow::new(uuid, format!("{source}-chan"), source, "Channel")
            .named(Some("bridge".to_string()))
            .place()
            .under(&at);
        let at = at.below(&channel);
        let chat = DataNodeRow::new(uuid, uuid, source, "Chat").read_together();
        d.data_nodes = vec![workspace, channel, chat.under(&at)];
        let mut author =
            NormalizedContact::new(source, format!("{source}-picard"), ContactKind::Person);
        author.handles = vec![ContactHandle::email(None, "picard@enterprise.org")];
        author.seen = Some(Seen {
            items: uuid.len() as u64,
            last_at: Some(format!("2364-02-11 {uuid}")),
        });
        d.contacts = vec![author];
        d
    }

    fn documents(source: &str, n: usize) -> Vec<RenderedMarkdown> {
        (0..n)
            .map(|i| carrying(source, &format!("{source}-{i:05}"), "body"))
            .collect()
    }

    /// One statement's runs on one kind of store in one pass.
    #[derive(Debug, Default, Clone, Copy)]
    struct Runs {
        count: u64,
        most_steps: u64,
    }

    /// Where a statement ran, with the source's own folder named alike for
    /// every source: a render store's read of an unchanged large source is
    /// held to what the small source's read costs.
    fn store_kind(root: &Path, file: &str) -> String {
        let file = Path::new(file);
        let under = file.strip_prefix(root).unwrap_or(file);
        let mut parts = under.components();
        match parts.next().and_then(|p| p.as_os_str().to_str()) {
            Some("small" | "large") => format!("<source>/{}", parts.as_path().display()),
            _ => under.display().to_string(),
        }
    }

    /// The statements of a pass that re-reads the small source after one
    /// of its documents changed, one went and one came: few enough that
    /// neither index drops its search indexes for the load. With `large`,
    /// a large source is indexed beside it first and does not change.
    async fn small_sources_pass(root: &Path, large: bool) -> BTreeMap<(String, String), Runs> {
        let recording = datalib_sql_work::record_under(root);
        let pool = index_pool(root).await;
        let mut sources = vec!["small".to_string()];
        render(root, "small", &documents("small", SMALL));
        if large {
            sources.push("large".to_string());
            render(root, "large", &documents("large", LARGE));
        }
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        render(
            root,
            "small",
            &[
                carrying("small", "small-00000", "changed"),
                carrying("small", "small-new", "new"),
            ],
        );
        unrender(root, "small", "small-00001");
        recording.take();
        build_grid_index_for(&pool, root, &sources, |_| {}, None, &StopFlag::new())
            .await
            .unwrap();
        let ran = recording.take();
        pool.close().await;
        let root = root.canonicalize().unwrap();
        let mut runs: BTreeMap<_, Runs> = BTreeMap::new();
        for r in ran {
            if !datalib_sql_work::judge::reads_or_writes_rows(&r.sql) {
                continue;
            }
            let of = runs.entry((r.sql, store_kind(&root, &r.file))).or_default();
            of.count += 1;
            of.most_steps = of.most_steps.max(r.work.vm_steps);
        }
        runs
    }

    /// Guards #1926: the index step spent 11-20 s on each source of a
    /// 1.2M-row index, one document changed or none, reading a source's
    /// nodes by every document row in the index.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_small_sources_pass_costs_the_same_beside_a_large_source() {
        let (alone_dir, beside_dir) = (tempdir().unwrap(), tempdir().unwrap());
        let alone = small_sources_pass(alone_dir.path(), false).await;
        let beside = small_sources_pass(beside_dir.path(), true).await;
        assert!(
            alone
                .keys()
                .any(|(sql, _)| sql == super::nodes::WRITE_FROM_BEST_ACCOUNT.as_str()),
            "the pass settles no nodes; this test checks nothing: {:#?}",
            alone.keys()
        );
        let mut grew = Vec::new();
        for ((sql, store), b) in &beside {
            if ONCE_A_PASS.iter().any(|s| s.as_str() == sql.as_str()) {
                continue;
            }
            let a = alone.get(&(sql.clone(), store.clone())).copied();
            let within = a.is_some_and(|a| {
                b.most_steps <= FACTOR * a.most_steps + SLACK && b.count <= FACTOR * a.count
            });
            if !within {
                let sql = sql.split_whitespace().collect::<Vec<_>>().join(" ");
                grew.push(format!("{store}: alone {a:?}, beside {b:?}: {sql}"));
            }
        }
        assert!(
            grew.is_empty(),
            "these statements did more beside a large source:\n{}",
            grew.join("\n")
        );
    }
}
