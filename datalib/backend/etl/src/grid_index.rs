//! The unified grid index: one table stacked from every source's render
//! store.
//!
//! Two entry points, which are the same write path from two sides.
//! [`apply_one`] writes one rendered document; [`build_grid_index`] stacks
//! every source's store into the unified index, which is the `grid_index` DAG
//! step's whole job.
//!
//! Writes are delete-then-insert, gated on `markdowns.source_fingerprint`, so
//! a re-render replaces a document's rows rather than accumulating them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bulk::BulkUpsertable;
use anyhow::{Context, Result};
use datalib_schema::edges::{EdgeRow, DDL as EDGES_DDL};
use datalib_schema::grid_rows::{GridRow, DDL as GRID_ROWS_DDL};
use datalib_schema::markdowns::DDL as MARKDOWNS_TABLE_DDL;
use datalib_schema::source_cursors::{SourceCursorRow, DDL as SOURCE_CURSORS_DDL};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tokio::sync::Mutex;

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
        }
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
/// `source_fingerprint` is the renderer's input hash, compared on later runs
/// to decide whether to re-render.
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
    pub markdowns_skipped: usize,
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
/// opposite of [`crate::doltlite_raw::open`]'s policy, because every row here
/// is a pure function of a row in a source's render store, so a rebuild costs
/// one local scan. It is also the only answer that yields correct values:
/// `ADD COLUMN` leaves existing rows NULL, and the fingerprint skip then makes
/// those NULLs permanent.
///
/// All three tables go together even when only one drifted, because
/// `markdowns` holds the fingerprints that drive that skip.
pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
    for ddl in index_ddl() {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .with_context(|| format!("create {}", table_of(ddl)))?;
    }
    reconcile_index_schema(pool).await
}

/// The table a DDL statement creates, for error messages. Degrades to the
/// raw SQL rather than panicking.
fn table_of(ddl: &str) -> String {
    crate::doltlite_raw::parse_create_table_name(ddl).unwrap_or_else(|| ddl.to_string())
}

