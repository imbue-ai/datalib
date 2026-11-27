//! The per-source render output store: one doltlite database holding
//! every row a source's render produced, plus what render could not do.
//!
//! The index reads it by querying `dolt_diff` for what changed since the
//! commit it last read, which is also how a document a source stopped
//! holding gets named and deleted.
//!
//! Every write goes through a SQL transaction that leaves the store in a
//! state a consumer may read — a document whole with its rows, edges and
//! problems, never a document with its rows deleted and not yet re-inserted.
//! That is what lets a doltlite commit land at any moment between them
//! (a checkpoint, the end of the run) without anyone checking what is
//! in it. How many documents share one transaction is a throughput choice
//! ([`IndexedMarkdownStore::begin_batch`]): each transaction rewrites every
//! page it touched (docs/dev/doltlite.md#what-a-write-costs).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_contact_schema::NormalizedContact;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_schema::edges::DDL as EDGES_DDL;
use datalib_schema::grid_rows::DDL as GRID_ROWS_DDL;
use datalib_schema::markdowns::DDL as MARKDOWNS_DDL;
use datalib_schema::measurements::{SourceMeasurementRow, DDL as MEASUREMENTS_DDL};
use datalib_schema::problems::{
    ProblemRow, Reason, ScopeKind, Severity, Stage, DDL as PROBLEMS_DDL,
};
use datalib_schema::render_cursor::{RenderCursorRow, DDL as RENDER_CURSOR_DDL};
use datalib_schema::render_inputs::{DDL as RENDER_INPUTS_DDL, INDEX_DDL as RENDER_INPUTS_INDEX};
use datalib_schema::source_contact_handles::DDL as SOURCE_CONTACT_HANDLES_DDL;
use datalib_schema::source_contacts::DDL as SOURCE_CONTACTS_DDL;
use datalib_schema::source_photos::DDL as SOURCE_PHOTOS_DDL;

use crate::grid_index::{RenderedMarkdown, WriteLock};
use datalib_etl::bulk::BulkUpsertable;
use datalib_sql::{sql, sql_id, Sql};

/// File name inside a source's `render_markdown/`.
pub const STORE_FILE: &str = "indexed_markdown.doltlite_db";

/// What `remove_documents` deletes beside the rows
/// `grid_index::delete_document_rows` takes, the set bound as one JSON array.
const REMOVED_PROBLEMS: Sql = sql!(
    "etl.render.store.delete_problems_of_documents",
    "DELETE FROM problems \
     WHERE scope_kind = ? AND scope_key IN (SELECT value FROM json_each(?))"
);
const REMOVED_SECTIONS: Sql = sql!(
    "etl.render.store.delete_sections_of_documents",
    "DELETE FROM sections WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
);
const DOCUMENT_SECTIONS: Sql = sql!(
    "etl.render.store.delete_sections_of_document",
    "DELETE FROM sections WHERE markdown_uuid = ?"
);
const REMOVED_DRAWN: Sql = sql!(
    "etl.render.store.delete_drawings_of_documents",
    "DELETE FROM drawings WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
);
const DOCUMENT_DRAWN: Sql = sql!(
    "etl.render.store.delete_drawings_of_document",
    "DELETE FROM drawings WHERE markdown_uuid = ?"
);
const REMOVED_FOLDS: Sql = sql!(
    "etl.render.store.delete_folds_of_documents",
    "DELETE FROM folds WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
);
const DOCUMENT_FOLDS: Sql = sql!(
    "etl.render.store.delete_folds_of_document",
    "DELETE FROM folds WHERE markdown_uuid = ?"
);
/// Every column of `sections`, in the order a `SectionRow` is read.
const SECTION_COLUMNS: &str = "markdown_uuid, seq, uuid, text, framed, form, author, \
     author_handle, at, tags, link, reactions, replies, replies_held, continues, addressed, \
     labels, class";

/// A kept section as [`IndexedMarkdownStore::kept_sections`] reads it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct KeptSection {
    pub markdown_uuid: String,
    pub seq: i64,
    pub uuid: Option<String>,
    pub text: String,
    pub framed: bool,
    pub addressed: Option<String>,
    pub labels: Option<String>,
    pub author: Option<String>,
    pub author_handle: Option<String>,
    pub created_at: Option<String>,
}

impl KeptSection {
    fn piece(self) -> datalib_search_index::text::Piece {
        let addressed = self
            .addressed
            .as_deref()
            .and_then(datalib_schema::sections::parse_addressed)
            .unwrap_or_default()
            .into_iter()
            .map(|a| datalib_search_index::text::Addressed {
                role: a.role.as_str().to_string(),
                name: a.name,
                handle: a.handle,
            })
            .collect();
        datalib_search_index::text::Piece {
            uuid: self.uuid,
            text: self.text,
            framed: self.framed,
            author: self.author,
            author_handle: self.author_handle,
            created_at: self.created_at,
            addressed,
            labels: self
                .labels
                .as_deref()
                .and_then(datalib_schema::sections::parse_labels)
                .unwrap_or_default(),
        }
    }
}

/// What the search index files a document under: its render's hash and
/// the drawing's version, so a change to either draws it again. `None` for
/// a document its renderer handed without sections, which has no text kept.
pub fn index_key(row: &datalib_schema::markdowns::MarkdownRow) -> Option<String> {
    row.content_hash
        .as_ref()
        .map(|h| format!("{h}{}", datalib_search_index::text::key_suffix()))
}

/// Replace what the store keeps of `md`'s sections, and how each is
/// drawn. The grid index keeps none, so this is the render store's own
/// write beside `grid_index::apply_one`. A document read back from the
/// store has no sections, and keeps the ones it has, as it keeps its
/// layout.
async fn put_sections(write_lock: &WriteLock, md: &RenderedMarkdown) -> Result<()> {
    if !md.rows.is_empty() && md.sections.is_empty() {
        return Ok(());
    }
    let mut guard = write_lock.acquire().await?;
    let conn = guard.conn();
    for sql in [DOCUMENT_SECTIONS, DOCUMENT_DRAWN, DOCUMENT_FOLDS] {
        sql.query()
            .bind(&md.markdown_uuid)
            .execute(&mut **conn)
            .await
            .context("delete prior sections")?;
    }
    if md.rows.is_empty() {
        return Ok(());
    }
    for row in crate::section::drawn(&md.markdown_uuid, &md.sections) {
        sql!(
            "etl.render.store.insert_drawing",
            "INSERT INTO drawings (markdown_uuid, seq, md) VALUES (?, ?, ?)"
        )
        .query()
        .bind(&row.markdown_uuid)
        .bind(row.seq)
        .bind(&row.md)
        .execute(&mut **conn)
        .await
        .with_context(|| format!("document {}: store drawing {}", md.markdown_uuid, row.seq))?;
    }
    for row in crate::section::folds(&md.markdown_uuid, &md.sections) {
        sql!(
            "etl.render.store.insert_fold",
            "INSERT INTO folds (markdown_uuid, seq, class, summary) VALUES (?, ?, ?, ?)"
        )
        .query()
        .bind(&row.markdown_uuid)
        .bind(row.seq)
        .bind(&row.class)
        .bind(&row.summary)
        .execute(&mut **conn)
        .await
        .with_context(|| format!("document {}: store fold {}", md.markdown_uuid, row.seq))?;
    }
    for row in crate::section::stored(&md.markdown_uuid, &md.sections) {
        // Every value bound; the column list is a constant.
        Sql::dynamic(
            sql_id!("etl.render.store.insert_section"),
            format!(
                "INSERT INTO sections ({SECTION_COLUMNS}) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
        )
        .query()
        .bind(&row.markdown_uuid)
        .bind(row.seq)
        .bind(&row.uuid)
        .bind(&row.text)
        .bind(row.framed)
        .bind(&row.form)
        .bind(&row.author)
        .bind(&row.author_handle)
        .bind(&row.at)
        .bind(&row.tags)
        .bind(&row.link)
        .bind(&row.reactions)
        .bind(row.replies)
        .bind(row.replies_held)
        .bind(row.continues)
        .bind(&row.addressed)
        .bind(&row.labels)
        .bind(&row.class)
        .execute(&mut **conn)
        .await
        .with_context(|| format!("document {}: store section {}", md.markdown_uuid, row.seq))?;
    }
    Ok(())
}

/// Where a page of a document starts ([`IndexedMarkdownStore::document_page`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageAsk {
    /// The first page.
    Start,
    /// The page around the piece of this item; the first page when the
    /// document has no piece of it.
    At(String),
    /// The page from this cursor on.
    After(i64),
    /// The page ending just before this cursor.
    Before(i64),
}

/// A page of a document: its pieces, spelled, and the cursors either side
/// of them, `None` where the page reaches that end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentPage {
    pub sections: Vec<crate::section::SpelledSection>,
    pub before: Option<i64>,
    pub after: Option<i64>,
    /// The page starts at the document's first piece, where its front
    /// matter is.
    pub from_top: bool,
}

type Pieces = (
    Vec<datalib_schema::sections::SectionRow>,
    Vec<datalib_schema::drawings::DrawingRow>,
    Vec<datalib_schema::folds::FoldRow>,
);

/// The rows the store keeps of a document's pieces from place `first` to
/// `last`, both included.
async fn pieces_between(
    pool: &SqlitePool,
    markdown_uuid: &str,
    first: i64,
    last: i64,
) -> Result<Pieces> {
    // The column list is a constant; every value is bound.
    let sections = Sql::dynamic(
        sql_id!("etl.render.store.read_sections_in_range"),
        format!(
            "SELECT {SECTION_COLUMNS} \
         FROM sections WHERE markdown_uuid = ? AND seq BETWEEN ? AND ? ORDER BY seq"
        ),
    )
    .query_as()
    .bind(markdown_uuid)
    .bind(first)
    .bind(last)
    .fetch_all(pool)
    .await
    .context("read a document's sections")?;
    let drawn = sql!(
        "etl.render.store.read_drawings_in_range",
        "SELECT markdown_uuid, seq, md FROM drawings \
         WHERE markdown_uuid = ? AND seq BETWEEN ? AND ? ORDER BY seq"
    )
    .query_as()
    .bind(markdown_uuid)
    .bind(first)
    .bind(last)
    .fetch_all(pool)
    .await
    .context("read a document's drawings")?;
    let folds = sql!(
        "etl.render.store.read_folds_in_range",
        "SELECT markdown_uuid, seq, class, summary FROM folds \
         WHERE markdown_uuid = ? AND seq BETWEEN ? AND ? ORDER BY seq"
    )
    .query_as()
    .bind(markdown_uuid)
    .bind(first)
    .bind(last)
    .fetch_all(pool)
    .await
    .context("read a document's folds")?;
    Ok((sections, drawn, folds))
}

pub fn path_for(rendered_root: &Path) -> PathBuf {
    rendered_root.join(STORE_FILE)
}

/// Every `CREATE TABLE` this store holds, in creation order.
///
/// One list so the DDL pass cannot cover a different set than the
/// schema check — the same reason `grid_index::index_ddl` exists.
fn store_ddl() -> Vec<&'static str> {
    store_tables()
        .map(|(_table, ddl)| ddl)
        .chain(std::iter::once(RENDER_INPUTS_INDEX))
        .collect()
}

/// Which grid row a raw record a download's problem names belongs to,
/// where this store holds that row: `<table>:<id>` as the problem spells
/// it, and the row's uuid. A reader joins a download's problems to it,
/// since the download never sees the ids rows are minted under.
pub const ENTITY_ITEMS_DDL: &[(&str, &str)] = &[(
    "entity_items",
    "CREATE TABLE IF NOT EXISTS entity_items (\n    entity_key TEXT PRIMARY KEY NOT NULL,\n    item_uuid TEXT NOT NULL\n)",
)];

/// `(table, CREATE TABLE)` for every table this store holds but
/// `_datalib_meta`, in creation order.
pub(crate) fn store_tables() -> impl Iterator<Item = (&'static str, &'static str)> {
    GRID_ROWS_DDL
        .iter()
        .chain(MARKDOWNS_DDL.iter())
        .chain(EDGES_DDL.iter())
        .chain(datalib_schema::data_nodes::DDL.iter())
        .chain(datalib_schema::filings::DDL.iter())
        .chain(datalib_schema::sections::DDL.iter())
        .chain(datalib_schema::drawings::DDL.iter())
        .chain(datalib_schema::folds::DDL.iter())
        .chain(SOURCE_CONTACTS_DDL.iter())
        .chain(SOURCE_CONTACT_HANDLES_DDL.iter())
        .chain(SOURCE_PHOTOS_DDL.iter())
        .chain(PROBLEMS_DDL.iter())
        .chain(ENTITY_ITEMS_DDL.iter())
        .chain(MEASUREMENTS_DDL.iter())
        .chain(RENDER_CURSOR_DDL.iter())
        .chain(RENDER_INPUTS_DDL.iter())
        .copied()
}

/// Indexes that change no row, so they stay out of [`schema_hash`] and
/// are recorded apart ([`shape`]): adding one must not re-render every
/// source. `open` creates them.
fn lookup_indexes() -> impl Iterator<Item = &'static str> {
    crate::grid_index::DOCUMENT_LOOKUP_INDEXES
        .iter()
        .copied()
        .chain([
            datalib_schema::problems::BY_SCOPE_INDEX_DDL,
            // The render asks it at every checkpoint (`documents_for_render_units`).
            "CREATE INDEX IF NOT EXISTS markdowns_by_render_unit ON markdowns (render_unit_key)",
            // Every render pass asks it once per download problem.
            "CREATE INDEX IF NOT EXISTS grid_rows_by_upstream_key \
             ON grid_rows (upstream_entity_kind, upstream_id)",
        ])
}

const GRID_ROW_BY_UPSTREAM: Sql = sql!(
    "etl.render.store.find_row_by_upstream_key",
    "SELECT uuid FROM grid_rows \
     INDEXED BY grid_rows_by_upstream_key \
     WHERE upstream_entity_kind = ? AND upstream_id = ? LIMIT 1"
);

