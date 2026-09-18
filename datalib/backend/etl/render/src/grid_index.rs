//! The unified grid index: one table stacked from every source's render
//! store.
//!
//! Two entry points, which are the same write path from two sides.
//! [`apply_one`] writes one rendered document; [`build_grid_index`] stacks
//! every source's store into the unified index, which is the `grid_index` DAG
//! step's whole job.
//!
//! Writes are delete-then-insert, so a re-render replaces a document's rows
//! rather than accumulating them. Nothing here decides whether a document
//! changed: doltlite's content-addressed storage makes rewriting an
//! identical row free, and `dolt_diff` reports only what actually moved.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use datalib_etl::bulk::BulkUpsertable;
use datalib_schema::edges::{EdgeRow, DDL as EDGES_DDL};
use datalib_schema::grid_rows::{GridRow, DDL as GRID_ROWS_DDL};
use datalib_schema::markdowns::DDL as MARKDOWNS_TABLE_DDL;
use datalib_schema::source_cursors::{SourceCursorRow, DDL as SOURCE_CURSORS_DDL};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tokio::sync::Mutex;

use crate::section::Section;

/// Serializes concurrent writers against one doltlite index pool, and
/// optionally batches every write into one transaction.
///
/// doltlite serializes writes at the file level, so per-task pool connections
/// calling `apply_one` race for the write lock and eventually see `(code 5)
/// database is locked`. Batching matters as much: each per-doc auto-commit
/// costs ~50ms, because every statement boundary materializes the prolly
/// tree's manifest.
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
    /// document an earlier run wrote" — see `insert_grid_row`.
    written: std::sync::Mutex<std::collections::HashSet<String>>,
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
        }
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
}

/// Every `CREATE TABLE` in the grid index, in creation order. One list, so
/// the DDL pass and the schema check can't drift into covering different
/// sets of tables.
fn index_ddl() -> impl Iterator<Item = &'static str> {
    GRID_ROWS_DDL
        .iter()
        .map(|(_table, ddl)| *ddl)
        .chain(std::iter::once(MARKDOWNS_DDL))
        .chain(EDGES_DDL.iter().map(|(_table, ddl)| *ddl))
        // `source_cursors` belongs in this list, not beside it: the reconcile
        // drops and rebuilds every table named here together, and a cursor
        // that survived a rebuild would tell the next run "nothing changed"
        // about an index that had just been emptied.
        .chain(SOURCE_CURSORS_DDL.iter().map(|(_table, ddl)| *ddl))
}

/// Apply the index DDL, rebuilding from scratch if what's on disk no longer
/// matches.
///
/// **Drop and rebuild, rather than `ALTER TABLE … ADD COLUMN`** — the
/// opposite of [`datalib_etl::doltlite_raw::open`]'s policy, because every row here
/// is a pure function of a row in a source's render store, so a rebuild costs
/// one local scan. It is also the only answer that yields correct values:
/// `ADD COLUMN` leaves existing rows NULL, and the cursor then never
/// revisits them.
///
/// Every table goes together even when only one drifted — see the note on
/// `source_cursors` in [`index_ddl`].
pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
    for ddl in index_ddl() {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .with_context(|| format!("create {}", table_of(ddl)))?;
    }
    reconcile_index_schema(pool).await
}

/// The `grid_index` step's handle on the index: the one way to open it
/// for writing.
///
/// Through [`datalib_etl::doltlite_raw::open_derived`] for what every
/// writer gets there — a crashed run's dirty rows sealed into their own
/// rescue commit, one connection never recycled — and with no DDL of
/// its own, because the index reconciles its schema by
/// [`init_schema`]'s all-or-nothing rule rather than `open`'s per-table
/// one. The schema is then committed here, as `open` would have: a
/// reader cannot tell a table nobody committed from a source with no
/// rows, and one build without doltlite fails loudly at this check
/// instead of indexing nothing and reporting success.
pub async fn open_index(db_path: &Path) -> Result<SqlitePool> {
    let pool = datalib_etl::doltlite_raw::open_derived(db_path, &[])
        .await
        .with_context(|| format!("open the grid index at {}", db_path.display()))?;
    init_schema(&pool).await?;
    datalib_etl::doltlite_raw::commit_run(&pool, "schema: grid index")
        .await
        .context("commit the grid index schema")?;
    anyhow::ensure!(
        datalib_etl::pin::carries_committed_schema(&pool).await,
        "opened {} but its tables are not committed: either the schema commit \
         did not take, or this binary is not linked against doltlite",
        db_path.display()
    );
    Ok(pool)
}

/// The table a DDL statement creates, for error messages. Degrades to the
/// raw SQL rather than panicking.
fn table_of(ddl: &str) -> String {
    datalib_etl::doltlite_raw::parse_create_table_name(ddl).unwrap_or_else(|| ddl.to_string())
}