/// Drop and recreate every index table if any one of them disagrees with
/// its DDL. See [`init_schema`] for why it is all-or-nothing.
async fn reconcile_index_schema(pool: &SqlitePool) -> Result<()> {
    let mut drift: Vec<String> = Vec::new();
    for ddl in index_ddl() {
        let Some(table) = crate::doltlite_raw::parse_create_table_name(ddl) else {
            continue;
        };
        let declared = crate::doltlite_raw::declared_column_names(ddl, &table)
            .await
            .with_context(|| format!("declared columns for {table}"))?;
        let actual = crate::doltlite_raw::actual_column_names(pool, &table)
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
        let Some(table) = crate::doltlite_raw::parse_create_table_name(ddl) else {
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

/// Bump when the canonical-tuple shape in `compute_row_set_hash` or the
/// rendered `.md` layout changes: every `documents.row_set_hash` is
/// invalidated and the next ingest re-renders.
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
/// source *rename*, which is legitimate. Claims are recorded before the
/// fingerprint skip, so an overlap is caught even when one sidecar is
/// unchanged.
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
        source_name: &str,
        markdown_uuid: &str,
        rows: &[GridRow],
    ) -> Option<IdCollision> {
        if let Some(prior) = self.markdowns.get(markdown_uuid) {
            return Some(IdCollision {
                id_kind: "markdown_uuid",
                id: markdown_uuid.to_string(),
                first_source: prior.clone(),
                first_markdown_uuid: markdown_uuid.to_string(),
                second_source: source_name.to_string(),
                second_markdown_uuid: markdown_uuid.to_string(),
            });
        }
        self.markdowns
            .insert(markdown_uuid.to_string(), source_name.to_string());

        for row in rows {
            if let Some((prior_source, prior_md)) = self.rows.get(&row.uuid) {
                return Some(IdCollision {
                    id_kind: "grid_rows.uuid",
                    id: row.uuid.clone(),
                    first_source: prior_source.clone(),
                    first_markdown_uuid: prior_md.clone(),
                    second_source: source_name.to_string(),
                    second_markdown_uuid: markdown_uuid.to_string(),
                });
            }
            self.rows.insert(
                row.uuid.clone(),
                (source_name.to_string(), markdown_uuid.to_string()),
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
        _ => "chat",
    }
}

/// SHA-256 over the canonical per-row tuple, sorted by `(when_ts, uuid)` so
/// the hash is independent of producer order. The encoding is
/// length-prefixed and `\0`-delimited, so it is stable across Rust versions
/// in a way `Debug` is not.
pub fn compute_row_set_hash(rows: &[GridRow]) -> String {
    let mut sorted: Vec<&GridRow> = rows.iter().collect();
    sorted.sort_by(|a, b| a.when_ts.cmp(&b.when_ts).then_with(|| a.uuid.cmp(&b.uuid)));
    let mut h = Sha256::new();
    let push = |h: &mut Sha256, v: Option<&str>| {
        match v {
            Some(s) => {
                h.update(b"S");
                h.update((s.len() as u64).to_le_bytes());
                h.update(s.as_bytes());
            }
            None => h.update(b"N"),
        }
        h.update(b"\x00");
    };
    let push_i = |h: &mut Sha256, v: Option<i64>| {
        match v {
            Some(n) => {
                h.update(b"I");
                h.update(n.to_le_bytes());
            }
            None => h.update(b"N"),
        }
        h.update(b"\x00");
    };
    for r in sorted {
        push(&mut h, Some(&r.uuid));
        push(&mut h, Some(&r.kind));
        push(&mut h, r.when_ts.as_deref());
        push(&mut h, r.author.as_deref());
        push_i(&mut h, r.message_index);
        push(&mut h, Some(&r.text));
        push(&mut h, r.source_url.as_deref());
        push(&mut h, r.slack_link.as_deref());
        push(&mut h, r.git_sha.as_deref());
        push(&mut h, r.upstream_id.as_deref());
        push(&mut h, r.upstream_entity_kind.as_deref());
        push(&mut h, r.upstream_scope.as_deref());
        push(&mut h, r.notion_page_uuid.as_deref());
        push(&mut h, r.notion_block_uuid.as_deref());
    }
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// One markdown's payload as handed from render to the indexer,
/// constructed once the md and its blobs are durably on disk so that render
/// and index commit per-document atomically.
#[derive(Debug, Clone)]
pub struct RenderedMarkdown {
    pub markdown_uuid: String,
    /// User-facing config name (e.g. `tiny-slack`), falling back to the
    /// provider string.
    pub source_name: String,
    pub source_fingerprint: String,
    /// A cheap probe the orchestrator can check *before* loading payloads to
    /// decide whether a markdown moved. Slack stamps each thread's
    /// `MAX(fetched_at)`. None when the provider has nothing cheaper than the
    /// fingerprint.
    pub upstream_cursor: Option<String>,
    /// Absolute path to the rendered `.md`; `qmd_path` is this with the
    /// out-dir prefix stripped.
    pub md_path: PathBuf,
    pub render_version: u32,
    pub rows: Vec<GridRow>,
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

/// Write one rendered document into the index unconditionally. The caller
/// has already applied the fingerprint skip. `out_dir` is stripped off
/// `md_path` to produce a portable `qmd_path`.
pub async fn apply_one(
    write_lock: &WriteLock,
    out_dir: &Path,
    md: &RenderedMarkdown,
    now_override: Option<&str>,
) -> Result<usize> {
    let qmd_rel = md
        .md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md.md_path)
        .to_string_lossy()
        .to_string();
    apply_markdown(write_lock, md, &qmd_rel, now_override).await
}

/// Stack every source's render store into the unified index — the
/// `grid_index` DAG step's whole job.
///
/// **Each source is asked what changed, not read whole**, via `dolt_diff`
/// between the commit `source_cursors` last consumed and the store's HEAD.
/// Two things fall out of that: a document a source stopped holding can be
/// named and deleted, and the cursor advances inside the write transaction,
/// so it can never claim more than the index holds.
///
/// The fingerprint skip is still not redundant — the cold path reads whole
/// stores, and the skip is what makes that cheap.
pub async fn build_grid_index(
    pool: &SqlitePool,
    out_dir: &Path,
    progress: impl Fn(&str),
    now_override: Option<&str>,
) -> Result<GridIndexSummary> {
    // The whole loop runs in one begin/commit batch: doltlite charges ~50ms
    // per auto-committed statement bundle, which is ruinous on a full rebuild.
    // An error rolls back, leaving the index exactly as it was.
    let write_lock = WriteLock::new(pool.clone());
    // One dir per source plus the reserved `system/`; the directory name IS
    // the config-level source name. Cursors load before the write transaction
    // opens, because the index pool is one connection wide.
    let cursors = load_source_cursors(pool).await?;

    let mut docs: Vec<(String, RenderedMarkdown)> = Vec::new();
    // `source_name → (new_head, documents_applied)` for the cursors this
    // run will advance, and the ids each source dropped.
    let mut advanced: Vec<(String, String)> = Vec::new();
    let mut removed: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = fs::read_dir(out_dir) {
        let mut stanzas: Vec<(String, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            if entry.file_name() == datalib_core::layout::SYSTEM_DIR {
                continue;
            }
            let stanza = entry.file_name().to_string_lossy().into_owned();
            let rendered_root = entry.path().join("rendered_md");
            if crate::indexed_markdown::path_for(&rendered_root).is_file() {
                stanzas.push((stanza, rendered_root));
            }
        }
        stanzas.sort();
        for (stanza, rendered_root) in stanzas {
            // Read-only: the render step owns this store, and an ordinary
            // open would rescue-commit and schema-commit into it — writing to
            // a file we do not own, and (once producers stream) committing
            // the renderer's in-flight rows on its behalf.
            let store =
                crate::indexed_markdown::IndexedMarkdownStore::open_for_reading(&rendered_root)
                    .with_context(|| format!("open render store for {stanza}"))?;
            let cursor = cursors.get(&stanza).map(String::as_str);
            let scan = store
                .changed_since(cursor)
                .with_context(|| format!("diff render store for {stanza}"))?;
            // Say which path was taken, every time: a cold start that fires
            // silently on every run looks exactly like a fast one from the
            // outside — it just does more work and still gets the right answer.
            match (&scan.changed_buckets, cursor) {
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
            // Read at the commit the scan named, so the changed set and the
            // rows behind it describe one commit. No commit means this store
            // has nothing committed to read — an interrupted first render, or
            // a build with no dolt extensions — and reading it anyway would
            // mean indexing rows the renderer had not finished writing.
            let Some(pin) = crate::pin::Pin::from_scan(scan.new_head.as_deref())
                .with_context(|| format!("pin the render store for {stanza}"))?
            else {
                tracing::warn!(
                    source = %stanza,
                    "index: this store names no commit, so there is nothing \
                     committed to index; skipping it this run"
                );
                store.close();
                continue;
            };
            let found = store
                .documents_matching(out_dir, scan.changed_buckets.as_ref(), &pin)
                .with_context(|| format!("read documents from {stanza}"))?;
            // An id the diff named that the store no longer has is a deletion.
            // Only a diff can produce this.
            if let Some(changed) = &scan.changed_buckets {
                let present: HashSet<&str> =
                    found.iter().map(|d| d.markdown_uuid.as_str()).collect();
                for gone in changed.iter().filter(|u| !present.contains(u.as_str())) {
                    removed.push((stanza.clone(), gone.clone()));
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

    // Loaded before the write transaction opens: the index pool is one
    // connection wide, so a read while the transaction holds that connection
    // would deadlock.
    let prior_fingerprints = load_fingerprints(pool).await?;

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
        load_all_batch(
            &write_lock,
            &prior_fingerprints,
            out_dir,
            &docs,
            &progress,
            now_override,
            &mut summary,
        )
        .await?;
        // Cursors last and in the same transaction: a failure above rolls
        // back to both the old rows and the old cursors.
        let now = now_override
            .map(str::to_string)
            .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339());
        let mut guard = write_lock.acquire().await?;
        let conn = guard.conn();
        for (source_name, store_commit) in &advanced {
            write_source_cursor(
                conn,
                &SourceCursorRow {
                    source_name: source_name.clone(),
                    store_commit: store_commit.clone(),
                    indexed_at: now.clone(),
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
    prior_fingerprints: &HashMap<String, String>,
    out_dir: &Path,
    docs: &[(String, RenderedMarkdown)],
    progress: &impl Fn(&str),
    now_override: Option<&str>,
    summary: &mut GridIndexSummary,
) -> Result<()> {
    // See [`IdClaims`]: catches two sources writing the same id.
    let mut claims = IdClaims::new();
    for (stanza, md) in docs {
        // The stanza dir name is the config-level source name.
        let source_name = if stanza.is_empty() {
            md.rows
                .first()
                .map(|r| r.provider.clone())
                .unwrap_or_default()
        } else {
            stanza.clone()
        };

        // Claim ids BEFORE the fingerprint skip, so an overlap between two
        // sources is still caught on a steady-state re-run where one of them
        // is unchanged and would never be looked at.
        if let Some(collision) = claims.claim(&source_name, &md.markdown_uuid, &md.rows) {
            return Err(anyhow::anyhow!("{collision}"))
                .with_context(|| format!("load {} from {stanza}", md.markdown_uuid));
        }

        if prior_fingerprints.get(&md.markdown_uuid) == Some(&md.source_fingerprint) {
            summary.markdowns_skipped += 1;
            continue;
        }
        // The stanza name is authoritative. Everything else comes through
        // from the store unchanged.
        let md = RenderedMarkdown {
            source_name,
            // Already rows in the store; re-applying would double-count.
            problems: Vec::new(),
            ..md.clone()
        };
        let inserted = apply_one(write_lock, out_dir, &md, now_override)
            .await
            .with_context(|| format!("load {} from {stanza}", md.markdown_uuid))?;
        summary.rows_inserted += inserted;
        summary.markdowns_loaded += 1;
        progress(&format!(
            "loaded {}/{}",
            summary.markdowns_loaded + summary.markdowns_skipped,
            summary.markdowns_total
        ));
    }
    Ok(())
}

/// Bulk fingerprint snapshot, read once per sync into the map every
/// renderer consults at per-markdown skip time. NULL fingerprints are
/// omitted, so the caller treats them as "not rendered".
pub async fn load_fingerprints(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query(
        "SELECT markdown_uuid, source_fingerprint \
         FROM markdowns WHERE source_fingerprint IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .context("load_fingerprints")?;
    let mut out: HashMap<String, String> = HashMap::with_capacity(rows.len());
    for r in rows {
        let uuid: String = r.try_get("markdown_uuid")?;
        let fp: String = r.try_get("source_fingerprint")?;
        out.insert(uuid, fp);
    }
    Ok(out)
}

pub async fn load_source_cursors(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query("SELECT source_name, store_commit FROM source_cursors")
        .fetch_all(pool)
        .await
        .context("load_source_cursors")?;
    let mut out: HashMap<String, String> = HashMap::with_capacity(rows.len());
    for r in rows {
        out.insert(r.try_get("source_name")?, r.try_get("store_commit")?);
    }
    Ok(out)
}

async fn write_source_cursor(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    row: &SourceCursorRow,
) -> Result<()> {
    sqlx::query("DELETE FROM source_cursors WHERE source_name = ?")
        .bind(&row.source_name)
        .execute(&mut **conn)
        .await
        .context("clear prior source cursor")?;
    let sql = crate::bulk::insert_sql::<SourceCursorRow>();
    // Audited: `sql` is built from `SourceCursorRow`'s associated consts.
    row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
        .execute(&mut **conn)
        .await
        .with_context(|| format!("write source cursor {}", row.source_name))?;
    Ok(())
}

/// Remove a document and everything hanging off it: the three deletes
/// `apply_markdown` runs before re-inserting, without the insert. Only
/// reachable because the index diffs rather than re-reads — a deleted
/// conversation used to stay in the grid forever.
pub async fn delete_markdown(write_lock: &WriteLock, markdown_uuid: &str) -> Result<()> {
    let mut guard = write_lock.acquire().await?;
    let conn = guard.conn();
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
    now_override: Option<&str>,
) -> Result<usize> {
    // Inside `begin_transaction` every guard hands back the same connection,
    // so the per-doc statements accumulate in one batch; otherwise each takes
    // a fresh connection and auto-commits.
    let mut guard = write_lock.acquire().await?;
    let conn = guard.conn();

    sqlx::query("DELETE FROM grid_rows WHERE markdown_uuid = ?")
        .bind(&md.markdown_uuid)
        .execute(&mut **conn)
        .await
        .context("delete prior rows")?;

    for row in &md.rows {
        insert_grid_row(conn, row).await?;
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

    let rendered_at = now_override
        .map(str::to_string)
        .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339());
    upsert_markdown(conn, md, qmd_path, &rendered_at)
        .await
        .context("upsert markdowns")?;

    // The grid_index step issues one dolt_commit per run after the whole
    // load; per-doc commits would drown dolt_log.
    Ok(md.rows.len())
}

/// The row whose `uuid` matches `markdown_uuid` — the chat/thread/PR/page
/// row — falling back to the first row.
fn pick_canonical<'a>(rows: &'a [GridRow], markdown_uuid: &str) -> Option<&'a GridRow> {
    rows.iter()
        .find(|r| r.uuid == markdown_uuid)
        .or_else(|| rows.first())
}

async fn upsert_markdown(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    md: &RenderedMarkdown,
    qmd_path: &str,
    rendered_at: &str,
) -> Result<()> {
    let Some(canonical) = pick_canonical(&md.rows, &md.markdown_uuid) else {
        return Ok(());
    };
    let kind = doc_kind_for(&canonical.kind);
    let timestamps: Vec<&str> = md
        .rows
        .iter()
        .filter_map(|r| r.when_ts.as_deref())
        .collect();
    let created_at = timestamps.iter().min().copied();
    let updated_at = timestamps.iter().max().copied();
    let row_set_hash = compute_row_set_hash(&md.rows);
    let version_str = format!("{RENDERER_VERSION}.{}", md.render_version);
    // Fall back to the canonical row's provider when build_grid_index
    // rebuilds from disk without the config-level name.
    let source_name = if md.source_name.is_empty() {
        canonical.provider.clone()
    } else {
        md.source_name.clone()
    };

    sqlx::query("DELETE FROM markdowns WHERE markdown_uuid = ?")
        .bind(&md.markdown_uuid)
        .execute(&mut **conn)
        .await
        .context("delete prior markdowns row")?;
    sqlx::query(
        "INSERT INTO markdowns \
         (markdown_uuid, source_name, provider, kind, title, created_at, updated_at, \
          md_path, source_fingerprint, upstream_cursor, row_set_hash, renderer_version, rendered_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&md.markdown_uuid)
    .bind(&source_name)
    .bind(&canonical.provider)
    .bind(kind)
    .bind(&canonical.conversation_name)
    .bind(created_at)
    .bind(updated_at)
    .bind(qmd_path)
    .bind(&md.source_fingerprint)
    .bind(md.upstream_cursor.as_deref())
    .bind(&row_set_hash)
    .bind(&version_str)
    .bind(rendered_at)
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

async fn insert_grid_row(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    row: &GridRow,
) -> Result<()> {
    // A plain INSERT, not the bulk upsert: a `PRIMARY KEY (uuid)` collision
    // here is a finding, not an update — see the error arm below.
    // `ON CONFLICT DO UPDATE` would silently overwrite it.
    let sql = crate::bulk::insert_sql::<GridRow>();
    // Audited: `sql` comes from `GridRow`'s associated consts; values bound.
    let res = row
        .bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
        .execute(&mut **conn)
        .await;

    if let Err(e) = res {
        // Almost always `PRIMARY KEY (uuid)`. The bare sqlx error names the
        // constraint but not the row already there, which is the only thing
        // that says which other document minted this id.
        let existing: Option<(String, String)> = sqlx::query_as(
            "SELECT provider, IFNULL(markdown_uuid, '') FROM grid_rows WHERE uuid = ? LIMIT 1",
        )
        .bind(&row.uuid)
        .fetch_optional(&mut **conn)
        .await
        .ok()
        .flatten();
        return match existing {
            Some((provider, md)) => Err(anyhow::Error::new(e)).with_context(|| {
                format!(
                    "insert grid_row {}: an existing {provider} row already holds that \
                     uuid (markdown {md}); the incoming row is a {} from markdown {}",
                    row.uuid,
                    row.provider,
                    row.markdown_uuid.as_deref().unwrap_or("<none>"),
                )
            }),
            None => {
                Err(anyhow::Error::new(e)).with_context(|| format!("insert grid_row {}", row.uuid))
            }
        };
    }
    Ok(())
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
            when_ts: Some("2026-06-02T13:00:00-07:00".into()),
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
        insert_grid_row(&mut conn, &row)
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
            when_ts: None,
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
    /// and `source_name` to its own — no error, no row-count delta.
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

    /// A source *rename* must stay legal: same ids, different `source_name`,
    /// one claimant per id within the run. Run-scoping the tracker is
    /// precisely what keeps this working.
    #[test]
    fn a_renamed_source_reclaiming_its_own_ids_is_clean() {
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
            when_ts: Some("2026-06-02T20:00:00+00:00".into()),
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
        };
        RenderedMarkdown {
            markdown_uuid: uuid.clone(),
            source_name: "test".into(),
            source_fingerprint: format!("fp-{uuid}"),
            upstream_cursor: None,
            md_path: PathBuf::from(format!("/tmp/{uuid}.md")),
            render_version: 1,
            rows: vec![row],
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
                    apply_one(lock.as_ref(), &out_dir, &md, None)
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
                    apply_one(lock.as_ref(), &out_dir, &md, None)
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
            apply_one(&lock, &out_dir, &mk_md(0, idx), None)
                .await
                .unwrap();
        }
        lock.rollback_transaction().await.unwrap();

        let grid_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grid_rows")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(grid_n, 0, "ROLLBACK must leave grid_rows untouched");
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

    use crate::doltlite_raw::actual_column_names;
    use crate::grid_index::{init_schema, EDGES_DDL, MARKDOWNS_DDL};

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
            "INSERT INTO markdowns (markdown_uuid, source_name, provider, kind, source_fingerprint) \
             VALUES ('md-1', 'claude_web', 'claude', 'Chat', 'fp-1')",
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
    /// fingerprints are cleared so the rebuild actually runs.
    ///
    /// The `markdowns` assertion is the load-bearing half: recreating
    /// `grid_rows` alone satisfies every "does the column exist" check while
    /// leaving the fingerprints in place, and `build_grid_index` skips a
    /// document whose fingerprint still matches — so the index would stay
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
            "fingerprints must be cleared, or build_grid_index skips every \
             document and the rebuilt index stays empty"
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
             entire_chat, text, upstream_id, upstream_entity_kind, upstream_scope, markdown_uuid) \
             VALUES ('row-2', 'claude', 'Chat', 'Claude', 'conv-1', '/chat/md-1', 'hi', \
             'upstream-1', 'conversation', '', 'md-1')",
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
            "INSERT INTO markdowns (markdown_uuid, source_name, provider, kind, source_fingerprint) \
             VALUES ('md-1', 'claude_web', 'claude', 'Chat', 'fp-1')",
        )
        .execute(&pool)
        .await
        .unwrap();

        init_schema(&pool).await.expect("second init_schema");

        assert_eq!(
            count(&pool, "markdowns").await,
            1,
            "a matching schema must not be rebuilt; the fingerprints that make \
             the index incremental would be thrown away on every run"
        );
    }
}

#[cfg(test)]
mod source_cursor_tests {
    //! What the cursor buys, and the trap in testing it.
    //!
    //! Before the cursor, a steady-state re-index still *read* every document
    //! and dropped the unchanged ones by fingerprint. Nothing was written
    //! either way, so "nothing was loaded" proves nothing. `markdowns_total`
    //! — documents actually read — is the field that separates the two, and
    //! every test here asserts on it.

    use std::path::Path;
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
    use tempfile::tempdir;

    use crate::grid_index::{build_grid_index, init_schema, load_source_cursors, RenderedMarkdown};
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
            .when_ts(Some("2026-01-01T00:00:00+00:00".to_string()))
            .build()
            .unwrap();
        RenderedMarkdown {
            markdown_uuid: uuid.to_string(),
            source_name: source.to_string(),
            // Fingerprint follows the text, the way a renderer's does.
            source_fingerprint: format!("fp-{text}"),
            upstream_cursor: None,
            md_path: root
                .join(source)
                .join("rendered_md")
                .join(format!("{uuid}.md")),
            render_version: 1,
            rows: vec![row],
            edges: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn rendered_root(root: &Path, source: &str) -> std::path::PathBuf {
        root.join(source).join("rendered_md")
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

    /// A document that belongs to a conversation other than itself, and
    /// whose `.md` actually exists on disk — the shape a periodizing
    /// renderer produces, and the one the deletion path has to handle.
    fn doc_in_conversation(
        root: &Path,
        source: &str,
        uuid: &str,
        conversation_uuid: &str,
    ) -> RenderedMarkdown {
        let mut md = doc(root, source, uuid, "body");
        md.rows[0].conversation_uuid = conversation_uuid.to_string();
        std::fs::create_dir_all(md.md_path.parent().unwrap()).unwrap();
        std::fs::write(&md.md_path, "# rendered\n").unwrap();
        md
    }

    /// One conversation, several rendered documents, all of them gone when
    /// the conversation is.
    ///
    /// The fan-out is the reason `documents_for_conversation` exists rather
    /// than the renderer just naming the document it wants dropped: slack,
    /// signal and beeper split one conversation across periods, and once
    /// the conversation is gone from the raw store nothing but this store
    /// still knows how many periods it had. A removal keyed on the
    /// conversation drops all of them; one keyed on a recomputed document
    /// id would drop whichever period the renderer guessed and silently
    /// leave the rest.
    #[tokio::test(flavor = "multi_thread")]
    async fn removing_a_conversation_takes_every_period_it_rendered_into() {
        let td = tempdir().unwrap();
        let root = td.path();
        let pool = index_pool(root).await;
        let conv = "conv-1";
        let jan = doc_in_conversation(root, "src", "md-jan", conv);
        let feb = doc_in_conversation(root, "src", "md-feb", conv);
        let other = doc_in_conversation(root, "src", "md-other", "conv-2");
        let (jan_md, feb_md, other_md) = (
            jan.md_path.clone(),
            feb.md_path.clone(),
            other.md_path.clone(),
        );
        render(root, "src", &[jan, feb, other]);
        build_grid_index(&pool, root, |_| {}, None).await.unwrap();
        assert_eq!(index_row_count(&pool).await, 3);

        let store = IndexedMarkdownStore::open(&rendered_root(root, "src")).unwrap();
        let mut gone = store.documents_for_conversation(conv).unwrap();
        gone.sort();
        assert_eq!(
            gone,
            vec!["md-feb".to_string(), "md-jan".to_string()],
            "both of the conversation's periods, and only those"
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
             reading the store and comparing fingerprints"
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

        let store = IndexedMarkdownStore::open(&rendered_root(root, "src")).unwrap();
        let head = store.changed_since(None).unwrap().new_head;
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
        sqlx::query("UPDATE source_cursors SET store_commit = ? WHERE source_name = 'src'")
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
}