const DOCUMENTS_FOR_RENDER_UNITS: Sql = sql!(
    "etl.render.store.list_documents_of_render_units",
    "SELECT render_unit_key, markdown_uuid \
     FROM markdowns INDEXED BY markdowns_by_render_unit \
     WHERE render_unit_key IN (SELECT value FROM json_each(?))"
);

/// This store's shape: what its `_datalib_meta` records, what the render
/// step folds into its params so a change to any render-store table
/// re-renders every source, and what the index compares before it reads
/// one. Nobody has to remember a bump, and a column added to `grid_rows`
/// is not `NULL` on every row rendered before it.
pub fn schema_hash() -> String {
    shape().tables
}

/// This store's whole shape, its lookup indexes included: what its owner
/// records and what [`IndexedMarkdownStore::open_for_reading`] requires.
pub fn shape() -> datalib_store_meta::Shape {
    let indexes: Vec<&str> = lookup_indexes().collect();
    datalib_etl::doltlite_raw::derived_shape(&store_ddl(), &indexes)
}

/// One raw row a render unit's render asked for, found or not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Input {
    /// A table of the raw store, bare name.
    pub table: String,
    /// Its primary key as text; a composite key's columns in
    /// the key's order, joined by `|` — see [`join_key`].
    pub id: String,
}

/// The `input_id` that stands for every row of a table: a render unit that
/// reads a table whole declares this rather than each key.
pub const WHOLE_TABLE: &str = "*";

impl Input {
    pub fn new(table: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            id: id.into(),
        }
    }

    pub fn whole_table(table: impl Into<String>) -> Self {
        Self::new(table, WHOLE_TABLE)
    }
}

/// How a composite primary key is rendered as one `input_id`, on both
/// sides: the provider declaring it and the driver reading the diff.
pub fn join_key(parts: &[String]) -> String {
    parts.join("|")
}

/// A source's render output store, open for writing.
///
/// The pool is one connection wide and a transaction holds it, so every
/// read the driver may issue inside one goes through the write lock —
/// which hands back the held connection — rather than the pool, which
/// would wait on itself.
/// What a render store holds, whole store: its documents, and the items
/// they count between them — messages, readings, events — which is the
/// number that says how much of a source has arrived.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Holdings {
    pub documents: i64,
    pub items: i64,
}

pub struct IndexedMarkdownStore {
    pool: SqlitePool,
    write_lock: WriteLock,
    path: PathBuf,
    /// The run-pinned "now" stamped onto problem rows — see
    /// [`Self::with_now`].
    now: String,
    /// The commit a reader was opened at; `None` for the owner's handle.
    pin: Option<datalib_etl::pin::Pin>,
}

/// Run a future to completion from a synchronous caller.
///
/// Without a runtime to join, the future runs on one process-wide
/// runtime rather than a fresh one per call. A per-call runtime is
/// dropped as soon as the future completes, and sqlx returns a
/// checked-out connection to its pool from a task *spawned* at drop
/// (`PoolConnection::drop`): kill the runtime first and that task never
/// runs, the pool never gets the connection back, and the next `acquire` opens
/// a second connection to the same doltlite file while the first is
/// still closing on its worker thread — two live handles on one store,
/// surfacing as `database is locked` under load.
pub fn blocking<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => fallback_runtime().block_on(fut),
    }
}

fn fallback_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build a runtime for blocking store calls")
    })
}

impl IndexedMarkdownStore {
    pub fn open(rendered_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(rendered_root)
            .with_context(|| format!("mkdir -p {}", rendered_root.display()))?;
        let path = path_for(rendered_root);
        let indexes: Vec<&str> = lookup_indexes().collect();
        let pool = blocking(datalib_etl::doltlite_raw::open_derived_indexed(
            &path,
            &store_ddl(),
            &indexes,
            datalib_etl::doltlite_raw::StoreKind::Render,
        ))
        .with_context(|| format!("open indexed markdown store {}", path.display()))?;
        Ok(Self {
            write_lock: WriteLock::new(pool.clone()),
            pool,
            path,
            now: datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs(),
            pin: None,
        })
    }

    /// Open somebody else's render store to read it.
    ///
    /// The index is not this store's owner — the render step is — so this goes
    /// through [`datalib_etl::doltlite_raw::open_reader`] and performs none of the
    /// writes [`Self::open`] does on the way in. Read that function's note for
    /// what those are and why they are a hazard here specifically.
    ///
    /// Pinned at open — at `commit`, or HEAD — so every read through this
    /// handle names one commit: `changed_since`'s diff and
    /// `documents_matching`'s rows must agree. `None` means the store has
    /// no commit to read; the caller contributes nothing rather than
    /// reading the working set.
    ///
    /// No `now`, no write lock: nothing reached through this handle may write.
    ///
    /// A store at a commit whose `_datalib_meta` records another shape
    /// than [`shape`] is refused as `datalib_store_meta::OtherShape`,
    /// before a row is read: its render step brings it up to date.
    pub fn open_for_reading(rendered_root: &Path, commit: Option<&str>) -> Result<Option<Self>> {
        let path = path_for(rendered_root);
        let Some(reader) = blocking(datalib_etl::doltlite_raw::open_reader(&path, commit))
            .with_context(|| format!("open render store for reading {}", path.display()))?
        else {
            return Ok(None);
        };
        let checked = blocking(datalib_store_meta::read(reader.pool())).and_then(|meta| {
            let step = datalib_store_meta::step_of(&path);
            datalib_store_meta::require(&path, step.as_deref(), meta.as_ref(), &shape())
        });
        if let Err(e) = checked {
            blocking(reader.close());
            return Err(e);
        }
        let pool = reader.pool().clone();
        Ok(Some(Self {
            write_lock: WriteLock::new(pool.clone()),
            pool,
            path,
            now: String::new(),
            pin: Some(reader.pin().clone()),
        }))
    }

    /// The commit this reader reads at. `None` on the owner's handle.
    pub fn pin(&self) -> Option<&datalib_etl::pin::Pin> {
        self.pin.as_ref()
    }

    /// Use the run-pinned "now" (`--now` / `$DATALIB_DAG_NOW`) for the
    /// problem timestamps this store stamps, so every row one render
    /// writes agrees. Without it the store samples its own clock at
    /// open, which is right for a test and near enough for a one-off
    /// tool, but leaves a long run's rows spread over its duration.
    pub fn with_now(mut self, now: &str) -> Self {
        if !now.is_empty() {
            self.now = now.to_string();
        }
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The renderer versions the provider's own documents carry.
    ///
    /// The storage report is excluded: datalib renders it, not any of
    /// the source's processors, so measuring its version against what
    /// they declare would fail every source — and counting it as
    /// "declared" would blunt the check for the documents it exists to
    /// guard.
    ///
    /// So is a document a run kept although it could not render it
    /// again (its problem says so): it carries the version that last
    /// rendered it, and counting that would fail every run, or walk the
    /// whole source every run, until the one render unit renders.
    pub fn render_versions(&self) -> Result<BTreeSet<u32>> {
        blocking(async {
            let rows = sql!(
                "etl.render.store.list_renderer_versions",
                "SELECT DISTINCT renderer_version FROM markdowns \
                 WHERE renderer_version IS NOT NULL AND kind <> ? \
                   AND markdown_uuid NOT IN ( \
                       SELECT scope_key FROM problems \
                        WHERE scope_kind = ? AND stage = ? AND reason IN (?, ?))"
            )
            .query()
            .bind(datalib_schema::measurements::DOC_KIND)
            .bind(ScopeKind::Markdown.as_str())
            .bind(Stage::Render.as_str())
            .bind(Reason::RenderFailed.as_str())
            .bind(Reason::NoDocument.as_str())
            .fetch_all(&self.pool)
            .await
            .context("read renderer versions")?;
            let mut out = BTreeSet::new();
            for r in rows {
                let v: String = r.try_get(0)?;
                if let Some(n) = v.rsplit('.').next().and_then(|s| s.parse::<u32>().ok()) {
                    out.insert(n);
                }
            }
            Ok(out)
        })
    }

    /// Open a batch: one SQL transaction that every write until
    /// [`Self::commit_batch`] joins. The driver holds one open across the
    /// documents between two checkpoints, so a render of 200k documents
    /// is a few hundred transactions rather than 200k.
    pub fn begin_batch(&self) -> Result<()> {
        blocking(self.write_lock.begin_transaction())
    }

    pub fn commit_batch(&self) -> Result<()> {
        blocking(self.write_lock.commit_transaction())
    }

    pub fn rollback_batch(&self) -> Result<()> {
        blocking(self.write_lock.rollback_transaction())
    }

    /// Run `f` as one SQL transaction, or as part of the batch already open.
    ///
    /// A unit of work — a document with its rows, the end-of-run sweep with
    /// the cursor — is replaced whole or not at all, and never straddles a
    /// batch boundary, because the boundary is only ever placed between two
    /// calls to this.
    pub fn transaction<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if blocking(self.write_lock.in_transaction()) {
            return f();
        }
        blocking(self.write_lock.begin_transaction())?;
        match f() {
            Ok(v) => {
                blocking(self.write_lock.commit_transaction())?;
                Ok(v)
            }
            Err(e) => {
                let _ = blocking(self.write_lock.rollback_transaction());
                Err(e)
            }
        }
    }

    /// Store `md`, replacing what the store held for it. Always: a
    /// document whose rows come out unchanged writes identical rows, and
    /// doltlite's content-addressed tables then carry no diff for it —
    /// that, and nothing here, is how "unchanged" is decided. A document
    /// with no rows is removed; its problems stay.
    pub fn put_document(&self, md: &RenderedMarkdown) -> Result<()> {
        if let Some(row) = md
            .rows
            .iter()
            .find(|r| r.is_document && r.item_count.is_none())
        {
            bail!(
                "document {}: its document row {} has no item_count; \
                 every renderer says how many things its document holds",
                md.markdown_uuid,
                row.uuid
            );
        }
        self.transaction(|| {
            blocking(async {
                crate::grid_index::apply_one(&self.write_lock, md)
                    .await
                    .with_context(|| format!("apply {}", md.markdown_uuid))?;
                put_sections(&self.write_lock, md).await?;
                self.sweep_problems(&md.markdown_uuid, &md.problems).await
            })
        })
    }

    /// Drop documents and everything the store keeps of them. However many
    /// there are, they go in a handful of statements, each binding the
    /// whole set once.
    pub fn remove_documents(&self, markdown_uuids: &[String]) -> Result<()> {
        if markdown_uuids.is_empty() {
            return Ok(());
        }
        self.transaction(|| {
            blocking(async {
                let mut guard = self.write_lock.acquire().await?;
                let conn = guard.conn();
                let set = serde_json::to_string(markdown_uuids)?;
                crate::grid_index::delete_document_rows(
                    conn,
                    crate::grid_index::Store::Render,
                    markdown_uuids,
                )
                .await
                .context("remove documents from the store")?;
                for sql in [REMOVED_SECTIONS, REMOVED_DRAWN, REMOVED_FOLDS] {
                    sql.query()
                        .bind(&set)
                        .execute(&mut **conn)
                        .await
                        .context("remove the sections of documents removed from the store")?;
                }
                REMOVED_PROBLEMS
                    .query()
                    .bind(ScopeKind::Markdown.as_str())
                    .bind(&set)
                    .execute(&mut **conn)
                    .await
                    .context("remove the problems of documents removed from the store")?;
                Ok(())
            })
        })
    }

    /// Record what `render_unit_key` was rendered from, replacing what it
    /// declared last time. Joins the open batch, so a render unit's documents
    /// and its inputs reach the store together. Says whether the render unit
    /// had any before.
    pub fn put_inputs(&self, render_unit_key: &str, inputs: &[Input]) -> Result<bool> {
        self.transaction(|| {
            blocking(async {
                let mut guard = self.write_lock.acquire().await?;
                let conn = guard.conn();
                let prior = sql!("etl.render.store.delete_inputs_of_render_unit", "DELETE FROM render_inputs WHERE render_unit_key = ?").query()
                    .bind(render_unit_key)
                    .execute(&mut **conn)
                    .await
                    .context("clear prior render_inputs")?
                    .rows_affected();
                for chunk in inputs.chunks(datalib_etl::bulk::SQL_CHUNK / 3) {
                    let mut sql = String::from(
                        "INSERT OR IGNORE INTO render_inputs (render_unit_key, input_table, input_id) VALUES ",
                    );
                    for (i, _) in chunk.iter().enumerate() {
                        if i > 0 {
                            sql.push_str(", ");
                        }
                        sql.push_str("(?, ?, ?)");
                    }
                    // Audited: a placeholder run sized from the chunk; every
                    // value is bound.
                    let mut q = Sql::dynamic(sql_id!("etl.render.store.insert_render_inputs"), sql).query();
                    for input in chunk {
                        q = q.bind(render_unit_key).bind(&input.table).bind(&input.id);
                    }
                    q.execute(&mut **conn)
                        .await
                        .with_context(|| format!("write render_inputs for {render_unit_key}"))?;
                }
                Ok(prior > 0)
            })
        })
    }