/// Drop and recreate every index table if any one of them disagrees with
/// its DDL. See [`init_schema`] for why it is all-or-nothing.
async fn reconcile_index_schema(pool: &SqlitePool) -> Result<()> {
    let mut drift: Vec<String> = Vec::new();
    for ddl in index_ddl() {
        let Some(table) = datalib_etl::doltlite_raw::parse_create_table_name(ddl) else {
            continue;
        };
        let declared = datalib_etl::doltlite_raw::declared_column_names(ddl, &table)
            .await
            .with_context(|| format!("declared columns for {table}"))?;
        let actual = datalib_etl::doltlite_raw::actual_column_names(pool, &table)
            .await
            .with_context(|| format!("actual columns for {table}"))?;
        if declared == actual {
            continue;
        }
        let missing: Vec<&str> = declared
            .difference(&actual)
            .map(String::as_str)
            .collect::<Vec<_>>();
        let extra: Vec<&str> = actual
            .difference(&declared)
            .map(String::as_str)
            .collect::<Vec<_>>();
        drift.push(format!(
            "{table} (missing: [{}], unexpected: [{}])",
            missing.join(", "),
            extra.join(", ")
        ));
    }
    if drift.is_empty() {
        return Ok(());
    }

    tracing::warn!(
        drift = %drift.join("; "),
        "grid_index: index schema predates this build; dropping and rebuilding \
         every index table from the per-source render stores (no re-download, \
         no re-render)"
    );
    for ddl in index_ddl() {
        let Some(table) = datalib_etl::doltlite_raw::parse_create_table_name(ddl) else {
            continue;
        };
        // Audited: `table` is parsed out of our own static index DDL.
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
            .execute(pool)
            .await
            .with_context(|| format!("drop {table} for index rebuild"))?;
    }
    for ddl in index_ddl() {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .with_context(|| format!("recreate {}", table_of(ddl)))?;
    }
    Ok(())
}

/// The index side of `markdowns.renderer_version` (`"<index>.<render>"`).
/// Bump when the rendered `.md` layout changes for every provider at
/// once: every document's version then differs from its store's.
pub const RENDERER_VERSION: &str = "rust-v1";

// ── Cross-source id collision detection ─────────────────────────────

/// One id claimed by two different sources inside a single index run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdCollision {
    /// Which id space collided — `"markdown_uuid"` or `"grid_rows.uuid"`.
    pub id_kind: &'static str,
    /// The contested id.
    pub id: String,
    /// Source that claimed it first (sidecars are walked in sorted order, so
    /// "first" is stable across runs).
    pub first_source: String,
    /// `markdown_uuid` the first claim arrived under.
    pub first_markdown_uuid: String,
    /// Source that claimed it second — the one whose data would have won or
    /// blown up.
    pub second_source: String,
    /// `markdown_uuid` the second claim arrived under.
    pub second_markdown_uuid: String,
}

impl std::fmt::Display for IdCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "two sources claim the same {}: {} \
             — first from source {:?} (markdown {}), then from source {:?} (markdown {}). \
             Either the same upstream account is configured twice, or this provider's id \
             recipe is missing a discriminator. Nothing was written; fix the config (or the \
             recipe) and re-run.",
            self.id_kind,
            self.id,
            self.first_source,
            self.first_markdown_uuid,
            self.second_source,
            self.second_markdown_uuid,
        )
    }
}

/// Which source claimed each id during ONE index run.
///
/// Two sources emitting the same `markdown_uuid` or `grid_rows.uuid` is not a
/// benign duplicate: a full overlap erases the first source's rows with no
/// error and no row-count change, and a partial one rolls the batch back with
/// an error naming neither source. This makes both loud, and names both sides.
///
/// Run-scoped on purpose — checking ids already in the database would flag a
/// source *rename*, which is legitimate.
#[derive(Debug, Default)]
pub struct IdClaims {
    /// markdown_uuid → source that claimed it.
    markdowns: HashMap<String, String>,
    /// grid_rows.uuid → (source, markdown_uuid) that claimed it.
    rows: HashMap<String, (String, String)>,
}

impl IdClaims {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one sidecar's claims, returning the first collision found.
    /// Same-source re-claims are impossible by construction, so any repeat is
    /// a genuine cross-source clash.
    pub fn claim(
        &mut self,
        source_id: &str,
        markdown_uuid: &str,
        rows: &[GridRow],
    ) -> Option<IdCollision> {
        if let Some(prior) = self.markdowns.get(markdown_uuid) {
            return Some(IdCollision {
                id_kind: "markdown_uuid",
                id: markdown_uuid.to_string(),
                first_source: prior.clone(),
                first_markdown_uuid: markdown_uuid.to_string(),
                second_source: source_id.to_string(),
                second_markdown_uuid: markdown_uuid.to_string(),
            });
        }
        self.markdowns
            .insert(markdown_uuid.to_string(), source_id.to_string());

        for row in rows {
            if let Some((prior_source, prior_md)) = self.rows.get(&row.uuid) {
                return Some(IdCollision {
                    id_kind: "grid_rows.uuid",
                    id: row.uuid.clone(),
                    first_source: prior_source.clone(),
                    first_markdown_uuid: prior_md.clone(),
                    second_source: source_id.to_string(),
                    second_markdown_uuid: markdown_uuid.to_string(),
                });
            }
            self.rows.insert(
                row.uuid.clone(),
                (source_id.to_string(), markdown_uuid.to_string()),
            );
        }
        None
    }
}

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
    /// What render could not do while producing this document: records
    /// dropped, fields nulled, lossy rules that fired. Travels with the
    /// document so the rows and the record of what was lost commit together.
    /// Empty when read back from the store, where they are already rows.
    pub problems: Vec<datalib_schema::render_problems::RenderProblemRow>,
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