    /// What each of `render_unit_keys` was last declared to read.
    pub fn inputs_of(&self, render_unit_keys: &[&str]) -> Result<HashMap<String, Vec<Input>>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let mut out: HashMap<String, Vec<Input>> = HashMap::new();
            for chunk in render_unit_keys.chunks(datalib_etl::bulk::SQL_CHUNK) {
                let mut sql = String::from(
                    "SELECT render_unit_key, input_table, input_id FROM render_inputs WHERE render_unit_key IN (",
                );
                datalib_etl::bulk::push_placeholder_list(&mut sql, chunk.len());
                sql.push(')');
                // Audited: a placeholder run sized from the chunk; every
                // key is bound.
                let mut q =
                    Sql::dynamic(sql_id!("etl.render.store.list_inputs_of_render_units"), sql)
                        .query_as::<(String, String, String)>();
                for key in chunk {
                    q = q.bind(*key);
                }
                for (render_unit, table, id) in q
                    .fetch_all(&mut **guard.conn())
                    .await
                    .context("read the inputs of render units")?
                {
                    out.entry(render_unit)
                        .or_default()
                        .push(Input::new(table, id));
                }
            }
            Ok(out)
        })
    }

    /// Which render units declare each of `inputs`, matched exactly: a
    /// whole-table declaration does not count as reading one row.
    pub fn readers_of(&self, inputs: &[Input]) -> Result<Vec<(Input, String)>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let mut out = Vec::new();
            for chunk in inputs.chunks(datalib_etl::bulk::SQL_CHUNK / 2) {
                let mut sql = String::from(
                    "SELECT input_table, input_id, render_unit_key FROM render_inputs \
                     WHERE (input_table, input_id) IN (",
                );
                for (i, _) in chunk.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    sql.push_str("(?, ?)");
                }
                sql.push(')');
                // Audited: a placeholder run sized from the chunk; every
                // value is bound.
                let mut q = Sql::dynamic(sql_id!("etl.render.store.list_readers_of_inputs"), sql)
                    .query_as::<(String, String, String)>();
                for input in chunk {
                    q = q.bind(&input.table).bind(&input.id);
                }
                for (table, id, render_unit) in q
                    .fetch_all(&mut **guard.conn())
                    .await
                    .context("read who declares these inputs")?
                {
                    out.push((Input::new(table, id), render_unit));
                }
            }
            Ok(out)
        })
    }

    /// Every raw table any render unit declared an input from.
    pub fn input_tables(&self) -> Result<Vec<String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            sql!(
                "etl.render.store.list_input_tables",
                "SELECT DISTINCT input_table FROM render_inputs ORDER BY input_table"
            )
            .query_scalar()
            .fetch_all(&mut **guard.conn())
            .await
            .context("read render_inputs tables")
        })
    }

    /// The render units that declared any of `changed` as an input — the
    /// reverse lookup. `changed` is `(table, id)` as the diff named them;
    /// a render unit that declared a whole table matches any row of it.
    pub fn render_units_reading(&self, changed: &[Input]) -> Result<HashSet<String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let mut out = HashSet::new();
            let tables: BTreeSet<&str> = changed.iter().map(|i| i.table.as_str()).collect();
            for table in tables {
                out.extend(
                    sql!("etl.render.store.list_units_reading_whole_table", "SELECT DISTINCT render_unit_key FROM render_inputs WHERE input_table = ? AND input_id = ?").query_scalar::<String>()
                    .bind(table)
                    .bind(WHOLE_TABLE)
                    .fetch_all(&mut **guard.conn())
                    .await
                    .context("reverse lookup of whole-table inputs")?,
                );
            }
            for chunk in changed.chunks(datalib_etl::bulk::SQL_CHUNK / 2) {
                let mut sql = String::from(
                    "SELECT DISTINCT render_unit_key FROM render_inputs WHERE (input_table, input_id) IN (",
                );
                for (i, _) in chunk.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    sql.push_str("(?, ?)");
                }
                sql.push(')');
                // Audited: a placeholder run sized from the chunk; every
                // value is bound.
                let mut q = Sql::dynamic(
                    sql_id!("etl.render.store.list_units_reading_changed_rows"),
                    sql,
                )
                .query_scalar::<String>();
                for input in chunk {
                    q = q.bind(&input.table).bind(&input.id);
                }
                out.extend(
                    q.fetch_all(&mut **guard.conn())
                        .await
                        .context("reverse lookup in render_inputs")?,
                );
            }
            Ok(out)
        })
    }

    /// The render units that declared any input from `table`, a whole-table
    /// one included.
    pub fn render_units_reading_table(&self, table: &str) -> Result<HashSet<String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let render_units = sql!(
                "etl.render.store.list_units_reading_table",
                "SELECT DISTINCT render_unit_key FROM render_inputs WHERE input_table = ?"
            )
            .query_scalar::<String>()
            .bind(table)
            .fetch_all(&mut **guard.conn())
            .await
            .context("the render units that read one table")?;
            Ok(render_units.into_iter().collect())
        })
    }

    /// Up to `limit` of the row ids `render_unit_key` declared from `table`,
    /// in order, after `after`; a whole-table input is not a row.
    pub fn ids_read_from(
        &self,
        render_unit_key: &str,
        table: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            sql!(
                "etl.render.store.page_input_ids_of_unit",
                "SELECT input_id FROM render_inputs \
                  WHERE render_unit_key = ?1 AND input_table = ?2 AND input_id <> ?3 \
                    AND (?4 IS NULL OR input_id > ?4) \
                  ORDER BY input_id LIMIT ?5"
            )
            .query_scalar::<String>()
            .bind(render_unit_key)
            .bind(table)
            .bind(WHOLE_TABLE)
            .bind(after)
            .bind(limit as i64)
            .fetch_all(&mut **guard.conn())
            .await
            .context("the rows a render unit read from one table")
        })
    }

    /// Every document rendered under any of `render_unit_keys`, as
    /// `(render unit, document)`. One query rather than one per render unit — a run
    /// declares as many render units as it rendered.
    pub fn documents_for_render_units(
        &self,
        render_unit_keys: &[&str],
    ) -> Result<Vec<(String, String)>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            DOCUMENTS_FOR_RENDER_UNITS
                .query_as::<(String, String)>()
                .bind(serde_json::to_string(render_unit_keys)?)
                .fetch_all(&mut **guard.conn())
                .await
                .context("documents for render units")
        })
    }

    /// Which of `uuids` this store holds a `grid_rows` row for.
    pub fn grid_rows_among(&self, uuids: &[String]) -> Result<HashSet<String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let mut out = HashSet::new();
            for chunk in uuids.chunks(datalib_etl::bulk::SQL_CHUNK) {
                let mut sql = String::from("SELECT uuid FROM grid_rows WHERE uuid IN (");
                datalib_etl::bulk::push_placeholder_list(&mut sql, chunk.len());
                sql.push(')');
                // Audited: a placeholder run sized from the chunk; every
                // uuid is bound.
                let mut q = Sql::dynamic(sql_id!("etl.render.store.find_grid_rows_among"), sql)
                    .query_scalar::<String>();
                for uuid in chunk {
                    q = q.bind(uuid);
                }
                out.extend(
                    q.fetch_all(&mut **guard.conn())
                        .await
                        .context("look up grid rows by uuid")?,
                );
            }
            Ok(out)
        })
    }

    /// The grid row minted from each `(upstream_entity_kind, upstream_id)`
    /// key, for the keys the store holds a row for.
    pub fn grid_rows_by_upstream(
        &self,
        keys: &[(&str, String)],
    ) -> Result<HashMap<(String, String), String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let mut out = HashMap::new();
            for (kind, id) in keys {
                let uuid: Option<String> = GRID_ROW_BY_UPSTREAM
                    .query_scalar()
                    .bind(kind)
                    .bind(id)
                    .fetch_optional(&mut **guard.conn())
                    .await
                    .context("look up a grid row by its upstream key")?;
                if let Some(uuid) = uuid {
                    out.insert((kind.to_string(), id.clone()), uuid);
                }
            }
            Ok(out)
        })
    }

    /// The newest `items` sample per subject in `source_measurements`:
    /// what the storage report compares its counts against to decide
    /// whether anything moved.
    pub fn latest_items(&self) -> Result<HashMap<String, Option<i64>>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let rows = sql!(
                "etl.render.store.read_latest_item_counts",
                "SELECT subject, items FROM source_measurements m \
                  WHERE measured_at_utc = (SELECT MAX(measured_at_utc) FROM source_measurements \
                                            WHERE subject = m.subject)"
            )
            .query()
            .fetch_all(&mut **guard.conn())
            .await
            .context("read latest measurements")?;
            let mut out = HashMap::with_capacity(rows.len());
            for r in rows {
                out.insert(r.try_get::<String, _>(0)?, r.try_get::<Option<i64>, _>(1)?);
            }
            Ok(out)
        })
    }

    /// The renderer version a stored document carries, if it is there.
    pub fn document_version(&self, markdown_uuid: &str) -> Result<Option<u32>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let v: Option<Option<String>> = sql!(
                "etl.render.store.read_document_version",
                "SELECT renderer_version FROM markdowns WHERE markdown_uuid = ?"
            )
            .query_scalar()
            .bind(markdown_uuid)
            .fetch_optional(&mut **guard.conn())
            .await
            .context("read a document's version")?;
            Ok(v.flatten()
                .and_then(|v| v.rsplit('.').next().and_then(|s| s.parse::<u32>().ok())))
        })
    }

    /// Where the last render left off, if it recorded it.
    pub fn cursor(&self) -> Result<Option<RenderCursorRow>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            sql!(
                "etl.render.store.read_render_cursor",
                "SELECT * FROM render_cursor LIMIT 1"
            )
            .query_as::<RenderCursorRow>()
            .fetch_optional(&mut **guard.conn())
            .await
            .context("read the render cursor")
        })
    }

    /// Record where this render left off. Call it inside the transaction
    /// that holds the run's last work, so the cursor and the documents it
    /// describes reach the store together.
    pub fn write_cursor(&self, row: &RenderCursorRow) -> Result<()> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            sql!(
                "etl.render.store.delete_render_cursor",
                "DELETE FROM render_cursor"
            )
            .query()
            .execute(&mut **conn)
            .await
            .context("clear the prior render cursor")?;
            let sql = Sql::dynamic(
                sql_id!("etl.render.store.insert_render_cursor"),
                datalib_etl::bulk::insert_sql::<RenderCursorRow>(),
            );
            // Audited: `sql` is built from `RenderCursorRow`'s associated
            // consts; every value is bound.
            row.bind_into(sql.query())
                .execute(&mut **conn)
                .await
                .context("write the render cursor")?;
            Ok(())
        })
    }

    /// How many documents this store holds and how many items they
    /// count between them, `except` one — the storage report, which
    /// datalib writes about the source rather than out of it, and which
    /// would otherwise make a source that renders nothing read as
    /// holding one thing.
    ///
    /// Whole store, not this run: the number is what the source has,
    /// and an incremental render touches a handful of documents out of
    /// a hundred thousand.
    pub fn holdings(&self, except: Option<&str>) -> Result<Holdings> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let (documents, items): (i64, i64) = sql!(
                "etl.render.store.count_documents_and_items",
                "SELECT COUNT(*), COALESCE(SUM(item_count), 0) FROM markdowns \
                 WHERE ? IS NULL OR markdown_uuid <> ?"
            )
            .query_as()
            .bind(except)
            .bind(except)
            .fetch_one(&mut **guard.conn())
            .await
            .context("count the store's documents and items")?;
            Ok(Holdings { documents, items })
        })
    }

    /// Every `markdowns` row, ordered by id: what the search index reads to
    /// learn which documents it still has to index and where their
    /// sections lie.
    pub fn markdown_rows(&self) -> Result<Vec<datalib_schema::markdowns::MarkdownRow>> {
        blocking(async {
            sql!(
                "etl.render.store.list_markdowns_rows",
                "SELECT * FROM markdowns ORDER BY markdown_uuid"
            )
            .query_as()
            .fetch_all(&self.pool)
            .await
            .context("read markdowns")
        })
    }

    /// The `markdowns` rows of these documents, ordered by id.
    pub fn markdown_rows_in(
        &self,
        markdown_uuids: &[String],
    ) -> Result<Vec<datalib_schema::markdowns::MarkdownRow>> {
        blocking(async {
            sql!(
                "etl.render.store.list_markdowns_rows_in",
                "SELECT * FROM markdowns \
                 WHERE markdown_uuid IN (SELECT value FROM json_each(?)) \
                 ORDER BY markdown_uuid"
            )
            .query_as()
            .bind(serde_json::to_string(markdown_uuids)?)
            .fetch_all(&self.pool)
            .await
            .context("read markdowns")
        })
    }

    /// What the store keeps of these documents' sections, in order, each
    /// with the author and time of the row it is: what the search index
    /// draws a document's text from.
    pub fn kept_sections(&self, markdown_uuids: &[String]) -> Result<Vec<KeptSection>> {
        blocking(async {
            sql!(
                "etl.render.store.read_kept_sections",
                "SELECT s.markdown_uuid, s.seq, s.uuid, s.text, s.framed, s.addressed, \
                        s.labels, g.author, g.author_handle, g.created_at \
                 FROM sections s LEFT JOIN grid_rows g ON g.uuid = s.uuid \
                 WHERE s.markdown_uuid IN (SELECT value FROM json_each(?)) \
                 ORDER BY s.markdown_uuid, s.seq"
            )
            .query_as()
            .bind(serde_json::to_string(markdown_uuids)?)
            .fetch_all(&self.pool)
            .await
            .context("read kept sections")
        })
    }

    /// A document spelled back from what the store keeps of it, section by
    /// section in order: joined, exactly the document render drew. `None`
    /// for a document the store does not hold.
    pub fn document(
        &self,
        markdown_uuid: &str,
    ) -> Result<Option<Vec<crate::section::SpelledSection>>> {
        blocking(async {
            let (sections, drawn, folds) =
                pieces_between(&self.pool, markdown_uuid, 0, i64::MAX).await?;
            if sections.is_empty() && drawn.is_empty() {
                return Ok(None);
            }
            Ok(Some(crate::section::spell(&sections, &drawn, &folds)))
        })
    }

    /// A page of a document, in its own order: at most `limit` of its
    /// top-level nodes where `ask` says ([`crate::document_window`]), and
    /// the cursors either side of them. Only the page's pieces are read and
    /// spelled. `None` for a document the store does not hold; a page past
    /// either end is empty.
    pub fn document_page(
        &self,
        markdown_uuid: &str,
        ask: PageAsk,
        limit: usize,
    ) -> Result<Option<DocumentPage>> {
        use crate::document_window::{spans, window, Ask};
        blocking(async {
            let len: Option<i64> = sql!(
                "etl.render.store.count_document_pieces",
                "SELECT MAX(m) + 1 FROM ( \
                   SELECT MAX(seq) AS m FROM sections WHERE markdown_uuid = ? \
                   UNION ALL SELECT MAX(seq) FROM drawings WHERE markdown_uuid = ?)"
            )
            .query_scalar()
            .bind(markdown_uuid)
            .bind(markdown_uuid)
            .fetch_one(&self.pool)
            .await
            .context("read how many pieces a document has")?;
            let Some(len) = len else {
                return Ok(None);
            };
            let folds: Vec<datalib_schema::folds::FoldRow> = sql!(
                "etl.render.store.read_document_folds",
                "SELECT markdown_uuid, seq, class, summary FROM folds \
                 WHERE markdown_uuid = ? ORDER BY seq"
            )
            .query_as()
            .bind(markdown_uuid)
            .fetch_all(&self.pool)
            .await
            .context("read a document's folds")?;
            let ask = match ask {
                PageAsk::Start => Ask::Start,
                PageAsk::After(cursor) => Ask::After(cursor),
                PageAsk::Before(cursor) => Ask::Before(cursor),
                PageAsk::At(uuid) => {
                    let seq: Option<i64> = sql!(
                        "etl.render.store.find_piece_seq_by_uuid",
                        "SELECT seq FROM sections WHERE markdown_uuid = ? AND uuid = ?"
                    )
                    .query_scalar()
                    .bind(markdown_uuid)
                    .bind(&uuid)
                    .fetch_optional(&self.pool)
                    .await
                    .context("find a piece of a document")?;
                    seq.map_or(Ask::Start, Ask::Around)
                }
            };
            let Some(w) = window(&spans(len, &folds), ask, limit) else {
                return Ok(Some(DocumentPage::default()));
            };
            let (sections, drawn, folds) =
                pieces_between(&self.pool, markdown_uuid, w.first, w.last).await?;
            Ok(Some(DocumentPage {
                sections: crate::section::spell(&sections, &drawn, &folds),
                before: w.before,
                after: w.after,
                from_top: w.first == 0,
            }))
        })
    }

    /// What each of these items says, as the store keeps it, by its uuid:
    /// its body where its renderer split one off, else its piece whole
    /// with the class of the box it is in. Read only for these documents'
    /// pieces; an item the store does not hold is left out.
    pub fn item_texts(
        &self,
        markdown_uuids: &[String],
        uuids: &[String],
    ) -> Result<Vec<(String, String, Option<String>)>> {
        blocking(async {
            sql!(
                "etl.render.store.read_item_texts",
                "SELECT uuid, text, class FROM sections \
                 WHERE markdown_uuid IN (SELECT value FROM json_each(?)) \
                 AND uuid IN (SELECT value FROM json_each(?))"
            )
            .query_as()
            .bind(serde_json::to_string(markdown_uuids)?)
            .bind(serde_json::to_string(uuids)?)
            .fetch_all(&self.pool)
            .await
            .context("read items' texts")
        })
    }

    /// [`Self::document`] as one string: the document's markdown.
    pub fn document_text(&self, markdown_uuid: &str) -> Result<Option<String>> {
        Ok(self
            .document(markdown_uuid)?
            .map(|sections| sections.into_iter().map(|s| s.md).collect()))
    }

    /// Every document the store can spell, by id, as its markdown.
    pub fn document_texts(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let mut texts = std::collections::BTreeMap::new();
        for row in self.markdown_rows()? {
            if let Some(text) = self.document_text(&row.markdown_uuid)? {
                texts.insert(row.markdown_uuid, text);
            }
        }
        Ok(texts)
    }

    /// These documents as the search index takes them, drawn from what
    /// the store keeps of them (`datalib_search_index::text`), all read at
    /// the one commit this reader is at, so a document's key always names
    /// the sections it was drawn from. A document with no [`index_key`] is
    /// left out.
    pub fn index_docs(
        &self,
        source_id: &str,
        markdown_uuids: &[String],
    ) -> Result<Vec<datalib_search_index::IndexDoc>> {
        let rows = self.markdown_rows_in(markdown_uuids)?;
        let mut kept: HashMap<String, Vec<KeptSection>> = HashMap::new();
        for k in self.kept_sections(markdown_uuids)? {
            kept.entry(k.markdown_uuid.clone()).or_default().push(k);
        }
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let content_hash = index_key(&row)?;
                let pieces: Vec<_> = kept
                    .remove(&row.markdown_uuid)
                    .unwrap_or_default()
                    .into_iter()
                    .map(KeptSection::piece)
                    .collect();
                let facts = index_facts(&row);
                Some(datalib_search_index::IndexDoc {
                    source_id: source_id.to_string(),
                    markdown_uuid: row.markdown_uuid,
                    content_hash,
                    title: row.title.unwrap_or_default(),
                    head: datalib_search_index::text::head(&pieces, &facts),
                    sections: datalib_search_index::text::draw(&pieces, &facts),
                })
            })
            .collect())
    }

    /// Every document this store holds. The other half of a sweep: a
    /// renderer that walked its whole raw store says what should be here,
    /// and whatever else is here is what the store lost.
    pub fn all_document_uuids(&self) -> Result<Vec<String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let rows = sql!(
                "etl.render.store.list_document_uuids",
                "SELECT markdown_uuid FROM markdowns"
            )
            .query()
            .fetch_all(&mut **guard.conn())
            .await
            .context("list every document in the store")?;
            rows.into_iter()
                .map(|r| r.try_get::<String, _>(0).map_err(Into::into))
                .collect()
        })
    }
}