/// Stack every source's render store into the unified index — the
/// `grid_index` DAG step's whole job.
///
/// **Each source is asked what changed, not read whole**, via `dolt_diff`
/// between the commit `source_cursors` last consumed and the store's HEAD.
/// Two things fall out of that: a document a source stopped holding can be
/// named and deleted, and the cursor advances inside the write transaction,
/// so it can never claim more than the index holds.
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
    build_grid_index_for(pool, out_dir, &sources, progress, now_override).await
}

/// Stack the render stores of exactly `sources` into the index. The step
/// passes the groups its declared inputs name, so a source dropped from
/// the config stops being read on the next run even while its tree is
/// still on disk — and a directory that is not in the config is never
/// read at all. A listed source with no store yet is skipped: its render
/// step has not produced one.
pub async fn build_grid_index_for(
    pool: &SqlitePool,
    out_dir: &Path,
    sources: &[String],
    progress: impl Fn(&str),
    now_override: Option<&str>,
) -> Result<GridIndexSummary> {
    // The whole loop runs in one begin/commit batch: doltlite charges ~50ms
    // per auto-committed statement bundle, which is ruinous on a full rebuild.
    // An error rolls back, leaving the index exactly as it was.
    let write_lock = WriteLock::new(pool.clone());
    // Cursors and the per-source document lists load before the write
    // transaction opens, because the index pool is one connection wide.
    let cursors = load_source_cursors(pool).await?;
    let indexed = load_markdown_uuids_by_source(pool).await?;

    let mut docs: Vec<(String, RenderedMarkdown)> = Vec::new();
    // `source_id → (new_head, documents_applied)` for the cursors this
    // run will advance, and the ids each source dropped.
    let mut advanced: Vec<(String, String)> = Vec::new();
    let mut removed: Vec<(String, String)> = Vec::new();
    {
        let mut stanzas: Vec<(String, PathBuf)> = Vec::new();
        for source in sources {
            let rendered_root = out_dir
                .join(source)
                .join(datalib_etl::layout::RENDER_MARKDOWN_DIR);
            if crate::indexed_markdown::path_for(&rendered_root).is_file() {
                stanzas.push((source.clone(), rendered_root));
            } else {
                tracing::info!(source, "index: no render store yet; skipping it this run");
            }
        }
        stanzas.sort();
        stanzas.dedup();
        for (stanza, rendered_root) in stanzas {
            // Read-only: the render step owns this store, and an ordinary
            // open would rescue-commit and schema-commit into it — writing to
            // a file we do not own, and (once producers stream) committing
            // the renderer's in-flight rows on its behalf.
            // Pinned at open: the diff below and the rows behind it name
            // one commit, and the views exist before either query runs.
            let Some(store) = crate::indexed_markdown::IndexedMarkdownStore::open_for_reading(
                &rendered_root,
                None,
            )
            .with_context(|| format!("open render store for {stanza}"))?
            else {
                tracing::warn!(
                    source = %stanza,
                    "index: this store names no commit, so there is nothing \
                     committed to index; skipping it this run"
                );
                continue;
            };
            let pin = store.pin().expect("a reader is pinned at open").clone();
            let cursor = cursors.get(&stanza).map(String::as_str);
            let scan = store
                .changed_since(cursor, &pin)
                .with_context(|| format!("diff render store for {stanza}"))?;
            // Say which path was taken, every time: a cold start that fires
            // silently on every run looks exactly like a fast one from the
            // outside — it just does more work and still gets the right answer.
            match (&scan.render, cursor) {
                (None, None) => tracing::info!(
                    source = %stanza,
                    "index: no cursor for this source; reading its whole store"
                ),
                (None, Some(from)) => tracing::warn!(
                    source = %stanza,
                    from,
                    "index: cursor unusable against this store (reset, rebuilt, or no \
                     dolt_diff); falling back to reading it whole"
                ),
                (Some(changed), _) => tracing::info!(
                    source = %stanza,
                    changed = changed.len(),
                    scan_ms = scan.scan_elapsed.map(|d| d.as_millis() as u64),
                    "index: documents changed since the last index"
                ),
            }
            let found = store
                .documents_matching(out_dir, scan.render.as_ref(), &pin)
                .with_context(|| format!("read documents from {stanza}"))?;
            let present: HashSet<&str> = found.iter().map(|d| d.markdown_uuid.as_str()).collect();
            match &scan.render {
                // An id the diff named that the store no longer has is a
                // deletion.
                Some(changed) => {
                    for gone in changed.iter().filter(|u| !present.contains(u.as_str())) {
                        removed.push((stanza.clone(), gone.clone()));
                    }
                }
                // The store was read whole, so it is the complete answer:
                // a document the index holds for this source that the
                // store does not is one the source no longer produces. A
                // committed store with no rows is an honest "nothing";
                // the store that could not be read was skipped above.
                None => {
                    for gone in indexed
                        .get(&stanza)
                        .into_iter()
                        .flatten()
                        .filter(|u| !present.contains(u.as_str()))
                    {
                        removed.push((stanza.clone(), gone.clone()));
                    }
                }
            }
            store.close();
            // Only advance a cursor when we know the HEAD we consumed.
            // `new_head: None` means `dolt_log()` did not answer, and an
            // unwritten cursor cold-starts the next run — the safe direction.
            if let Some(head) = scan.new_head {
                advanced.push((stanza.clone(), head));
            }
            docs.extend(found.into_iter().map(|d| (stanza.clone(), d)));
        }
    }

    let mut summary = GridIndexSummary {
        markdowns_total: docs.len(),
        ..Default::default()
    };

    write_lock
        .begin_transaction()
        .await
        .context("WriteLock::begin_transaction for build_grid_index")?;
    let res = async {
        for (stanza, gone) in &removed {
            delete_markdown(&write_lock, gone)
                .await
                .with_context(|| format!("delete {gone} dropped by {stanza}"))?;
            summary.markdowns_removed += 1;
        }
        load_all_batch(&write_lock, out_dir, &docs, &progress, &mut summary).await?;
        // Cursors last and in the same transaction: a failure above rolls
        // back to both the old rows and the old cursors.
        let now = run_stamp(now_override);
        let mut guard = write_lock.acquire().await?;
        let conn = guard.conn();
        for (source_id, store_commit) in &advanced {
            write_source_cursor(
                conn,
                &SourceCursorRow {
                    source_id: source_id.clone(),
                    store_commit: store_commit.clone(),
                    indexed_at_utc: now.utc.clone(),
                    tz_offset: now.tz_offset.clone(),
                    documents_applied: summary.markdowns_loaded as i64,
                },
            )
            .await?;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    match res {
        Ok(()) => {
            write_lock
                .commit_transaction()
                .await
                .context("WriteLock::commit_transaction for build_grid_index")?;
            Ok(summary)
        }
        Err(e) => {
            // Best effort — the held connection rolls back on drop anyway.
            let _ = write_lock.rollback_transaction().await;
            Err(e)
        }
    }
}

/// The per-document loop of [`build_grid_index`], separated so the caller
/// can wrap it in one transaction.
async fn load_all_batch(
    write_lock: &WriteLock,
    out_dir: &Path,
    docs: &[(String, RenderedMarkdown)],
    progress: &impl Fn(&str),
    summary: &mut GridIndexSummary,
) -> Result<()> {
    // See [`IdClaims`]: catches two sources writing the same id.
    let mut claims = IdClaims::new();
    for (stanza, md) in docs {
        // The stanza dir name is the source's id.
        let source_id = if stanza.is_empty() {
            md.rows
                .first()
                .map(|r| r.provider.clone())
                .unwrap_or_default()
        } else {
            stanza.clone()
        };

        if let Some(collision) = claims.claim(&source_id, &md.markdown_uuid, &md.rows) {
            return Err(anyhow::anyhow!("{collision}"))
                .with_context(|| format!("load {} from {stanza}", md.markdown_uuid));
        }
        // Every document the diff named is applied. One whose rows come
        // out identical writes identical rows, and doltlite's tables are
        // content-addressed: the next commit carries no diff for it.
        // The stanza name is authoritative. Everything else comes through
        // from the store unchanged.
        let md = RenderedMarkdown {
            source_id,
            // Already rows in the store; re-applying would double-count.
            problems: Vec::new(),
            ..md.clone()
        };
        let inserted = apply_one(write_lock, out_dir, &md)
            .await
            .with_context(|| format!("load {} from {stanza}", md.markdown_uuid))?;
        summary.rows_inserted += inserted;
        summary.markdowns_loaded += 1;
        progress(&format!(
            "loaded {}/{}",
            summary.markdowns_loaded, summary.markdowns_total
        ));
    }
    Ok(())
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

pub async fn load_source_cursors(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query("SELECT source_id, store_commit FROM source_cursors")
        .fetch_all(pool)
        .await
        .context("load_source_cursors")?;
    let mut out: HashMap<String, String> = HashMap::with_capacity(rows.len());
    for r in rows {
        out.insert(r.try_get("source_id")?, r.try_get("store_commit")?);
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
    delete_document_rows(guard.conn(), markdown_uuid).await
}

/// The rows a document owns: its grid rows, its outgoing edges and its
/// `markdowns` row. Not its `render_problems`, which say why a document
/// is the way it is and outlive one that ends with nothing.
pub(crate) async fn delete_document_rows(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    markdown_uuid: &str,
) -> Result<()> {
    for sql in [
        "DELETE FROM grid_rows WHERE markdown_uuid = ?",
        "DELETE FROM edges WHERE src_markdown_uuid = ?",
        "DELETE FROM markdowns WHERE markdown_uuid = ?",
    ] {
        sqlx::query(sql)
            .bind(markdown_uuid)
            .execute(&mut **conn)
            .await
            .with_context(|| format!("delete {markdown_uuid} from the index"))?;
    }
    Ok(())
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
        delete_document_rows(conn, &md.markdown_uuid).await?;
        tracing::info!(
            document = %md.markdown_uuid,
            "render: this document has no rows left; dropped it",
        );
        return Ok(0);
    }
    let canonical = document_row(&md.rows, &md.markdown_uuid)?;

    sqlx::query("DELETE FROM grid_rows WHERE markdown_uuid = ?")
        .bind(&md.markdown_uuid)
        .execute(&mut **conn)
        .await
        .context("delete prior rows")?;

    for row in &md.rows {
        insert_grid_row(conn, write_lock, row).await?;
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

    upsert_markdown(conn, md, canonical, qmd_path)
        .await
        .context("upsert markdowns")?;
    write_lock.note_written(&md.markdown_uuid);

    // The grid_index step issues one dolt_commit per run after the whole
    // load; per-doc commits would drown dolt_log.
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
          md_path, upstream_cursor, renderer_version, bucket_key) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&md.markdown_uuid)
    .bind(&source_id)
    .bind(&canonical.provider)
    .bind(kind)
    .bind(&canonical.conversation_name)
    .bind(canonical.created_at.as_deref())
    .bind(canonical.modified_at.as_deref())
    .bind(qmd_path)
    .bind(md.upstream_cursor.as_deref())
    .bind(&version_str)
    .bind(&md.bucket_key)
    .execute(&mut **conn)
    .await
    .context("insert markdowns row")?;
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

/// Insert one row, and settle a `PRIMARY KEY (uuid)` collision by *who*
/// wrote the row's current owner. A document an earlier run wrote still
/// holding this uuid means the row has moved — a message re-bucketed
/// into another period, a chat whose document id is minted from a name
/// that changed — and the incoming document takes it over; the old
/// owner, when it re-renders, no longer emits it. A document *this* run
/// wrote still holding it means two documents minted one id, which is a
/// finding and fails. A plain upsert would hide the second behind the
/// first.
async fn insert_grid_row(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    write_lock: &WriteLock,
    row: &GridRow,
) -> Result<()> {
    let sql = datalib_etl::bulk::insert_sql::<GridRow>();
    // Audited: `sql` comes from `GridRow`'s associated consts; values bound.
    let res = row
        .bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
        .execute(&mut **conn)
        .await;
    let Err(e) = res else {
        return Ok(());
    };

    // The bare sqlx error names the constraint but not the row already
    // there, which is the only thing that says which document holds it
    // and when that document was rendered.
    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT provider, IFNULL(markdown_uuid, '') FROM grid_rows WHERE uuid = ? LIMIT 1",
    )
    .bind(&row.uuid)
    .fetch_optional(&mut **conn)
    .await
    .ok()
    .flatten();
    let Some((provider, md)) = existing else {
        return Err(anyhow::Error::new(e)).with_context(|| format!("insert grid_row {}", row.uuid));
    };
    let moved = provider == row.provider
        && md != row.markdown_uuid.as_deref().unwrap_or("")
        && !write_lock.written_this_run(&md);
    if !moved {
        return Err(anyhow::Error::new(e)).with_context(|| {
            format!(
                "insert grid_row {}: an existing {provider} row already holds that \
                 uuid (markdown {md}, written this run); the incoming row is a {} \
                 from markdown {}",
                row.uuid,
                row.provider,
                row.markdown_uuid.as_deref().unwrap_or("<none>"),
            )
        });
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
    row.bind_into(sqlx::query(sqlx::AssertSqlSafe(
        datalib_etl::bulk::insert_sql::<GridRow>(),
    )))
    .execute(&mut **conn)
    .await
    .with_context(|| format!("insert moved grid_row {}", row.uuid))?;
    Ok(())
}

#[cfg(test)]
mod open_index_tests {
    use super::*;

    /// A `grid_index` pass that died after its SQL `COMMIT` and before its
    /// `dolt_commit` leaves the batch in the working set. The next
    /// `open_index` seals it into a rescue commit — so the applet, which
    /// reads at HEAD, sees those rows — rather than a bare pool folding
    /// them into the next pass's commit unremarked.
    #[tokio::test]
    async fn rows_a_killed_pass_left_uncommitted_are_rescued_by_the_next_open() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("db.doltlite_db");
        let pool = open_index(&path).await.expect("open_index");
        if !datalib_etl::doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        sqlx::query(
            "INSERT INTO markdowns (markdown_uuid, source_id, provider, kind, md_path, \
             renderer_version) VALUES ('m-1', 'src', 'claude', 'chat', 'x.md', 'v')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let pool = open_index(&path).await.expect("reopen");
        let messages: Vec<String> = sqlx::query_scalar("SELECT message FROM dolt_log()")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(
            messages.iter().any(|m| m.starts_with("rescue:")),
            "no rescue commit: {messages:?}"
        );
        let head = datalib_etl::pin::head(&pool).await.unwrap().unwrap();
        // Audited: the hash is `Pin::at`-checked and the table is a literal.
        let at_head: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM dolt_at_markdowns('{}')",
            head.commit()
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(at_head, 1, "the orphaned row is committed now");
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
    use datalib_schema::grid_rows::GridRow;
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
            is_document: true,
            author: Some("Jean-Luc Picard".into()),
            account: Some("acct-1701".into()),
            project: Some("proj-1701".into()),
            org_uuid: Some("org-1701".into()),
            org_name: Some("Starfleet".into()),
            channel: Some("bridge".into()),
            conversation_name: Some("Klingon Diplomatic Greeting".into()),
            conversation_uuid: "conv-1701".into(),
            message_index: Some(3),
            entire_chat: "/chat/conv-1701".into(),
            text: "Tea. Earl Grey. Hot.".into(),
            slack_link: Some("https://example.test/archives/C1/p1".into()),
            qmd_path: Some("chats/conv-1701.md".into()),
            source_url: Some("https://claude.ai/chat/conv-1701".into()),
            git_sha: Some("0123456789abcdef".into()),
            upstream_id: Some("upstream-1701".into()),
            upstream_entity_kind: Some("conversation".into()),
            upstream_scope: Some("claude.ai".into()),
            notion_page_uuid: Some("notion-page-1701".into()),
            notion_block_uuid: Some("notion-block-1701".into()),
            markdown_uuid: Some("md-1701".into()),
            byte_size: Some(4_096),
            item_count: Some(17),
            diff_status: Some("modified".into()),
            diff_changed_columns: Some("text|author".into()),
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
            .connect_with(opts)
            .await
            .expect("open pool");
        init_schema(&pool).await.expect("init_schema");

        let row = fully_populated_row();
        let mut conn = pool.acquire().await.expect("acquire");
        let lock = WriteLock::new(pool.clone());
        insert_grid_row(&mut conn, &lock, &row)
            .await
            .expect("insert_grid_row");
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
             declared on GridRow (or derived in insert_grid_row) but \
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
mod id_claim_tests {
    //! [`IdClaims`] is the tripwire for two configured sources minting the
    //! same id. A full overlap used to be silent (the second document erased
    //! the first's rows and the run reported success); a partial overlap blew
    //! the batch up with an error naming neither source. These pin both.
    use super::*;
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::providers::Provider;

    fn row(uuid: &str, markdown_uuid: &str) -> GridRow {
        GridRow {
            uuid: uuid.into(),
            provider: Provider::Claude.as_str().into(),
            kind: "Chat".into(),
            source_label: "Claude".into(),
            created_at: None,
            modified_at: None,
            // The claims these tests make are about uuids, not documents.
            is_document: false,
            author: None,
            account: None,
            project: None,
            org_uuid: None,
            org_name: None,
            channel: None,
            conversation_name: None,
            conversation_uuid: markdown_uuid.into(),
            message_index: None,
            entire_chat: format!("/chat/{markdown_uuid}"),
            text: String::new(),
            slack_link: None,
            qmd_path: None,
            source_url: None,
            git_sha: None,
            upstream_id: None,
            upstream_entity_kind: None,
            upstream_scope: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            markdown_uuid: Some(markdown_uuid.into()),
            byte_size: None,
            item_count: None,
            diff_status: None,
            diff_changed_columns: None,
        }
    }

    #[test]
    fn distinct_sources_with_distinct_ids_are_clean() {
        let mut claims = IdClaims::new();
        assert!(claims
            .claim(
                "claude-api",
                "md-a",
                &[row("r1", "md-a"), row("r2", "md-a")]
            )
            .is_none());
        assert!(claims
            .claim("slack-work", "md-b", &[row("r3", "md-b")])
            .is_none());
    }

    /// The silent case: `claude_api` and `claude_export` over one
    /// account both key on Anthropic's `conversation_uuid`, so both
    /// documents carry the same `markdown_uuid`. Whichever applied
    /// second used to delete the other's rows and rewrite `md_path`
    /// and `source_id` to its own — no error, no row-count delta.
    #[test]
    fn same_markdown_uuid_from_two_sources_is_reported() {
        let mut claims = IdClaims::new();
        assert!(claims
            .claim("claude-api", "conv-1", &[row("r1", "conv-1")])
            .is_none());
        let hit = claims
            .claim("claude-export", "conv-1", &[row("r1", "conv-1")])
            .expect("overlapping markdown_uuid must be reported");
        assert_eq!(hit.id_kind, "markdown_uuid");
        assert_eq!(hit.id, "conv-1");
        assert_eq!(hit.first_source, "claude-api");
        assert_eq!(hit.second_source, "claude-export");
        // Naming both sides is the whole point — it is the only thing that
        // tells an operator which two stanzas to look at.
        let msg = hit.to_string();
        assert!(msg.contains("claude-api"), "{msg}");
        assert!(msg.contains("claude-export"), "{msg}");
    }

    /// The loud-but-useless case: two sources whose documents differ but
    /// whose *rows* collide. This used to surface as a bare sqlx PRIMARY KEY
    /// error deep inside a rolled-back batch.
    #[test]
    fn same_row_uuid_under_different_markdowns_is_reported() {
        let mut claims = IdClaims::new();
        assert!(claims
            .claim("papers", "md-a", &[row("doc-blake3", "md-a")])
            .is_none());
        let hit = claims
            .claim("archive", "md-b", &[row("doc-blake3", "md-b")])
            .expect("overlapping row uuid must be reported");
        assert_eq!(hit.id_kind, "grid_rows.uuid");
        assert_eq!(hit.id, "doc-blake3");
        assert_eq!(hit.first_source, "papers");
        assert_eq!(hit.first_markdown_uuid, "md-a");
        assert_eq!(hit.second_source, "archive");
        assert_eq!(hit.second_markdown_uuid, "md-b");
    }

    /// Changing a source's id must stay legal: the same document and row
    /// ids arrive under a new `source_id`, one claimant per id within the
    /// run. Run-scoping the tracker is precisely what keeps this working.
    #[test]
    fn a_source_that_changed_id_reclaiming_its_own_ids_is_clean() {
        let mut first_run = IdClaims::new();
        assert!(first_run
            .claim("slack", "md-a", &[row("r1", "md-a")])
            .is_none());

        let mut second_run = IdClaims::new();
        assert!(second_run
            .claim("slack-work", "md-a", &[row("r1", "md-a")])
            .is_none());
    }
}

#[cfg(test)]
// Test diagnostics; cargo test captures and prints them per-test.
#[allow(clippy::disallowed_macros)]
mod write_lock_tests {
    //! Reproduces the production "(code 5) database is locked": several
    //! per-source render workers calling [`apply_one`] in parallel against one
    //! pool with `max_connections > 1`. Without the [`WriteLock`] each task
    //! gets its own connection, all race for doltlite's file-level write lock,
    //! and the losers time out. No artificial sleeps — the contention is real,
    //! from the same code path production uses.
    use super::*;
    use datalib_schema::grid_rows::GridRow;
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
            is_document: true,
            author: None,
            account: Some("acct-test".into()),
            project: None,
            org_uuid: None,
            org_name: None,
            channel: None,
            conversation_name: Some(format!("Conv {uuid}")),
            conversation_uuid: uuid.clone(),
            message_index: None,
            entire_chat: format!("/chat/{uuid}"),
            text: format!("body for {uuid}"),
            slack_link: None,
            qmd_path: Some(format!("chats/{uuid}.md")),
            source_url: None,
            git_sha: None,
            upstream_id: None,
            upstream_entity_kind: None,
            upstream_scope: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            markdown_uuid: Some(uuid.clone()),
            byte_size: None,
            item_count: None,
            diff_status: None,
            diff_changed_columns: None,
        };
        RenderedMarkdown {
            markdown_uuid: uuid.clone(),
            source_id: "test".into(),
            upstream_cursor: None,
            bucket_key: None,
            md_path: PathBuf::from(format!("/tmp/{uuid}.md")),
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            edges: Vec::new(),
            problems: Vec::new(),
        }
    }

    async fn open_pool(db: &Path, max_conn: u32) -> SqlitePool {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))
            .unwrap()
            .create_if_missing(true);
        SqlitePoolOptions::new()
            .max_connections(max_conn)
            .connect_with(opts)
            .await
            .unwrap()
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
        let out_dir = PathBuf::from("/tmp");

        let mut handles = Vec::with_capacity(N_TASKS);
        for task in 0..N_TASKS {
            let lock = write_lock.clone();
            let out_dir = out_dir.clone();
            handles.push(tokio::spawn(async move {
                for idx in 0..PER_TASK {
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
        let out_dir = PathBuf::from("/tmp");

        // Every apply_one below reuses the held conn and accumulates into the
        // open transaction.
        write_lock.begin_transaction().await.expect("BEGIN");

        let mut handles = Vec::with_capacity(N_TASKS);
        for task in 0..N_TASKS {
            let lock = write_lock.clone();
            let out_dir = out_dir.clone();
            handles.push(tokio::spawn(async move {
                for idx in 0..PER_TASK {
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
mod schema_reconcile_tests {
    //! The index must survive a schema change to `grid_rows`, `markdowns` or
    //! `edges` without a human deleting the file.
    //!
    //! Both directions fail silently. A reconcile that doesn't fire leaves
    //! every statement naming a new column erroring against an older root.
    //! One that fires when it shouldn't wipes a healthy index on every run,
    //! and since the rebuild repopulates it, the only symptom is slowness.

    use std::path::Path;
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
    use tempfile::tempdir;

    use crate::grid_index::{init_schema, EDGES_DDL, MARKDOWNS_DDL};
    use datalib_etl::doltlite_raw::actual_column_names;

    /// `grid_rows` exactly as data roots created before #216 have it on disk.
    /// Written out longhand rather than derived from the current DDL: the
    /// point is to pin a shape from history, and a computed one would
    /// silently become today's shape again.
    const PRE_216_GRID_ROWS_DDL: &str = "CREATE TABLE IF NOT EXISTS grid_rows (
        uuid VARCHAR(96) NOT NULL,
        provider VARCHAR(32) NOT NULL,
        kind VARCHAR(32) NOT NULL,
        source_label VARCHAR(32) NOT NULL,
        when_ts VARCHAR(40),
        when_ts_utc VARCHAR(40),
        when_offset VARCHAR(8),
        author VARCHAR(255),
        account VARCHAR(96),
        project VARCHAR(96),
        org_uuid VARCHAR(96),
        org_name VARCHAR(255),
        channel VARCHAR(255),
        conversation_name TEXT,
        conversation_uuid VARCHAR(96) NOT NULL,
        message_index INT,
        entire_chat VARCHAR(255) NOT NULL,
        text LONGTEXT NOT NULL,
        slack_link VARCHAR(512),
        qmd_path VARCHAR(512),
        source_url VARCHAR(1024),
        git_sha VARCHAR(64),
        external_id VARCHAR(128),
        notion_page_uuid VARCHAR(96),
        notion_block_uuid VARCHAR(96),
        markdown_uuid VARCHAR(96),
        PRIMARY KEY (uuid)
    )";

    async fn open_pool(db: &Path) -> SqlitePool {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))
            .unwrap()
            .create_if_missing(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap()
    }

    async fn count(pool: &SqlitePool, table: &str) -> i64 {
        // Test helper; `table` is a literal at every callsite.
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn seed_pre_216(pool: &SqlitePool) {
        sqlx::query(PRE_216_GRID_ROWS_DDL)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(MARKDOWNS_DDL).execute(pool).await.unwrap();
        for (_table, ddl) in EDGES_DDL {
            sqlx::query(*ddl).execute(pool).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO markdowns (markdown_uuid, source_id, provider, kind) \
             VALUES ('md-1', 'claude_web', 'claude', 'Chat')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO grid_rows (uuid, provider, kind, source_label, conversation_uuid, \
             entire_chat, text, external_id, markdown_uuid) \
             VALUES ('row-1', 'claude', 'Chat', 'Claude', 'conv-1', '/chat/md-1', 'hi', \
             'upstream-1', 'md-1')",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// An index written before #216 is brought to the current schema, and its
    /// `markdowns` rows are cleared so the rebuild actually runs.
    ///
    /// The `markdowns` assertion is the load-bearing half: recreating
    /// `grid_rows` alone satisfies every "does the column exist" check while
    /// leaving the cursors in place, and `build_grid_index` reads nothing
    /// from a store its cursor already covers — so the index would stay
    /// empty until something upstream changed.
    #[tokio::test]
    async fn an_index_predating_a_column_rename_is_rebuilt() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("grid.doltlite_db")).await;
        seed_pre_216(&pool).await;

        init_schema(&pool).await.expect("init_schema");

        let cols = actual_column_names(&pool, "grid_rows").await.unwrap();
        for added in ["upstream_id", "upstream_entity_kind", "upstream_scope"] {
            assert!(cols.contains(added), "grid_rows must have gained {added}");
        }
        assert!(
            !cols.contains("external_id"),
            "the column upstream_id replaced must be gone"
        );
        assert_eq!(
            count(&pool, "markdowns").await,
            0,
            "markdowns must be cleared, or build_grid_index reads nothing \
             and the rebuilt index stays empty"
        );
        assert_eq!(count(&pool, "grid_rows").await, 0);
    }

    /// The write path works afterwards. `no such column: upstream_id` from
    /// inside `insert_grid_row` is what actually failed on a real data root,
    /// so asserting the column list alone would leave that untested.
    #[tokio::test]
    async fn the_rebuilt_index_accepts_a_write() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("grid.doltlite_db")).await;
        seed_pre_216(&pool).await;
        init_schema(&pool).await.expect("init_schema");

        sqlx::query(
            "INSERT INTO grid_rows (uuid, provider, kind, source_label, conversation_uuid, \
             entire_chat, text, upstream_id, upstream_entity_kind, upstream_scope, markdown_uuid, \
             is_document) \
             VALUES ('row-2', 'claude', 'Chat', 'Claude', 'conv-1', '/chat/md-1', 'hi', \
             'upstream-1', 'conversation', '', 'md-1', 1)",
        )
        .execute(&pool)
        .await
        .expect("insert naming the post-#216 columns must succeed");
    }

    /// An index already at the current schema is left completely alone. A
    /// reconcile whose comparison is subtly wrong would rebuild on every run,
    /// and nothing downstream would notice — the only symptom is a pipeline
    /// that quietly stopped being incremental.
    #[tokio::test]
    async fn a_current_index_is_not_touched() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("grid.doltlite_db")).await;
        init_schema(&pool).await.expect("first init_schema");

        sqlx::query(
            "INSERT INTO markdowns (markdown_uuid, source_id, provider, kind) \
             VALUES ('md-1', 'claude_web', 'claude', 'Chat')",
        )
        .execute(&pool)
        .await
        .unwrap();

        init_schema(&pool).await.expect("second init_schema");

        assert_eq!(
            count(&pool, "markdowns").await,
            1,
            "a matching schema must not be rebuilt; the rows that make the \
             index incremental would be thrown away on every run"
        );
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
        build_grid_index, build_grid_index_for, discover_sources, init_schema, load_source_cursors,
        RenderedMarkdown,
    };
    use crate::indexed_markdown::IndexedMarkdownStore;
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::providers::Provider;

    async fn index_pool(root: &Path) -> SqlitePool {
        let db = root.join("unified_index/grid/db.doltlite_db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))
                    .unwrap()
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        init_schema(&pool).await.unwrap();
        pool
    }

    fn doc(root: &Path, source: &str, uuid: &str, text: &str) -> RenderedMarkdown {
        let row = GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Test)
            .kind("Test")
            .source_label("Test")
            .conversation_uuid(uuid)
            .entire_chat(format!("/chat/{uuid}"))
            .text(text)
            .markdown_uuid(Some(uuid.to_string()))
            .created_at(Some("2026-01-01T00:00:00+00:00".to_string()))
            .is_document(true)
            .build()
            .unwrap();
        RenderedMarkdown {
            markdown_uuid: uuid.to_string(),
            source_id: source.to_string(),
            // Fingerprint follows the text, the way a renderer's does.
            upstream_cursor: None,
            bucket_key: None,
            md_path: rendered_root(root, source).join(format!("{uuid}.md")),
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            edges: Vec::new(),
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
        build_grid_index_for(&pool, root, &listed, |_| {}, None)
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
        let head = store.changed_since(None, &pin).unwrap().new_head;
        store.close();
        assert_eq!(Some(recorded), head, "the cursor is the store's HEAD");
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

        let text: String = sqlx::query_scalar("SELECT text FROM grid_rows WHERE uuid = 'md-2'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(text, "b-changed");
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

        // A hash from no history anyone has.
        sqlx::query("UPDATE source_cursors SET store_commit = ? WHERE source_id = 'src'")
            .bind("0123456789abcdef0123456789abcdef")
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
}