/// A document's front matter as the search index draws its facts line
/// from it. A `front` this build cannot read is no facts, as from a store
/// an older build wrote.
fn index_facts(row: &datalib_schema::markdowns::MarkdownRow) -> datalib_search_index::text::Facts {
    use crate::front_matter::{Front, FrontValue};
    let front = row
        .front
        .as_deref()
        .and_then(Front::from_json)
        .unwrap_or_default();
    datalib_search_index::text::Facts {
        source_name: datalib_schema::providers::Provider::parse(&row.provider)
            .map_or_else(|| row.provider.clone(), |p| p.label().to_string()),
        front: front
            .facts()
            .iter()
            .filter_map(|f| match &f.value {
                FrontValue::Plain(v) | FrontValue::Text(v) | FrontValue::Name(v) => {
                    Some((f.key.clone(), v.clone()))
                }
                FrontValue::Null => None,
            })
            .collect(),
    }
}

// `wanted` is a JSON array of document ids, bound as `?1`.
/// The people each of the documents in the JSON array `wanted` carries,
/// by document.
async fn contacts_of(
    pool: &SqlitePool,
    wanted: &str,
) -> Result<HashMap<String, Vec<NormalizedContact>>> {
    let rows = group_by_document::<datalib_schema::source_contacts::SourceContactRow>(
        pool,
        sql!(
            "etl.render.store.read_back_source_contacts",
            "SELECT * FROM source_contacts \
              WHERE markdown_uuid IN (SELECT value FROM json_each(?1)) \
              ORDER BY markdown_uuid, contact_key"
        ),
        "markdown_uuid",
        wanted,
    )
    .await
    .context("read source contacts")?;
    rows.into_iter()
        .map(|(md, rows)| {
            let contacts = rows
                .into_iter()
                .map(|r| {
                    serde_json::from_str(&r.contact_json).with_context(|| {
                        format!("source contact {} of {}", r.contact_key, r.markdown_uuid)
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((md, contacts))
        })
        .collect()
}

async fn group_by_document<T>(
    pool: &SqlitePool,
    sql: Sql,
    key: &str,
    wanted: &str,
) -> Result<HashMap<String, Vec<T>>, sqlx::Error>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>,
{
    use futures::TryStreamExt;
    let mut by_doc: HashMap<String, Vec<T>> = HashMap::new();
    let mut rows = sql.query().bind(wanted).fetch(pool);
    while let Some(row) = rows.try_next().await? {
        let doc: String = row.try_get(key)?;
        by_doc.entry(doc).or_default().push(T::from_row(&row)?);
    }
    Ok(by_doc)
}

impl IndexedMarkdownStore {
    async fn sweep_problems(&self, markdown_uuid: &str, problems: &[ProblemRow]) -> Result<()> {
        let mut guard = self.write_lock.acquire().await?;
        let conn = guard.conn();
        // Read the rows about to be rewritten, *before* the delete. This
        // is the whole reason the store stamps these rather than the
        // renderer: a renderer that set both timestamps to "now" every
        // run would make `first_seen_at_utc` a synonym for
        // `changed_at_utc`, and "this has been broken since Tuesday"
        // would be unanswerable.
        let mut seen: HashMap<String, ProblemRow> = HashMap::new();
        for r in sql!(
            "etl.render.store.read_problems_of_document",
            "SELECT * FROM problems WHERE scope_kind = ? AND scope_key = ?"
        )
        .query()
        .bind(ScopeKind::Markdown.as_str())
        .bind(markdown_uuid)
        .fetch_all(&mut **conn)
        .await
        .context("read the document's problems")?
        {
            let row = ProblemRow::from_row(&r)?;
            seen.insert(row.problem_uuid.clone(), row);
        }
        sql!(
            "etl.render.store.delete_problems_of_document",
            "DELETE FROM problems WHERE scope_kind = ? AND scope_key = ?"
        )
        .query()
        .bind(ScopeKind::Markdown.as_str())
        .bind(markdown_uuid)
        .execute(&mut **conn)
        .await
        .context("clear prior problems for this document")?;
        self.insert_problems(conn, problems, &seen).await
    }

    /// Insert problem rows, each stamped against the row `seen` holds
    /// under its uuid (`ProblemRow::stamped`).
    async fn insert_problems(
        &self,
        conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
        problems: &[ProblemRow],
        seen: &HashMap<String, ProblemRow>,
    ) -> Result<()> {
        let now = datalib_time::split_stamp(&self.now);
        for p in problems {
            let stamped = p.clone().stamped(
                seen.get(&p.problem_uuid),
                &now.utc,
                now.tz_offset.as_deref(),
            );
            // Same generated write path the rows use; see
            // `PortableTable`'s `BulkUpsertable` impl.
            let sql = Sql::dynamic(
                sql_id!("etl.render.store.insert_problem"),
                datalib_etl::bulk::insert_sql::<ProblemRow>(),
            );
            // Audited: `sql` is built from `ProblemRow`'s
            // associated consts, never from row data; all values bound.
            stamped
                .bind_into(sql.query())
                .execute(&mut **conn)
                .await
                .with_context(|| format!("insert problem {}", p.problem_uuid))?;
            if stamped.is_news(seen.get(&p.problem_uuid)) {
                datalib_schema::problems::note_recorded([&stamped]);
            }
        }
        Ok(())
    }

    /// Append one run's measurements to the source's series.
    ///
    /// Append, not upsert: this table is the history behind the
    /// sparkline, and the current value already lives in `grid_rows`.
    ///
    /// `INSERT OR REPLACE`, because the key is `(subject, measured_at_utc)`
    /// and a run stamps one pinned instant across every row it writes —
    /// so a re-run under the same `--now` should restate the series
    /// rather than fail the whole render on a duplicate carrying the
    /// same numbers.
    ///
    /// Hand-written rather than through `bulk::insert_sql`: the
    /// `PortableTable` derive emits no write path for a composite
    /// primary key, since `BulkUpsertable` assumes one `id` column.
    pub fn put_measurements(&self, samples: &[SourceMeasurementRow]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            for sample in samples {
                sql!(
                    "etl.render.store.upsert_measurement",
                    "INSERT OR REPLACE INTO source_measurements \
                     (subject, kind, measured_at_utc, tz_offset, bytes, items) \
                     VALUES (?, ?, ?, ?, ?, ?)"
                )
                .query()
                .bind(&sample.subject)
                .bind(&sample.kind)
                .bind(&sample.measured_at_utc)
                .bind(&sample.tz_offset)
                .bind(sample.bytes)
                .bind(sample.items)
                .execute(&mut **conn)
                .await
                .with_context(|| {
                    format!(
                        "insert measurement {} at {}",
                        sample.subject, sample.measured_at_utc
                    )
                })?;
            }
            Ok(())
        })
    }

    /// Make [`ENTITY_ITEMS_DDL`]'s table hold `items` and nothing else.
    pub fn replace_entity_items(&self, items: &[(String, String)]) -> Result<()> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            sql!(
                "etl.render.store.clear_entity_items",
                "DELETE FROM entity_items"
            )
            .query()
            .execute(&mut **conn)
            .await
            .context("clear the entity items")?;
            for (entity_key, item_uuid) in items {
                sql!(
                    "etl.render.store.insert_entity_item",
                    "INSERT INTO entity_items (entity_key, item_uuid) VALUES (?, ?)"
                )
                .query()
                .bind(entity_key)
                .bind(item_uuid)
                .execute(&mut **conn)
                .await
                .with_context(|| format!("record {entity_key}'s item"))?;
            }
            Ok(())
        })
    }

    /// Replace every row of one stage with `rows`, stamps carried as
    /// they are.
    pub fn replace_stage_problems(&self, stage: Stage, rows: &[ProblemRow]) -> Result<()> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            sql!(
                "etl.render.store.delete_problems_of_stage",
                "DELETE FROM problems WHERE stage = ?"
            )
            .query()
            .bind(stage.as_str())
            .execute(&mut **conn)
            .await
            .with_context(|| format!("clear {} problems", stage.as_str()))?;
            let sql = Sql::dynamic(
                sql_id!("etl.render.store.insert_stage_problem"),
                datalib_etl::bulk::insert_sql::<ProblemRow>(),
            );
            for row in rows {
                // Audited: `sql` is built from `ProblemRow`'s associated
                // consts, never from row data; all values bound.
                row.bind_into(sql.query())
                    .execute(&mut **conn)
                    .await
                    .with_context(|| format!("insert problem {}", row.problem_uuid))?;
            }
            Ok(())
        })
    }

    /// Write each of `rows` over the row of the same id, if there is
    /// one, and leave every other row alone: what the driver records on
    /// a document it kept, beside the problems its last render left.
    pub fn put_problems(&self, rows: &[ProblemRow]) -> Result<()> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            let mut seen: HashMap<String, ProblemRow> = HashMap::new();
            for row in rows {
                if let Some(r) = sql!(
                    "etl.render.store.find_problem_by_uuid",
                    "SELECT * FROM problems WHERE problem_uuid = ?"
                )
                .query()
                .bind(&row.problem_uuid)
                .fetch_optional(&mut **conn)
                .await
                .context("read the problem about to be rewritten")?
                {
                    seen.insert(row.problem_uuid.clone(), ProblemRow::from_row(&r)?);
                }
                sql!(
                    "etl.render.store.delete_problem_by_uuid",
                    "DELETE FROM problems WHERE problem_uuid = ?"
                )
                .query()
                .bind(&row.problem_uuid)
                .execute(&mut **conn)
                .await
                .context("clear the problem about to be rewritten")?;
            }
            self.insert_problems(conn, rows, &seen).await
        })
    }

    /// A document's problems without the document: what a run records
    /// for one it tried to produce and could not. Same sweep as
    /// [`put_document`](Self::put_document)'s, so producing it next
    /// time clears these.
    pub fn put_document_problems(&self, markdown_uuid: &str, rows: &[ProblemRow]) -> Result<()> {
        blocking(self.sweep_problems(markdown_uuid, rows))
    }

    /// Problems not attached to any document — a payload that would not
    /// deserialize has no `markdown_uuid` to be filed under. Keyed by the
    /// raw-store entity id, `<table>:<id>`. `whole_tables` names the
    /// tables the caller read every row of: their entity-scoped
    /// problems are replaced by `problems`, so an entity that reads
    /// cleanly now is cleared. An entity in `problems` from any other
    /// table is replaced by name and its neighbours are left alone.
    pub fn put_entity_problems(
        &self,
        whole_tables: &[&str],
        problems: &[ProblemRow],
    ) -> Result<()> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            let mut seen: HashMap<String, ProblemRow> = HashMap::new();
            // `INSTR(x, ?) = 1` rather than `LIKE 'table:%'`: a table
            // name may hold `_`, which LIKE reads as a wildcard.
            let prefixes: Vec<String> = whole_tables.iter().map(|t| format!("{t}:")).collect();
            let mut entities: Vec<&str> = problems.iter().map(|p| p.scope_key.as_str()).collect();
            entities.sort_unstable();
            entities.dedup();
            let clears: Vec<(&'static str, &str)> = prefixes
                .iter()
                .map(|p| {
                    (
                        "SELECT * FROM problems \
                     WHERE scope_kind = ? AND INSTR(scope_key, ?) = 1",
                        p.as_str(),
                    )
                })
                .chain(entities.iter().map(|e| {
                    (
                        "SELECT * FROM problems \
                     WHERE scope_kind = ? AND scope_key = ?",
                        *e,
                    )
                }))
                .collect();
            for (select, key) in clears {
                let delete = Sql::dynamic(
                    sql_id!("etl.render.store.delete_entity_problems"),
                    if select.contains("INSTR") {
                        "DELETE FROM problems WHERE scope_kind = ? AND INSTR(scope_key, ?) = 1"
                    } else {
                        "DELETE FROM problems WHERE scope_kind = ? AND scope_key = ?"
                    },
                );
                for r in Sql::dynamic(sql_id!("etl.render.store.read_entity_problems"), select)
                    .query()
                    .bind(ScopeKind::Entity.as_str())
                    .bind(key)
                    .fetch_all(&mut **conn)
                    .await
                    .context("read the entity problems about to be rewritten")?
                {
                    let row = ProblemRow::from_row(&r)?;
                    seen.insert(row.problem_uuid.clone(), row);
                }
                delete
                    .query()
                    .bind(ScopeKind::Entity.as_str())
                    .bind(key)
                    .execute(&mut **conn)
                    .await
                    .context("clear prior entity problems")?;
            }
            self.insert_problems(conn, problems, &seen).await
        })
    }

    /// Every commit in this store as `(hash, message)`, newest first.
    /// Empty without doltlite.
    pub fn log(&self) -> Result<Vec<(String, String)>> {
        blocking(async {
            if !datalib_etl::doltlite_raw::has_dolt_extensions(&self.pool).await {
                return Ok(Vec::new());
            }
            let rows = sql!(
                "etl.render.store.read_dolt_log",
                "SELECT commit_hash, message FROM dolt_log()"
            )
            .query()
            .fetch_all(&self.pool)
            .await
            .context("read dolt_log")?;
            rows.into_iter()
                .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
                .collect()
        })
    }

    pub fn documents(&self, pin: &datalib_etl::pin::Pin) -> Result<Vec<RenderedMarkdown>> {
        self.documents_matching(None, pin)
    }

    pub fn changed_since(
        &self,
        cursor: Option<&str>,
        pin: &datalib_etl::pin::Pin,
    ) -> Result<datalib_etl::doltlite_raw::DiffScan> {
        blocking(datalib_etl::doltlite_raw::scan_render_units(
            &self.pool,
            cursor,
            pin,
            &datalib_etl::doltlite_raw::DiffScanSpec {
                // Nothing in a render store fans out to "re-index
                // everything": every row already names the document it
                // belongs to. The providers need this for tables like
                // `users` / `channels`, whose rename shows up inside
                // every rendered doc; by the time rows reach here that
                // fan-out has already happened, on the render side.
                fanout_query: None,
                render_unit_query: "
                    SELECT DISTINCT markdown_uuid FROM (
                        SELECT coalesce(to_markdown_uuid, from_markdown_uuid) AS markdown_uuid
                          FROM dolt_diff_markdowns
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                        UNION
                        SELECT coalesce(to_markdown_uuid, from_markdown_uuid) AS markdown_uuid
                          FROM dolt_diff_grid_rows
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                        UNION
                        SELECT coalesce(to_src_markdown_uuid, from_src_markdown_uuid)
                                 AS markdown_uuid
                          FROM dolt_diff_edges
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                        UNION
                        SELECT coalesce(to_markdown_uuid, from_markdown_uuid) AS markdown_uuid
                          FROM dolt_diff_source_contacts
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                        UNION
                        SELECT coalesce(to_markdown_uuid, from_markdown_uuid) AS markdown_uuid
                          FROM dolt_diff_data_nodes
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                        UNION
                        SELECT coalesce(to_markdown_uuid, from_markdown_uuid) AS markdown_uuid
                          FROM dolt_diff_filings
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    )
                    WHERE markdown_uuid IS NOT NULL
                ",
            },
        ))
    }

    /// [`documents`](Self::documents), restricted to `only` when it is
    /// `Some`. An id in `only` with no document behind it is simply
    /// absent from the result — that is how a *deletion* reaches the
    /// caller, which compares what it asked for against what it got.
    ///
    /// Reads at `pin`, which must be the commit the caller's
    /// [`changed_since`](Self::changed_since) scanned to — otherwise the
    /// changed set and the rows behind it describe different commits. This
    /// is the read side of the store; the write side above deliberately does
    /// not go through the pinned views, since a render step writing its own
    /// store has nothing to protect itself from.
    pub fn documents_matching(
        &self,
        only: Option<&HashSet<String>>,
        pin: &datalib_etl::pin::Pin,
    ) -> Result<Vec<RenderedMarkdown>> {
        let _ = pin; // the reader was opened at it, in `open_for_reading`
        blocking(async {
            // Only the documents asked for: an unchanged source asks for
            // none, and must not cost a read of all of them.
            let mds: Vec<datalib_schema::markdowns::MarkdownRow> = match only {
                Some(keep) if keep.is_empty() => return Ok(Vec::new()),
                Some(keep) => sql!(
                    "etl.render.store.read_back_markdowns_in_set",
                    "SELECT * FROM markdowns \
                      WHERE markdown_uuid IN (SELECT value FROM json_each(?1)) \
                      ORDER BY markdown_uuid"
                )
                .query_as()
                .bind(serde_json::to_string(keep)?),
                None => sql!(
                    "etl.render.store.read_back_all_markdowns",
                    "SELECT * FROM markdowns ORDER BY markdown_uuid"
                )
                .query_as(),
            }
            .fetch_all(&self.pool)
            .await
            .context("read markdowns")?;
            if mds.is_empty() {
                return Ok(Vec::new());
            }
            // One read per table rather than a lookup per document.
            let wanted = serde_json::to_string(
                &mds.iter()
                    .map(|m| m.markdown_uuid.as_str())
                    .collect::<Vec<_>>(),
            )?;
            let mut rows_by_doc = group_by_document::<datalib_schema::grid_rows::GridRow>(
                &self.pool,
                sql!(
                    "etl.render.store.read_back_grid_rows",
                    "SELECT * FROM grid_rows \
                  WHERE markdown_uuid IN (SELECT value FROM json_each(?1)) \
                  ORDER BY markdown_uuid, uuid"
                ),
                "markdown_uuid",
                &wanted,
            )
            .await
            .context("read grid rows")?;
            let mut edges_by_doc = group_by_document::<datalib_schema::edges::EdgeRow>(
                &self.pool,
                sql!(
                    "etl.render.store.read_back_edges",
                    "SELECT * FROM edges \
                  WHERE src_markdown_uuid IN (SELECT value FROM json_each(?1)) \
                  ORDER BY src_markdown_uuid, edge_uuid"
                ),
                "src_markdown_uuid",
                &wanted,
            )
            .await
            .context("read edges")?;
            let mut nodes_by_doc = group_by_document::<datalib_schema::data_nodes::DataNodeRow>(
                &self.pool,
                sql!(
                    "etl.render.store.read_back_data_nodes",
                    "SELECT * FROM data_nodes \
                  WHERE markdown_uuid IN (SELECT value FROM json_each(?1)) \
                  ORDER BY markdown_uuid, uuid"
                ),
                "markdown_uuid",
                &wanted,
            )
            .await
            .context("read containers")?;
            let mut filings_by_doc = group_by_document::<datalib_schema::filings::FilingRow>(
                &self.pool,
                sql!(
                    "etl.render.store.read_back_filings",
                    "SELECT * FROM filings \
                  WHERE markdown_uuid IN (SELECT value FROM json_each(?1)) \
                  ORDER BY markdown_uuid, node_uuid, under_uuid"
                ),
                "markdown_uuid",
                &wanted,
            )
            .await
            .context("read filings")?;
            let mut contacts_by_doc = contacts_of(&self.pool, &wanted).await?;
            let mut photos_by_doc =
                group_by_document::<datalib_schema::source_photos::SourcePhotoRow>(
                    &self.pool,
                    sql!(
                        "etl.render.store.read_back_source_photos",
                        "SELECT * FROM source_photos \
                      WHERE markdown_uuid IN (SELECT value FROM json_each(?1)) \
                      ORDER BY markdown_uuid, photo_uuid"
                    ),
                    "markdown_uuid",
                    &wanted,
                )
                .await
                .context("read source photos")?;
            let mut out = Vec::with_capacity(mds.len());
            for md in mds {
                let rows = rows_by_doc.remove(&md.markdown_uuid).unwrap_or_default();
                let edges = edges_by_doc.remove(&md.markdown_uuid).unwrap_or_default();
                let data_nodes = nodes_by_doc.remove(&md.markdown_uuid).unwrap_or_default();
                let filings = filings_by_doc.remove(&md.markdown_uuid).unwrap_or_default();
                let contacts = contacts_by_doc
                    .remove(&md.markdown_uuid)
                    .unwrap_or_default();
                let photos = photos_by_doc
                    .remove(&md.markdown_uuid)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| {
                        serde_json::from_str(&r.photo_json).with_context(|| {
                            format!("source photo {} of {}", r.photo_uuid, r.markdown_uuid)
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                // `renderer_version` is `"<index>.<render>"`; the render
                // half is what the renderer declared.
                let render_version = md
                    .renderer_version
                    .as_deref()
                    .and_then(|v| v.rsplit('.').next())
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                out.push(RenderedMarkdown {
                    markdown_uuid: md.markdown_uuid.clone(),
                    source_id: md.source_id.clone(),
                    upstream_cursor: md.upstream_cursor.clone(),
                    render_unit_key: md.render_unit_key.clone(),
                    render_version,
                    rows,
                    sections: Vec::new(),
                    searchable: md.searchable,
                    content_hash: md.content_hash.clone(),
                    front: md
                        .front
                        .as_deref()
                        .and_then(crate::front_matter::Front::from_json)
                        .unwrap_or_default(),
                    edges,
                    contacts,
                    data_nodes,
                    filings,
                    photos,
                    problems: Vec::new(),
                });
            }
            Ok(out)
        })
    }

    /// The people each of `documents` carries, by document, and nothing
    /// else of them: what the grid index reads to fill its contacts alone.
    pub fn contacts_matching(
        &self,
        documents: &[String],
    ) -> Result<HashMap<String, Vec<NormalizedContact>>> {
        blocking(contacts_of(&self.pool, &serde_json::to_string(documents)?))
    }

    /// Every problem the store holds at the reader's pin — the whole
    /// table, because a consumer copies it wholesale: the pinned store
    /// is the complete truth about this source's problems at that
    /// commit, so the copy is the sweep.
    pub fn problems_at_pin(&self) -> Result<Vec<ProblemRow>> {
        assert!(self.pin.is_some(), "problems_at_pin is a reader's call");
        blocking(async {
            let rows = sql!(
                "etl.render.store.list_problems",
                "SELECT * FROM problems ORDER BY problem_uuid"
            )
            .query()
            .fetch_all(&self.pool)
            .await
            .context("read problems")?;
            rows.iter().map(ProblemRow::from_row).collect()
        })
    }

    /// Counts by severity of the problems render itself found, over the
    /// whole store: what the step reports at its end. The fetch-stage
    /// rows are the download's, copied here so the index can link them
    /// to a document; the download's own row counts them. A severity
    /// this build cannot name is an error — the store was written by a
    /// newer build and a silent zero would read as clean.
    pub fn own_problem_counts(&self) -> Result<HashMap<Severity, i64>> {
        blocking(async {
            let rows = sql!(
                "etl.render.store.count_problems_by_severity",
                "SELECT severity, COUNT(*) FROM problems WHERE stage <> ? GROUP BY severity"
            )
            .query()
            .bind(Stage::Fetch.as_str())
            .fetch_all(&self.pool)
            .await
            .context("count problems")?;
            let mut out = HashMap::new();
            for r in rows {
                let word: String = r.try_get(0)?;
                let severity = Severity::parse(&word)
                    .with_context(|| format!("problems.severity: unknown spelling {word:?}"))?;
                out.insert(severity, r.try_get::<i64, _>(1)?);
            }
            Ok(out)
        })
    }

    /// One `dolt_commit` for the whole render, not one per document.
    pub fn commit(&self, summary: &str) -> Result<Option<String>> {
        blocking(datalib_etl::doltlite_raw::commit_run(&self.pool, summary))
    }

    /// The store's content version, its HEAD's tree. `None` without
    /// doltlite.
    pub fn content_version(&self) -> Result<Option<String>> {
        blocking(datalib_etl::doltlite_raw::content_version(&self.pool))
    }

    /// The version a seal of `commit` reports.
    pub fn tree_of(&self, commit: &str) -> Result<String> {
        blocking(datalib_etl::doltlite_raw::tree_of(&self.pool, commit))
    }

    /// The commit a consumer's cursor names, through a fold: see
    /// [`datalib_etl::doltlite_raw::resolve_commit`].
    pub fn resolve_commit(&self, commit: &str, tree: Option<&str>) -> Result<Option<String>> {
        blocking(datalib_etl::doltlite_raw::resolve_commit(
            &self.pool, commit, tree,
        ))
    }

    pub fn close(self) {
        blocking(self.pool.close());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::problems::{Outcome, Problem, Reason, Scope, Stage};
    use datalib_schema::providers::Provider;

    use datalib_sql::{test_sql, test_sql_id, Sql};
    fn store(dir: &Path) -> IndexedMarkdownStore {
        IndexedMarkdownStore::open(dir).expect("open store")
    }

    /// Every per-document lookup `put_document` and `remove_documents`
    /// make, and the lookup of a render unit's documents at each checkpoint,
    /// goes through an index: without them a full render is quadratic in
    /// the store's size. The render unit lookup scanned `markdowns` at every
    /// checkpoint of a 135k-document render.
    #[test]
    fn replacing_a_document_finds_its_old_rows_by_index() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let mut statements = vec![
            DOCUMENTS_FOR_RENDER_UNITS,
            test_sql!("DELETE FROM problems WHERE scope_kind = ? AND scope_key = ?"),
            crate::grid_index::DELETE_DOCUMENT_GRID_ROWS,
            REMOVED_PROBLEMS,
            REMOVED_SECTIONS,
            DOCUMENT_SECTIONS,
            REMOVED_DRAWN,
            DOCUMENT_DRAWN,
            REMOVED_FOLDS,
            DOCUMENT_FOLDS,
        ];
        statements.extend_from_slice(crate::grid_index::DELETE_DOCUMENTS);
        statements.extend_from_slice(crate::grid_index::Store::Render.delete_contacts());
        blocking(crate::grid_index::assert_searched_by_index(
            &st.pool,
            &statements,
        ));
        st.close();
    }

    /// A render pass looks up the grid row of each of the download's
    /// problems by its upstream key. With no index on the key, each lookup
    /// walked every grid row of the source.
    #[test]
    fn the_upstream_key_lookup_searches_its_index() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        // Audited: a constant statement of this module.
        let explain = Sql::dynamic(
            test_sql_id!(),
            format!("EXPLAIN QUERY PLAN {GRID_ROW_BY_UPSTREAM}"),
        );
        let plan: Vec<String> = blocking(
            explain
                .query()
                .bind("message")
                .bind("x")
                .fetch_all(&st.pool),
        )
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("detail"))
        .collect();
        assert!(
            plan.iter().any(|d| d.starts_with("SEARCH grid_rows USING ")
                && d.ends_with(
                    "INDEX grid_rows_by_upstream_key (upstream_entity_kind=? AND upstream_id=?)"
                )),
            "{plan:?}"
        );
        st.close();
    }

    /// A store call from a thread with no runtime must hand its
    /// connection back to the pool. With a runtime built and dropped per
    /// call, sqlx's return-to-pool task died with the runtime, the pool's
    /// size fell to 0, and the next call opened a second connection to
    /// the same file while the first was still closing — `database is
    /// locked`, about once in thirty runs under a parallel test load.
    #[test]
    fn a_store_call_without_a_runtime_returns_its_connection_to_the_pool() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        assert!(tokio::runtime::Handle::try_current().is_err());
        for i in 0..3 {
            st.put_document(&doc(&format!("d{i}"), "fp")).unwrap();
            // `size` is the pool's own count of live connections; the
            // dying runtime dropped the guard that decrements it. Whether
            // it is idle yet is a race with the return task, so not asserted.
            assert_eq!(
                st.pool.size(),
                1,
                "call {i}: the one connection is still pooled"
            );
        }
        st.close();
    }

    /// A store already holding `n` one-row documents, `m0` … `m{n-1}`,
    /// committed. Filled by copying one seed row in a single statement,
    /// so the fill costs the same with or without the lookup indexes.
    fn store_holding(dir: &Path, n: usize) -> IndexedMarkdownStore {
        let st = store(dir);
        st.put_document(&doc("seed", "fp")).unwrap();
        let (_table, columns) = datalib_schema::grid_rows::COLUMNS[0];
        let copied: Vec<String> = columns
            .iter()
            .map(|c| match *c {
                "uuid" | "markdown_uuid" | "conversation_uuid" => "'m' || i".to_string(),
                other => other.to_string(),
            })
            .collect();
        // Audited: column names come from the derive's static list; `n` is
        // a number.
        let fill = Sql::dynamic(
            test_sql_id!(),
            format!(
            "WITH RECURSIVE k(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM k WHERE i + 1 < {n}) \
             INSERT INTO grid_rows ({}) SELECT {} FROM grid_rows, k WHERE uuid = 'seed'",
            columns.join(", "),
            copied.join(", "),
        ),
        );
        blocking(fill.query().execute(&st.pool)).unwrap();
        st.commit("fill").unwrap();
        st
    }

    /// Seconds to replace `replaced` of a store's documents in one batch,
    /// the way the render driver writes them.
    fn seconds_to_replace(st: &IndexedMarkdownStore, replaced: usize) -> f64 {
        let started = std::time::Instant::now();
        st.begin_batch().unwrap();
        for i in 0..replaced {
            st.put_document(&doc(&format!("m{i}"), "fp")).unwrap();
        }
        st.commit_batch().unwrap();
        started.elapsed().as_secs_f64()
    }

    /// Replacing a document costs about the same however many documents
    /// the store holds. Each `put_document` used to scan the whole of
    /// `grid_rows` for the document's old rows, so a full render was
    /// quadratic: a 20k-thread mailbox spent minutes there. Compared
    /// against a small store in the same process, so a slow machine
    /// slows both sides alike; each side's best of three, taken in turn,
    /// so a burst of load on a shared CI runner during one measurement
    /// is not read as a scan.
    #[test]
    fn replacing_a_document_does_not_slow_down_as_the_store_grows() {
        const REPLACED: usize = 700;
        const GROWTH: usize = 80;
        let small_dir = tempfile::tempdir().unwrap();
        let small = store_holding(small_dir.path(), REPLACED);
        let big_dir = tempfile::tempdir().unwrap();
        let big = store_holding(big_dir.path(), GROWTH * REPLACED);
        let (mut small_s, mut big_s) = (f64::MAX, f64::MAX);
        for _ in 0..3 {
            small_s = small_s.min(seconds_to_replace(&small, REPLACED));
            big_s = big_s.min(seconds_to_replace(&big, REPLACED));
        }
        assert!(
            big_s < 5.0 * small_s,
            "{GROWTH}x the documents made replacing {REPLACED} take {:.1}x as long \
             ({small_s:.3}s → {big_s:.3}s): a per-document lookup is scanning",
            big_s / small_s
        );
        small.close();
        big.close();
    }

    fn row(uuid: &str, markdown_uuid: &str) -> GridRow {
        GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Test)
            .kind("Test")
            .source_label("Test")
            .conversation_uuid(markdown_uuid)
            .entire_chat(format!("/chat/{markdown_uuid}"))
            .body("hello")
            .markdown_uuid(Some(markdown_uuid.to_string()))
            .created_at(Some("2026-01-01T00:00:00+00:00".to_string()))
            .is_document(true)
            .item_count(Some(3))
            .build()
            .expect("row")
    }

    fn doc(markdown_uuid: &str, label: &str) -> RenderedMarkdown {
        doc_with(markdown_uuid, label, Vec::new())
    }

    fn doc_with(markdown_uuid: &str, _label: &str, problems: Vec<ProblemRow>) -> RenderedMarkdown {
        RenderedMarkdown {
            searchable: true,
            content_hash: None,
            markdown_uuid: markdown_uuid.to_string(),
            source_id: "src".into(),
            upstream_cursor: None,
            render_unit_key: None,
            render_version: 7,
            rows: vec![row(markdown_uuid, markdown_uuid)],
            sections: Vec::new(),
            front: Default::default(),
            edges: Vec::new(),
            contacts: Vec::new(),
            data_nodes: Vec::new(),
            filings: Vec::new(),
            photos: Vec::new(),
            problems,
        }
    }

    fn edge(edge_uuid: &str, src: &str, dst: &str) -> datalib_schema::edges::EdgeRow {
        datalib_schema::edges::EdgeRow {
            kind: datalib_schema::edges::EdgeKind::Link.as_i64(),
            edge_uuid: edge_uuid.into(),
            src_markdown_uuid: src.into(),
            src_anchor_uuid: None,
            dst_markdown_uuid: dst.into(),
            dst_anchor_uuid: None,
            label: None,
        }
    }

    /// The reader takes each table in one ordered read and groups it in
    /// memory, where it used to look each document up — a full scan apiece
    /// under `dolt_at_`, so a 20k-document store took minutes. The grouping
    /// must still put every row and edge under its own document, in key
    /// order, and `only` must still drop the rest.
    #[test]
    /// A document's people round-trip through the store with every field
    /// they had, and a re-render that no longer names someone drops them —
    /// rows and handles both — the way a re-render drops stale edges.
    fn source_contacts_travel_with_their_document() {
        use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact, Seen};
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let mut riker =
            NormalizedContact::new("src", "email:riker@enterprise.org", ContactKind::Person);
        riker.names = vec!["Will Riker".into()];
        riker.handles = vec![
            ContactHandle::email(None, "riker@enterprise.org"),
            ContactHandle::phone(Some("cell".into()), "(555) 010-1234"),
        ];
        riker.seen = Some(Seen {
            items: 2,
            last_at: Some("2369-05-28T15:08:20+00:00".into()),
        });
        let mut d = doc("a", "fp");
        d.contacts = vec![riker.clone()];
        st.put_document(&d).unwrap();
        st.commit("with riker").unwrap();
        let handles = |st: &IndexedMarkdownStore| -> Vec<String> {
            blocking(
                test_sql!("SELECT handle FROM source_contact_handles ORDER BY handle")
                    .query_scalar()
                    .fetch_all(&st.pool),
            )
            .unwrap()
        };
        assert_eq!(
            handles(&st),
            ["email:riker@enterprise.org"],
            "only a handle the source could normalize is looked up by"
        );
        st.close();

        let rd = IndexedMarkdownStore::open_for_reading(td.path(), None)
            .unwrap()
            .expect("a committed store is readable");
        let pin = rd.pin().unwrap().clone();
        let docs = rd.documents_matching(None, &pin).unwrap();
        assert_eq!(docs[0].contacts, vec![riker]);
        rd.close();

        let st = store(td.path());
        st.put_document(&doc("a", "fp")).unwrap();
        assert!(handles(&st).is_empty());
        let rows: i64 = blocking(
            test_sql!("SELECT count(*) FROM source_contacts")
                .query_scalar()
                .fetch_one(&st.pool),
        )
        .unwrap();
        assert_eq!(rows, 0);
        st.close();
    }

    /// A container and a filing are the document's, like its contacts: read
    /// back with it, and gone when a re-render no longer names them.
    #[test]
    fn containers_and_filings_travel_with_their_document() {
        use datalib_schema::data_nodes::{DataNodeRow, Lineage};
        use datalib_schema::filings::FilingRow;
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let channel = DataNodeRow::new("a", "channel", "src", "Slack channel")
            .named(Some("#bridge".to_string()))
            .upstream("channel", "C01", None)
            .place();
        let thread = DataNodeRow::new("a", "a", "src", "Slack thread")
            .read_together()
            .under(&Lineage::top().below(&channel));
        let label = DataNodeRow::new("a", "label", "src", "Label").place();
        let mut d = doc("a", "fp");
        d.data_nodes = vec![channel.clone(), thread.clone(), label.clone()];
        d.filings = vec![FilingRow::new("a", "a", "label")];
        st.put_document(&d).unwrap();
        st.commit("with containers").unwrap();
        st.close();

        let rd = IndexedMarkdownStore::open_for_reading(td.path(), None)
            .unwrap()
            .expect("a committed store is readable");
        let pin = rd.pin().unwrap().clone();
        let docs = rd.documents_matching(None, &pin).unwrap();
        // The thread is the document's own row, so it sorts as that row.
        let row_key = d
            .rows
            .iter()
            .find(|r| r.uuid == "a")
            .unwrap()
            .sort_key
            .clone();
        assert!(!row_key.is_empty());
        let thread = thread.sort_key(Some(row_key));
        assert_eq!(docs[0].data_nodes, vec![thread, channel, label]);
        assert_eq!(docs[0].filings, vec![FilingRow::new("a", "a", "label")]);
        rd.close();

        let st = store(td.path());
        st.put_document(&doc("a", "fp")).unwrap();
        for table in ["data_nodes", "filings"] {
            let n: i64 = blocking(
                Sql::dynamic(test_sql_id!(), format!("SELECT count(*) FROM {table}"))
                    .query_scalar()
                    .fetch_one(&st.pool),
            )
            .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        st.close();
    }

    /// A document's sections are kept with it: a re-render replaces
    /// them, a document read back from the store (no sections) keeps
    /// them, and removing the document removes them, leaving other
    /// documents' alone.
    #[test]
    fn sections_travel_with_their_document() {
        use crate::section::Section;
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        type Kept = (String, i64, Option<String>, String, bool);
        let kept = |st: &IndexedMarkdownStore| -> Vec<Kept> {
            blocking(
                test_sql!(
                    "SELECT markdown_uuid, seq, uuid, text, framed FROM sections \
                     ORDER BY markdown_uuid, seq"
                )
                .query_as()
                .fetch_all(&st.pool),
            )
            .unwrap()
        };
        let with = |id: &str, sections: Vec<Section>| RenderedMarkdown {
            sections,
            ..doc(id, "fp")
        };
        let item = |uuid: &str, said: &str| {
            Section::item(
                uuid,
                "msg".into(),
                crate::item::Item::new(datalib_schema::sections::ItemForm::Message, said.into()),
            )
        };
        st.put_document(&with(
            "a",
            vec![
                Section::frame("# Tea\n".into()),
                item("a", "hi"),
                Section::keyed("m2", "<div>Data: yes</div>\n".into()),
            ],
        ))
        .unwrap();
        st.put_document(&with("b", vec![item("b", "b")])).unwrap();
        let b = ("b".into(), 0, Some("b".into()), "b".into(), false);
        assert_eq!(
            kept(&st),
            vec![
                ("a".into(), 1, Some("a".into()), "hi".into(), false),
                (
                    "a".into(),
                    2,
                    Some("m2".into()),
                    "<div>Data: yes</div>\n".into(),
                    true
                ),
                b.clone(),
            ]
        );

        st.put_document(&with("a", vec![item("a", "bye")])).unwrap();
        let a_now = ("a".into(), 0, Some("a".into()), "bye".into(), false);
        assert_eq!(kept(&st), vec![a_now.clone(), b.clone()]);

        st.put_document(&doc("a", "fp")).unwrap();
        assert_eq!(kept(&st), vec![a_now, b.clone()], "read back: kept");

        st.remove_documents(&["a".to_string()]).unwrap();
        assert_eq!(kept(&st), vec![b]);
        st.close();
    }

    /// The search index reads a document's kept sections with the author
    /// and time of the row each is, in order, and only the documents it
    /// asks for, at the commit the reader opened.
    #[test]
    fn a_reader_gets_kept_sections_with_their_rows_facts() {
        use crate::section::Section;
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        for id in ["a", "b"] {
            let d = RenderedMarkdown {
                sections: vec![
                    Section::unkeyed("Page text\n".into()),
                    Section::item(
                        id,
                        "msg".into(),
                        crate::item::Item::new(
                            datalib_schema::sections::ItemForm::Message,
                            "hi".into(),
                        ),
                    ),
                ],
                ..doc(id, "fp")
            };
            st.put_document(&d).unwrap();
        }
        st.commit("two documents").unwrap();
        st.close();

        let rd = IndexedMarkdownStore::open_for_reading(td.path(), None)
            .unwrap()
            .expect("a committed store is readable");
        let got = rd.kept_sections(&["a".to_string()]).unwrap();
        let shape: Vec<_> = got
            .iter()
            .map(|k| {
                (
                    k.seq,
                    k.uuid.as_deref(),
                    k.framed,
                    k.author.as_deref(),
                    k.created_at.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, None, true, None, None),
                (1, Some("a"), false, None, Some("2026-01-01T00:00:00+00:00")),
            ]
        );
        assert_eq!(rd.markdown_rows_in(&["b".to_string()]).unwrap().len(), 1);
        rd.close();
    }

    /// A renderer that hands a container under another document's id has
    /// a bug: the row would outlive the document that wrote it.
    #[test]
    fn a_container_carried_by_another_document_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let mut d = doc("a", "fp");
        d.data_nodes = vec![datalib_schema::data_nodes::DataNodeRow::new(
            "b", "chat", "src", "Chat",
        )];
        let err = st.put_document(&d).unwrap_err();
        assert!(
            format!("{err:#}").contains("carries container chat"),
            "{err:#}"
        );
        st.close();
    }

    #[test]
    fn a_pinned_read_groups_rows_and_edges_under_their_own_documents() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        st.transaction(|| {
            for id in ["b", "a", "c"] {
                let mut d = doc(id, "fp");
                for extra in ["z", "m"] {
                    let mut r = row(&format!("{id}-{extra}"), id);
                    r.is_document = false;
                    d.rows.push(r);
                }
                d.edges = vec![
                    edge(&format!("{id}-e2"), id, "a"),
                    edge(&format!("{id}-e1"), id, "c"),
                ];
                st.put_document(&d)?;
            }
            Ok(())
        })
        .unwrap();
        st.commit("three documents").unwrap();
        st.close();

        let rd = IndexedMarkdownStore::open_for_reading(td.path(), None)
            .unwrap()
            .expect("a committed store is readable");
        let pin = rd.pin().unwrap().clone();
        let shape = |docs: Vec<RenderedMarkdown>| -> Vec<(String, Vec<String>, Vec<String>)> {
            docs.into_iter()
                .map(|d| {
                    (
                        d.markdown_uuid,
                        d.rows.into_iter().map(|r| r.uuid).collect(),
                        d.edges.into_iter().map(|e| e.edge_uuid).collect(),
                    )
                })
                .collect()
        };
        let expect = |id: &str| {
            (
                id.to_string(),
                vec![id.to_string(), format!("{id}-m"), format!("{id}-z")],
                vec![format!("{id}-e1"), format!("{id}-e2")],
            )
        };

        let whole = rd.documents_matching(None, &pin).unwrap();
        assert_eq!(shape(whole), vec![expect("a"), expect("b"), expect("c")]);

        let only: HashSet<String> = ["c", "gone"].map(String::from).into();
        let some = rd.documents_matching(Some(&only), &pin).unwrap();
        assert_eq!(
            shape(some),
            vec![expect("c")],
            "an id with no document is absent"
        );

        let none = rd.documents_matching(Some(&HashSet::new()), &pin).unwrap();
        assert!(none.is_empty());
        rd.close();
    }

    /// A renderer that forgets to mark its document row, or marks two,
    /// fails its render outright: the old fallback ("the row whose uuid
    /// matches, else the first") is exactly the guess `is_document`
    /// exists to remove.
    #[test]
    fn a_document_must_have_exactly_one_document_row() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());

        let mut none = doc("d-none", "fp");
        none.rows[0].is_document = false;
        let err = st.put_document(&none).unwrap_err();
        assert!(
            format!("{err:#}").contains("none of its 1 rows is marked is_document"),
            "{err:#}"
        );

        let mut two = doc("d-two", "fp");
        two.rows.push(row("d-two-extra", "d-two"));
        let err = st.put_document(&two).unwrap_err();
        assert!(
            format!("{err:#}").contains("both marked is_document"),
            "{err:#}"
        );

        // Neither half-written document reached the store.
        let n: i64 = blocking(async {
            test_sql!("SELECT COUNT(*) FROM markdowns")
                .query_scalar()
                .fetch_one(&st.pool)
                .await
        })
        .unwrap();
        assert_eq!(n, 0);
        st.close();
    }

    /// The document row's stamps are what `markdowns` carries — copied,
    /// not recomputed from the inner rows, so a PR's `updated_at` wins
    /// over its last comment.
    #[test]
    fn markdowns_takes_its_stamps_from_the_document_row() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let mut d = doc("d-stamped", "fp");
        d.rows[0].created_at = Some("2026-01-01T00:00:00+00:00".into());
        d.rows[0].modified_at = Some("2026-03-01T00:00:00+00:00".into());
        let mut inner = row("d-stamped-m1", "d-stamped");
        inner.is_document = false;
        inner.created_at = Some("2026-02-01T00:00:00+00:00".into());
        d.rows.push(inner);
        st.put_document(&d).unwrap();
        let (created, modified): (Option<String>, Option<String>) = blocking(async {
            test_sql!(
                "SELECT created_at, modified_at FROM markdowns WHERE markdown_uuid = 'd-stamped'"
            )
            .query_as()
            .fetch_one(&st.pool)
            .await
        })
        .unwrap();
        assert_eq!(created.as_deref(), Some("2026-01-01T00:00:00+00:00"));
        assert_eq!(modified.as_deref(), Some("2026-03-01T00:00:00+00:00"));
        st.close();
    }

    /// The storage report is rendered by datalib, not by any of the
    /// source's processors, so its version must stay out of
    /// `render_versions` — the set the render step checks against what
    /// those processors declare.
    ///
    /// Getting this wrong is not subtle and not local: every source in
    /// the pipeline failed its render with "carry render_version [1],
    /// which none of its processors declare", and a download-only
    /// source (whose only document *is* the report) failed with
    /// "none of its processors implement render_version".
    #[test]
    fn the_storage_report_is_not_counted_as_a_provider_render_version() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());

        let mut report = doc("storage-doc", "fp-1");
        report.render_version = 1;
        report.rows = vec![GridRow::builder()
            .uuid("storage-doc")
            .provider(datalib_schema::providers::Provider::Datalib)
            .kind("Source Size")
            .source_label("Storage")
            .conversation_uuid("storage-doc")
            .entire_chat("/chat/storage-doc")
            .body("src/raw — 1.0 KiB")
            .markdown_uuid(Some("storage-doc".to_string()))
            .byte_size(Some(1024))
            .item_count(Some(1))
            .is_document(true)
            .build()
            .expect("row")];
        st.put_document(&report).expect("store report");

        assert!(
            st.render_versions().expect("versions").is_empty(),
            "a source whose only document is the storage report must \
             report no provider render versions at all"
        );

        // A real provider document alongside it is still counted.
        st.put_document(&doc("real-doc", "fp-2"))
            .expect("store provider doc");
        assert_eq!(
            st.render_versions().expect("versions"),
            BTreeSet::from([7]),
            "the provider's version is reported; the report's is not"
        );
    }

    fn sample(
        subject: &str,
        at: &str,
        bytes: Option<i64>,
        items: Option<i64>,
    ) -> SourceMeasurementRow {
        SourceMeasurementRow {
            subject: subject.into(),
            kind: "tree".into(),
            measured_at_utc: at.into(),
            tz_offset: None,
            bytes,
            items,
        }
    }

    /// The whole point of the second table: a later run adds to the
    /// series rather than replacing it. If this ever upserts on
    /// `subject` alone there is no history left to draw, and nothing
    /// downstream would report an error — the newest number would still
    /// be right.
    #[test]
    fn a_second_run_appends_to_the_series_instead_of_replacing_it() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());

        st.put_measurements(&[
            sample("src/raw", "2026-09-01T10:00:00-07:00", Some(100), Some(1)),
            sample(
                "src/raw/a.doltlite_db",
                "2026-09-01T10:00:00-07:00",
                Some(90),
                None,
            ),
        ])
        .expect("first run");
        st.put_measurements(&[sample(
            "src/raw",
            "2026-09-02T10:00:00-07:00",
            Some(250),
            Some(2),
        )])
        .expect("second run");

        let series: Vec<(String, Option<i64>)> = blocking(async {
            test_sql!(
                "SELECT measured_at_utc, bytes FROM source_measurements \
                 WHERE subject = 'src/raw' ORDER BY measured_at_utc"
            )
            .query()
            .fetch_all(&st.pool)
            .await
            .expect("read the series")
            .into_iter()
            .map(|r| {
                (
                    r.try_get::<String, _>(0).unwrap(),
                    r.try_get::<Option<i64>, _>(1).unwrap(),
                )
            })
            .collect()
        });
        assert_eq!(
            series,
            vec![
                ("2026-09-01T10:00:00-07:00".to_string(), Some(100)),
                ("2026-09-02T10:00:00-07:00".to_string(), Some(250)),
            ],
            "both runs must survive"
        );
    }

    /// A NULL byte count is a real value — every table row carries one,
    /// because a content-addressed store has no per-table byte layout.
    /// It must round-trip as NULL rather than as 0.
    #[test]
    fn an_absent_byte_count_round_trips_as_null() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        st.put_measurements(&[sample(
            "src/raw#t",
            "2026-09-01T10:00:00-07:00",
            None,
            Some(5),
        )])
        .expect("write");

        let (bytes, items): (Option<i64>, Option<i64>) = blocking(async {
            let r = test_sql!("SELECT bytes, items FROM source_measurements")
                .query()
                .fetch_one(&st.pool)
                .await
                .expect("read back");
            (r.try_get(0).unwrap(), r.try_get(1).unwrap())
        });
        assert_eq!(bytes, None);
        assert_eq!(items, Some(5));
    }

    /// A re-run under the same pinned `--now` restates the series
    /// rather than failing the whole render on a duplicate key.
    #[test]
    fn re_measuring_at_the_same_instant_restates_rather_than_fails() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let at = "2026-09-01T10:00:00-07:00";
        st.put_measurements(&[sample("src/raw", at, Some(100), Some(1))])
            .expect("first");
        st.put_measurements(&[sample("src/raw", at, Some(140), Some(2))])
            .expect("same instant again must not fail");

        let (n, bytes): (i64, Option<i64>) = blocking(async {
            let r = test_sql!("SELECT COUNT(*), MAX(bytes) FROM source_measurements")
                .query()
                .fetch_one(&st.pool)
                .await
                .expect("read back");
            (r.try_get(0).unwrap(), r.try_get(1).unwrap())
        });
        assert_eq!(n, 1, "one instant is one sample");
        assert_eq!(bytes, Some(140), "the later write wins");
    }

    fn problem(uuid: &str, scope: &str) -> ProblemRow {
        ProblemRow::new(
            "src",
            Stage::GridRow,
            Scope::Markdown(scope),
            Some(uuid),
            Outcome::Nulled,
            Problem::field("created_at", Reason::CoercionFailed, "not-a-date"),
            Some(7),
        )
    }

    /// The write path has no skip of its own: the same document written
    /// twice is the same row twice, and doltlite reports nothing between
    /// the two commits. The store-level check is `dolt_diff` staying
    /// empty; here, that the second write is accepted and the row is one.
    #[tokio::test(flavor = "multi_thread")]
    async fn writing_the_same_document_twice_is_one_row() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(&doc("md-1", "fp-1")).unwrap();
        s.put_document(&doc("md-1", "fp-1")).unwrap();
        let n: i64 = blocking(async {
            test_sql!("SELECT COUNT(*) FROM markdowns")
                .query_scalar()
                .fetch_one(&s.pool)
                .await
        })
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.render_versions().unwrap(), [7].into_iter().collect());
    }

    /// The count is of what the source has, so the storage report —
    /// datalib's own document about the source — is left out of it. A
    /// source that renders nothing holds only that report, and the
    /// column has to read zero there rather than one.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_storage_report_is_not_one_of_the_source_s_documents() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(&doc("storage", "fp-s")).unwrap();
        let none = Holdings::default();
        assert_eq!(s.holdings(Some("storage")).unwrap(), none);
        let one = Holdings {
            documents: 1,
            items: 3,
        };
        assert_eq!(s.holdings(None).unwrap(), one, "without one to skip");
        s.put_document(&doc("md-1", "fp-1")).unwrap();
        s.put_document(&doc("md-2", "fp-2")).unwrap();
        let two = Holdings {
            documents: 2,
            items: 6,
        };
        assert_eq!(s.holdings(Some("storage")).unwrap(), two);
    }

    /// A document that does not say how many things it holds would
    /// read as holding none, and the Items column would quietly
    /// undercount; the render store refuses it instead.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_row_without_an_item_count_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        let mut uncounted = doc("md-1", "fp-1");
        uncounted.rows[0].item_count = None;
        let err = s.put_document(&uncounted).unwrap_err();
        assert!(format!("{err:#}").contains("has no item_count"), "{err:#}");
        assert_eq!(s.holdings(None).unwrap(), Holdings::default());
    }

    /// Re-rendering replaces a document's rows rather than accumulating
    /// them — the delete-then-insert that makes a re-render idempotent.
    #[tokio::test(flavor = "multi_thread")]
    async fn re_rendering_a_document_replaces_its_rows() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(&doc("md-1", "fp-1")).unwrap();
        s.put_document(&doc("md-1", "fp-2")).unwrap();

        let n: i64 = blocking(async {
            test_sql!("SELECT COUNT(*) FROM grid_rows")
                .query_scalar()
                .fetch_one(&s.pool)
                .await
        })
        .unwrap();
        assert_eq!(n, 1, "one row, not two");
    }

    fn doc_in(markdown_uuid: &str, row_uuid: &str, fp: &str) -> RenderedMarkdown {
        let mut d = doc(markdown_uuid, fp);
        d.rows = vec![row(row_uuid, markdown_uuid)];
        d
    }

    /// A row that moved between two documents of one source — a message
    /// in a chat whose document id is minted from a name that changed —
    /// is taken over by the document
    /// that now emits it, whichever of the two renders first. Found by
    /// the provider contract harness on google_takeout: the incoming
    /// document failed on `UNIQUE constraint failed: grid_rows.uuid`
    /// while the old owner, rendered by an earlier run, still held it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_that_moved_between_documents_is_taken_over() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let earlier = store(root);
        earlier
            .put_document(&doc_in("doc-a", "row-1", "fp-a"))
            .unwrap();
        earlier.close();

        // A second open is a second run, whatever the clock says.
        let later = store(root);
        later
            .put_document(&doc_in("doc-b", "row-1", "fp-b"))
            .expect("the row moves to doc-b");
        let owner: String = blocking(async {
            test_sql!("SELECT markdown_uuid FROM grid_rows WHERE uuid = 'row-1'")
                .query_scalar()
                .fetch_one(&later.pool)
                .await
        })
        .unwrap();
        assert_eq!(owner, "doc-b");
        // doc-a re-renders without the row: its delete-by-owner must not
        // touch what doc-b now holds.
        later.put_document(&doc("doc-a", "fp-a2")).unwrap();
        let n: i64 = blocking(async {
            test_sql!("SELECT COUNT(*) FROM grid_rows WHERE uuid = 'row-1'")
                .query_scalar()
                .fetch_one(&later.pool)
                .await
        })
        .unwrap();
        assert_eq!(n, 1, "the moved row survives its old owner's re-render");
    }

    /// Two documents of one run minting one row uuid is a finding, not a
    /// move, and still fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_documents_in_one_run_minting_one_uuid_is_an_error() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(&doc_in("doc-a", "row-1", "fp-a")).unwrap();
        let err = s
            .put_document(&doc_in("doc-b", "row-1", "fp-b"))
            .expect_err("a same-run collision must fail");
        assert!(format!("{err:#}").contains("written this run"), "{err:#}");
    }

    /// The sweep is scoped to the document reprocessed. A document that
    /// was *skipped* this run keeps its problems — the failure mode
    /// worth a test, because the obvious "delete what wasn't re-emitted"
    /// rule empties the table on every steady-state run.
    #[tokio::test(flavor = "multi_thread")]
    async fn problems_survive_a_run_that_skipped_their_document() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(&doc_with("md-1", "fp-1", vec![problem("row-a", "md-1")]))
            .unwrap();
        s.put_document(&doc_with("md-2", "fp-1", vec![problem("row-b", "md-2")]))
            .unwrap();
        assert_eq!(
            s.own_problem_counts()
                .unwrap()
                .get(&Severity::Warning)
                .copied(),
            Some(2)
        );

        // A second run that only reprocesses md-2, and finds it clean.
        s.put_document(&doc("md-2", "fp-2")).unwrap();

        let counts = s.own_problem_counts().unwrap();
        assert_eq!(
            counts.get(&Severity::Warning).copied(),
            Some(1),
            "md-2's problem cleared; md-1's must not have — it was never looked at"
        );
    }

    /// A document that comes back with no rows — every one rejected by
    /// validation — is not a document: its `markdowns` row goes, so a
    /// render unit sweep never finds it under the render unit it used to be in.
    /// The problems saying why stay. Found by the contract harness on a
    /// gitlab merge request re-keyed under another render unit whose rows all
    /// failed `created_at`: `markdowns` kept the old render unit.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_re_rendered_with_no_rows_is_gone_render_unit_and_all() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        let mut first = doc("md-1", "fp-1");
        first.render_unit_key = Some("mr!17".into());
        s.put_document(&first).unwrap();
        assert_eq!(
            s.documents_for_render_units(&["mr!17"]).unwrap(),
            vec![("mr!17".to_string(), "md-1".to_string())]
        );

        let mut empty = doc_with("md-1", "fp-2", vec![problem("row-a", "md-1")]);
        empty.render_unit_key = Some("mr!17~new".into());
        empty.rows.clear();
        s.put_document(&empty).unwrap();

        assert!(
            s.documents_for_render_units(&["mr!17", "mr!17~new"])
                .unwrap()
                .is_empty(),
            "no markdowns row under either render unit"
        );
        assert!(s.all_document_uuids().unwrap().is_empty());
        assert_eq!(
            s.own_problem_counts()
                .unwrap()
                .get(&Severity::Warning)
                .copied(),
            Some(1),
            "the record of why every row went stays"
        );
    }

    /// A problem clears when the document is reprocessed and comes back
    /// clean. This is "overwritten or removed upon later success".
    #[tokio::test(flavor = "multi_thread")]
    async fn a_fixed_document_loses_its_problems() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(&doc_with("md-1", "fp-1", vec![problem("row-a", "md-1")]))
            .unwrap();
        assert_eq!(
            s.own_problem_counts()
                .unwrap()
                .get(&Severity::Warning)
                .copied(),
            Some(1)
        );

        s.put_document(&doc("md-1", "fp-2")).unwrap();
        assert!(
            s.own_problem_counts().unwrap().is_empty(),
            "reprocessed clean ⇒ no problem rows left"
        );
    }

    /// A document the run could not produce at all — a PDF whose
    /// conversion failed — carries its problem on its own scope with no
    /// document behind it, and the run that does produce it sweeps the
    /// problem like any other. Reported twice, it is one row with its
    /// first-seen stamp kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_that_failed_to_render_keeps_its_problem_until_it_renders() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        let failed = ProblemRow::new(
            "src",
            Stage::Render,
            Scope::Markdown("md-1"),
            Some("md-1"),
            Outcome::Dropped,
            Problem::record(Reason::RenderFailed, "pdftotext: exit 1"),
            Some(7),
        );
        s.put_document_problems("md-1", std::slice::from_ref(&failed))
            .unwrap();
        s.put_document_problems("md-1", &[failed]).unwrap();
        assert_eq!(
            s.own_problem_counts()
                .unwrap()
                .get(&Severity::Error)
                .copied(),
            Some(1),
            "the same failure twice is one row"
        );
        assert!(
            s.all_document_uuids().unwrap().is_empty(),
            "no document behind it"
        );

        s.put_document(&doc("md-1", "fp-1")).unwrap();
        assert!(
            s.own_problem_counts().unwrap().is_empty(),
            "the run that produced the document swept its failure"
        );
    }

    /// The download's problems, copied in so the index can link them to
    /// a document, are not render's: its count leaves them out, or the
    /// Manage screen shows one warning on both the Download and the
    /// Render row.
    #[tokio::test(flavor = "multi_thread")]
    async fn copied_fetch_problems_are_not_counted_as_renders() {
        let td = tempfile::tempdir().unwrap();
        let s = store(td.path());
        let refused = ProblemRow::new(
            "src",
            Stage::Fetch,
            Scope::Entity("listing:org:Acme"),
            None,
            Outcome::Dropped,
            Problem::record(Reason::Forbidden, "this org refuses the credential")
                .severity(Severity::Warning),
            None,
        );
        s.replace_stage_problems(Stage::Fetch, &[refused]).unwrap();
        assert!(s.own_problem_counts().unwrap().is_empty());

        let unreadable = ProblemRow::new(
            "src",
            Stage::Parse,
            Scope::Entity("users:u1"),
            None,
            Outcome::Dropped,
            Problem::record(Reason::Undeserializable, "{"),
            Some(7),
        );
        s.put_entity_problems(&["users"], &[unreadable]).unwrap();
        assert_eq!(
            s.own_problem_counts().unwrap(),
            HashMap::from([(Severity::Error, 1)])
        );
    }

    /// Entity-scoped problems clear by what the parse read: a table it
    /// read whole is replaced by the report, a table it read in part
    /// keeps what it had, and an entity named in the report is always
    /// replaced by name.
    #[tokio::test(flavor = "multi_thread")]
    async fn entity_problems_clear_by_what_the_parse_read() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        let unreadable = |entity: &str| {
            ProblemRow::new(
                "src",
                Stage::Parse,
                Scope::Entity(entity),
                None,
                Outcome::Dropped,
                Problem::record(Reason::Undeserializable, "{"),
                Some(7),
            )
        };
        s.put_entity_problems(
            &["users", "messages"],
            &[unreadable("users:u1"), unreadable("messages:m1")],
        )
        .unwrap();
        assert_eq!(
            s.own_problem_counts()
                .unwrap()
                .get(&Severity::Error)
                .copied(),
            Some(2)
        );
        // A narrowed run read users whole and no messages: u1 reads
        // cleanly now and goes; m1 was not looked at and stays.
        s.put_entity_problems(&["users"], &[]).unwrap();
        assert_eq!(
            s.own_problem_counts()
                .unwrap()
                .get(&Severity::Error)
                .copied(),
            Some(1),
            "the table not read keeps its rows"
        );
        // A partial read that names m1 again replaces it by name and
        // adds m2, without claiming anything about the rest.
        s.put_entity_problems(&[], &[unreadable("messages:m1"), unreadable("messages:m2")])
            .unwrap();
        assert_eq!(
            s.own_problem_counts()
                .unwrap()
                .get(&Severity::Error)
                .copied(),
            Some(2)
        );
        s.put_entity_problems(&["messages"], &[]).unwrap();
        assert!(s.own_problem_counts().unwrap().is_empty());
    }

    /// A render unit that declared a whole table is stale when any row of it
    /// moves, and one that declared a row is not stale for its neighbours.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_whole_table_input_matches_any_row_of_it() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_inputs("page", &[Input::whole_table("readings")])
            .unwrap();
        s.put_inputs("one", &[Input::new("readings", "r-1")])
            .unwrap();

        let stale = s
            .render_units_reading(&[Input::new("readings", "r-2")])
            .unwrap();
        assert_eq!(stale, ["page".to_string()].into_iter().collect());
        let stale = s
            .render_units_reading(&[Input::new("readings", "r-1")])
            .unwrap();
        assert_eq!(
            stale,
            ["page".to_string(), "one".to_string()]
                .into_iter()
                .collect()
        );
        let stale = s
            .render_units_reading(&[Input::new("devices", "d-1")])
            .unwrap();
        assert!(stale.is_empty());
    }

    /// A document read a page at a time, from the start and on by each
    /// page's cursor, is the document whole: every piece once and in order,
    /// a fold never cut, and the front matter only on the first page. A
    /// page around a piece holds it; back from the last page reads the same
    /// pieces again.
    #[test]
    fn a_document_reads_the_same_a_page_at_a_time() {
        use crate::section::{join, Section};
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let boxed = |u: &str| Section::boxed(u, "msg".into(), &format!("{u} said\n"));
        let sections = vec![
            Section::frame("---\ntitle: x\n---\n\n# Ten Forward\n\n".into()),
            boxed("a"),
            Section::fold_open("tool-group", "🛠 2 tool steps"),
            boxed("b"),
            boxed("c"),
            Section::fold_close(),
            boxed("d"),
            boxed("e"),
        ];
        st.put_document(&RenderedMarkdown {
            sections: sections.clone(),
            ..doc("d1", "fp")
        })
        .unwrap();

        let page = |ask| st.document_page("d1", ask, 2).unwrap().unwrap();
        let text = |p: &DocumentPage| p.sections.iter().map(|s| s.md.as_str()).collect::<String>();
        let mut pages = vec![page(PageAsk::Start)];
        while let Some(after) = pages.last().unwrap().after {
            pages.push(page(PageAsk::After(after)));
        }
        assert_eq!(
            pages.iter().map(|p| p.sections.len()).collect::<Vec<_>>(),
            [2, 5, 1],
            "title and a; the fold whole and d; e"
        );
        assert!(pages[0].from_top && !pages[1].from_top);
        assert_eq!(pages.iter().map(text).collect::<String>(), join(&sections));

        let around = page(PageAsk::At("c".into()));
        assert!(around
            .sections
            .iter()
            .any(|s| s.uuid.as_deref() == Some("c")));
        let back = page(PageAsk::Before(pages[1].after.unwrap()));
        assert_eq!(text(&back), text(&pages[1]));
        assert_eq!(st.document_page("nope", PageAsk::Start, 2).unwrap(), None);
        st.close();
    }
}
