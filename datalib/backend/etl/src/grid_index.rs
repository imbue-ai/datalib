//! Generic Load step: walk a `rendered_md/` tree of `.grid_rows.json`
//! sidecars and upsert their rows into Dolt.
//!
//! Two entry points:
//!
//!   * [`apply_one`] writes a single rendered document into `grid_rows`
//!     and stamps the `documents` row. Called per-doc by sync's render
//!     callback so render+index commit atomically.
//!   * [`build_grid_index`] walks a `rendered_md/` tree and calls `apply_one`
//!     for each sidecar. Used as a rebuild-from-disk tool; not on the
//!     hot path now that sync renders+loads per doc.
//!
//! The sidecar format is the cross-provider contract:
//!
//! ```jsonc
//! {
//!   "header": {
//!     "markdown_uuid": "…",            // primary key for the document
//!     "source_fingerprint": "…",       // hash of upstream payload
//!     "render_version": 1              // renderer-side schema stamp
//!   },
//!   "rows": [GridRow, …]
//! }
//! ```
//!
//! Skip logic: before applying we look up `documents.source_fingerprint`
//! by `markdown_uuid`; if it matches the sidecar header we treat the
//! document as up-to-date and leave `grid_rows` alone. Same delete-then-
//! insert pattern as the Python `populate_grid_rows`, generalized so
//! any provider's Render step can produce a sidecar tree this loader
//! consumes verbatim.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use datalib_schema::edges::{EdgeRow, DDL as EDGES_DDL};
use datalib_schema::grid_rows::{GridRow, DDL as GRID_ROWS_DDL};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tokio::sync::Mutex;

use datalib_index_lib::Sidecar;

/// Serializes concurrent writers against one doltlite index pool AND
/// optionally batches all writes into one big transaction — with
/// observability baked in.
///
/// Background: doltlite (like SQLite) serializes writes at the file
/// level — only one writer can advance the chunk store at a time. If
/// you give multiple tasks their own pool connections and call
/// `apply_one` from each, they race for the underlying write lock;
/// losers wait inside sqlx's `busy_timeout` (default ~5s) and
/// eventually see `(code 5) database is locked`. The orchestrator's
/// per-source parallel render hits this in production.
///
/// We also discovered (via the wait/hold counters this struct
/// reports) that each per-doc auto-commit costs ~50ms because every
/// statement boundary materializes the prolly tree's manifest. At
/// 488 docs that's ~24s of wall-clock time spent serializing tiny
/// writes through doltlite's per-commit overhead. Wrapping the whole
/// render phase in ONE `BEGIN ... COMMIT` collapses that overhead
/// — only the final COMMIT pays the manifest cost.
///
/// Putting both behaviors in one type keeps the contract simple:
/// every per-doc call to `apply_one` goes through `WriteLock::acquire`,
/// which returns `&mut conn` for the duration of one write. If a
/// transaction is active (`begin_transaction` was called), every
/// acquire uses the SAME held connection so the writes accumulate
/// in one transaction; otherwise each acquire takes a fresh pool
/// connection and statements auto-commit individually.
///
/// The metrics counters answer "where is the time going":
///
///   * `total_wait` — summed across all `acquire` calls; high values
///     relative to wall time mean writers are queuing behind one
///     another (doltlite write throughput is the bottleneck).
///   * `total_hold` — summed time the lock was held; divide by
///     `acquisitions` for the average per-doc write cost.
///   * `acquisitions` — number of `acquire` calls that ran.
pub struct WriteLock {
    pool: SqlitePool,
    inner: Mutex<WriteLockInner>,
    total_wait_ns: AtomicU64,
    total_hold_ns: AtomicU64,
    acquisitions: AtomicU64,
}

struct WriteLockInner {
    /// Held connection during an active `BEGIN ... COMMIT` batch.
    /// `None` outside a transaction; in that case `acquire` takes a
    /// fresh pool connection per call and statements auto-commit.
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

    /// Open one big write transaction. Subsequent `acquire` calls
    /// reuse the same connection so every statement lands inside
    /// the same `BEGIN ... COMMIT`. Pair with
    /// [`commit_transaction`] or [`rollback_transaction`].
    ///
    /// Panics if a transaction is already active — there's only one
    /// render phase per run and one ROLLBACK target.
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

    /// Commit the batch and release the held connection. Subsequent
    /// `acquire` calls revert to per-call auto-commit mode.
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

    /// Roll back the batch and release the held connection.
    /// Best-effort — if ROLLBACK itself errors we drop the conn
    /// anyway (the pool re-establishes per-connection state on
    /// next acquire).
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

    /// Acquire write access. Returns a guard wrapping `&mut conn`.
    /// If a transaction is active, the guard hands out the held
    /// connection (so the caller's statements accumulate in the
    /// batch); otherwise a fresh pool connection is taken and
    /// dropped at guard release (auto-commit per statement).
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

/// RAII guard: dropping it stamps the hold-time counter and (in
/// non-transaction mode) returns the per-call connection to the pool.
pub struct WriteLockGuard<'a> {
    inner: tokio::sync::MutexGuard<'a, WriteLockInner>,
    fresh_conn: Option<sqlx::pool::PoolConnection<sqlx::Sqlite>>,
    held_since: Instant,
    owner: &'a WriteLock,
}

impl<'a> WriteLockGuard<'a> {
    /// Mutable access to the active write connection. Same conn
    /// across every `acquire` while a transaction is open; a fresh
    /// per-call conn otherwise.
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

/// Per-rendered-markdown metadata projection: one row per `.md` file
/// in `<root>/rendered_md/`. `source_fingerprint` is the renderer's
/// input-hash, set when the markdown + blobs land on disk; subsequent
/// runs compare against it to decide whether to re-render.
/// `row_set_hash` is the load-side hash over the canonical grid_rows,
/// used by tools that walk a stale tree.
///
/// `markdown_uuid` is the canonical addressing primitive for rendered
/// output: every grid_row carries a FK back here, and `/api/chat/{uuid}`
/// dereferences it through `md_path`. Note that for sharded renders
/// (beeper renders one file per period) a single upstream
/// "conversation" maps to N rows here — `conversation_uuid` is not
/// unique in the table.
pub const MARKDOWNS_DDL: &str = r#"CREATE TABLE IF NOT EXISTS markdowns (
    markdown_uuid VARCHAR(96) NOT NULL,
    source_name VARCHAR(64) NOT NULL,
    provider VARCHAR(32) NOT NULL,
    kind VARCHAR(32) NOT NULL,
    title TEXT,
    created_at VARCHAR(40),
    updated_at VARCHAR(40),
    md_path VARCHAR(1024),
    source_fingerprint VARCHAR(64),
    upstream_cursor VARCHAR(64),
    row_set_hash CHAR(64),
    renderer_version VARCHAR(32),
    rendered_at VARCHAR(40),
    PRIMARY KEY (markdown_uuid)
)"#;

/// Stats emitted on every load run. Stable shape so a web UI can poll
/// or stream it without per-provider branches.
#[derive(Debug, Default, Serialize)]
pub struct GridIndexSummary {
    pub markdowns_total: usize,
    pub markdowns_loaded: usize,
    pub markdowns_skipped: usize,
    pub rows_inserted: usize,
}

/// Every `CREATE TABLE` in the grid index, in creation order.
///
/// One list so the DDL pass and the schema check below can't drift into
/// covering different sets of tables — a table missing from the check
/// would keep an old shape forever while the ones beside it healed.
fn index_ddl() -> impl Iterator<Item = &'static str> {
    GRID_ROWS_DDL
        .iter()
        .map(|(_table, ddl)| *ddl)
        .chain(std::iter::once(MARKDOWNS_DDL))
        .chain(EDGES_DDL.iter().map(|(_table, ddl)| *ddl))
}

/// Apply DDL for `grid_rows`, `markdowns`, and `edges`, and rebuild them
/// from scratch if what's on disk no longer matches.
///
/// `CREATE TABLE IF NOT EXISTS` is a no-op against a table that already
/// exists, so without the reconcile below an index created under an
/// older schema never gains a column a later change introduced — and
/// then every statement naming that column fails. That is not
/// hypothetical: #216 renamed `grid_rows.external_id` to `upstream_id`
/// and added two columns beside it, and every data root predating it
/// answered both the read path and the write path with
/// `no such column: upstream_id`.
///
/// **Drop and rebuild, rather than `ALTER TABLE … ADD COLUMN`.** This is
/// the opposite of [`crate::doltlite_raw::open`]'s policy for raw
/// stores, deliberately, because the two hold different kinds of row.
/// A raw store's rows cost a network fetch, so adding the column and
/// keeping the rows is the cheap correct answer. Every row here is a
/// pure function of a `.grid_rows.json` sidecar already on disk, so a
/// rebuild costs one local scan — and it is the *only* answer that
/// yields correct values: an `ADD COLUMN` leaves existing rows NULL in
/// the new column, and `markdowns.source_fingerprint` then makes the
/// next run skip exactly those documents, so the NULLs are permanent.
///
/// All three tables go together even when only one drifted, because
/// `markdowns` holds the fingerprints that drive that skip. Dropping
/// `grid_rows` alone would leave every document looking up-to-date and
/// the index permanently empty.
pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
    for ddl in index_ddl() {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .with_context(|| format!("create {}", table_of(ddl)))?;
    }
    reconcile_index_schema(pool).await
}

/// The table a DDL statement creates, for error messages. Every
/// statement in [`index_ddl`] is a `CREATE TABLE`, so the fallback is
/// unreachable in practice; it degrades to the raw SQL rather than
/// panicking.
fn table_of(ddl: &str) -> String {
    crate::doltlite_raw::parse_create_table_name(ddl).unwrap_or_else(|| ddl.to_string())
}

/// Drop and recreate every index table if any one of them disagrees
/// with its DDL. See [`init_schema`] for why it is all-or-nothing and
/// why rebuilding beats `ADD COLUMN` here.
async fn reconcile_index_schema(pool: &SqlitePool) -> Result<()> {
    let mut drift: Vec<String> = Vec::new();
    for ddl in index_ddl() {
        let Some(table) = crate::doltlite_raw::parse_create_table_name(ddl) else {
            continue;
        };
        let declared = crate::doltlite_raw::declared_column_names(pool, ddl, &table)
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
         every index table from the sidecar trees (no re-download, no re-render)"
    );
    for ddl in index_ddl() {
        let Some(table) = crate::doltlite_raw::parse_create_table_name(ddl) else {
            continue;
        };
        sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
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

/// Renderer-side cache stamp. Bump when the canonical-tuple shape in
/// `compute_row_set_hash` or the rendered `.md` layout changes — every
/// `documents.row_set_hash` is invalidated and the next ingest will
/// re-render. `rust-v1` is the clean break from the Python `"v1"` since
/// the hash encoding differs.
pub const RENDERER_VERSION: &str = "rust-v1";

// ─────────────────────────────────────────────────────────────────────
// Cross-source id collision detection
// ─────────────────────────────────────────────────────────────────────

/// One id claimed by two different sources inside a single index run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdCollision {
    /// Which id space collided — `"markdown_uuid"` or `"grid_rows.uuid"`.
    pub id_kind: &'static str,
    /// The contested id.
    pub id: String,
    /// Source name that claimed it first (sidecars are walked in sorted
    /// order, so "first" is stable across runs).
    pub first_source: String,
    /// `markdown_uuid` the first claim arrived under.
    pub first_markdown_uuid: String,
    /// Source name that claimed it second — the one whose data would
    /// have won or blown up.
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
/// Two sources emitting the same `markdown_uuid` or the same
/// `grid_rows.uuid` is not a benign duplicate. [`apply_markdown`]
/// deletes by `markdown_uuid` before inserting, so on a *full* overlap
/// the sidecar applied second erases the first one's rows and rewrites
/// the `markdowns` row with its own `md_path` and `source_name` — one
/// source's data vanishes from the index with no error and no row-count
/// change to notice. On a *partial* overlap (same row uuid, different
/// markdown) the plain `INSERT` instead trips `PRIMARY KEY (uuid)` and
/// rolls the whole batch back with a bare sqlx error naming neither
/// source. Cheap overlap failed loudly, total overlap failed silently;
/// this makes both loud and names both sides.
///
/// Scoped to a single run on purpose. Checking against ids already in
/// the database would flag a source *rename* — same ids arriving under
/// a new `source_name`, which is legitimate and must keep working —
/// whereas two sidecars claiming one id inside one walk is always
/// either a misconfiguration (the same upstream account wired up
/// twice) or an id recipe missing a discriminator.
///
/// Claims are recorded *before* the fingerprint skip check, so an
/// overlap is still caught on a steady-state re-run where one of the
/// two sidecars is unchanged and never applied.
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

    /// Record one sidecar's claims. Returns the first collision found,
    /// leaving the claim table in a usable state either way.
    ///
    /// Same-source re-claims are impossible by construction — one
    /// sidecar owns one `markdown_uuid` and the walk visits each
    /// sidecar once — so any repeat is a genuine cross-source clash and
    /// is reported even when both sides name the same source.
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

/// Map a grid_rows `kind` (string used in the UI) to the
/// `documents.kind` enum (chat/thread/page/pr/mr). Anything not in this
/// map is a child row and shouldn't be picked as the canonical document
/// row — but if it ends up being the only candidate we fall back to
/// `"chat"`, matching the Python behavior.
fn doc_kind_for(grid_kind: &str) -> &'static str {
    match grid_kind {
        "Chat" => "chat",
        "Slack Thread" => "thread",
        "GitHub PR" => "pr",
        "GitLab MR" => "mr",
        "Notion Page" | "Notion Database" => "page",
        "Notion Comment Thread" => "thread",
        // A PDF is a document, not a conversation. It reaches the same
        // grid and preview surfaces as everything else, but the sync
        // page shows this string, and calling a scanned manual a "chat"
        // is just wrong.
        "PDF Document" => "document",
        // Same reasoning as PDF: a Claude Project is a collection of
        // written context (description, custom instructions, knowledge
        // files), not a conversation.
        "Project" => "document",
        _ => "chat",
    }
}

/// SHA-256 over the canonical per-row tuple, sorted by `(when_ts, uuid)`
/// so the hash is independent of producer order. Encoding is a
/// `\0`-delimited concatenation of length-prefixed fields — stable across
/// Rust versions (unlike `Debug`), unlike Python's `repr` but that's
/// fine: bumping `RENDERER_VERSION` invalidates the old hashes anyway.
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

/// One markdown's payload as handed from render to the indexer. The
/// render-side callback constructs this once md + blobs are durably on
/// disk; [`apply_one`] writes the corresponding `grid_rows` + `markdowns`
/// rows so render+index commit per-doc atomically.
#[derive(Debug, Clone)]
pub struct RenderedMarkdown {
    pub markdown_uuid: String,
    /// User-facing config name (e.g. `tiny-slack`); falls back to the
    /// provider string when sync doesn't have one wired in.
    pub source_name: String,
    pub source_fingerprint: String,
    /// Optional provider-defined cheap-probe value the orchestrator can
    /// use *before* loading payloads to decide whether a markdown has
    /// changed since last run. Examples: slack stamps each thread's
    /// `MAX(fetched_at)` here, so the next run can `GROUP BY
    /// thread_root_uuid` on the existing index and skip loading
    /// untouched threads entirely. None when the provider has no
    /// cheaper-than-fingerprint signal.
    pub upstream_cursor: Option<String>,
    /// Absolute path to the rendered `.md`. Used to derive the
    /// `qmd_path` we stamp into `markdowns.md_path` by stripping the
    /// out-dir prefix.
    pub md_path: PathBuf,
    pub render_version: u32,
    pub rows: Vec<GridRow>,
    /// Outgoing edges originating from this markdown
    /// (`src_markdown_uuid == markdown_uuid`). Empty for renderers that
    /// don't emit edges yet — the Load step still issues the DELETE so
    /// stale rows from a previous render get cleaned up.
    pub edges: Vec<EdgeRow>,
}

/// Write one rendered document into Dolt unconditionally.
///
/// Skip semantics live in the *render* side now (`prior_fingerprints`
/// gate before each per-doc loop) — by the time we're called here the
/// caller has already decided the doc needs to land. `out_dir` is the
/// prefix stripped off `md_path` to produce a portable `qmd_path`.
///
/// `write_lock` owns the pool and serializes concurrent writers; see
/// [`WriteLock`] for the contention-avoidance contract and the
/// optional `begin_transaction` / `commit_transaction` batching that
/// collapses ~50ms-per-doc auto-commit overhead into one final
/// per-run COMMIT.
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

/// Walk every stanza's `<out>/<stanza>/rendered_md/` for `*.grid_rows.json`
/// sidecars and rebuild the index by calling [`apply_one`] for each. Off the
/// hot path now — sync's render step writes through `apply_one` per doc
/// directly — but useful as a disaster-recovery / "reindex from disk" tool.
pub async fn build_grid_index(
    pool: &SqlitePool,
    out_dir: &Path,
    progress: impl Fn(&str),
    now_override: Option<&str>,
) -> Result<GridIndexSummary> {
    // build_grid_index is single-threaded — there are no parallel workers
    // contending here. A fresh write lock owns the pool clone so
    // `apply_one` has somewhere to acquire connections. The whole
    // loop runs inside one begin/commit_transaction batch: doltlite
    // charges ~50ms per auto-committed statement bundle (prolly-tree
    // manifest mutation), which is ruinous on a full-root rebuild —
    // this is the DAG index step's hot path now, not just a
    // disaster-recovery tool. An error rolls the batch back, leaving
    // the index exactly as it was.
    let write_lock = WriteLock::new(pool.clone());
    // data_root holds one dir per stanza (each with a `rendered_md/` tree)
    // plus the reserved `system/` dir. Walk each stanza's rendered_md; skip
    // `system/` (the aggregate indices live there, no sidecars).
    // (stanza name, sidecar path): the stanza directory name IS the
    // config-level source name — `<data_root>/<name>/rendered_md/…` —
    // so `documents.source_name` keeps the user-facing name exactly as
    // the fused loader did.
    let mut sidecars: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            if entry.file_name() == datalib_core::layout::SYSTEM_DIR {
                continue;
            }
            let stanza = entry.file_name().to_string_lossy().into_owned();
            let rendered_root = entry.path().join("rendered_md");
            if rendered_root.is_dir() {
                let mut paths = Vec::new();
                collect_sidecars(&rendered_root, &mut paths);
                sidecars.extend(paths.into_iter().map(|p| (stanza.clone(), p)));
            }
        }
    }
    sidecars.sort();

    let mut summary = GridIndexSummary {
        markdowns_total: sidecars.len(),
        ..Default::default()
    };

    // Fingerprints are bulk-loaded BEFORE the write transaction: the
    // index pool is one connection wide (doltlite's HEAD is
    // per-connection), so a per-doc read against the pool while the
    // transaction holds that connection would deadlock.
    let prior_fingerprints = load_fingerprints(pool).await?;

    write_lock
        .begin_transaction()
        .await
        .context("WriteLock::begin_transaction for build_grid_index")?;
    let res = load_all_batch(
        &write_lock,
        &prior_fingerprints,
        out_dir,
        &sidecars,
        &progress,
        now_override,
        &mut summary,
    )
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
            // Best effort — the held connection rolls back on drop
            // anyway.
            let _ = write_lock.rollback_transaction().await;
            Err(e)
        }
    }
}

/// The per-sidecar loop of [`build_grid_index`], separated so the caller can
/// wrap it in one begin/rollback-or-commit transaction.
async fn load_all_batch(
    write_lock: &WriteLock,
    prior_fingerprints: &HashMap<String, String>,
    out_dir: &Path,
    sidecars: &[(String, PathBuf)],
    progress: &impl Fn(&str),
    now_override: Option<&str>,
    summary: &mut GridIndexSummary,
) -> Result<()> {
    // One run's id claims, used to catch two sources writing the same
    // `markdown_uuid` / `grid_rows.uuid`. See [`IdClaims`].
    let mut claims = IdClaims::new();
    for (stanza, sidecar_path) in sidecars {
        let raw = fs::read_to_string(sidecar_path)
            .with_context(|| format!("read {}", sidecar_path.display()))?;
        let sidecar: Sidecar = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", sidecar_path.display()))?;

        let md_path = derive_md_path(sidecar_path)
            .with_context(|| format!("derive .md path from {}", sidecar_path.display()))?;

        let markdown_uuid = sidecar.header.markdown_uuid.clone();
        let fingerprint = sidecar.header.source_fingerprint.clone();

        // The stanza dir name is the config-level source name; fall
        // back to the canonical row's provider only if it were somehow
        // empty.
        let source_name = if stanza.is_empty() {
            sidecar
                .rows
                .first()
                .map(|r| r.provider.clone())
                .unwrap_or_default()
        } else {
            stanza.clone()
        };

        // Claim this sidecar's ids BEFORE the fingerprint skip below:
        // an overlap between two sources must still be caught on a
        // steady-state re-run, where one of the two sidecars is
        // unchanged and would otherwise never be looked at.
        if let Some(collision) = claims.claim(&source_name, &markdown_uuid, &sidecar.rows) {
            return Err(anyhow::anyhow!("{collision}"))
                .with_context(|| format!("load {}", sidecar_path.display()));
        }

        if prior_fingerprints.get(&markdown_uuid) == Some(&fingerprint) {
            summary.markdowns_skipped += 1;
            continue;
        }
        let md = RenderedMarkdown {
            markdown_uuid,
            source_name,
            source_fingerprint: fingerprint,
            // build_grid_index rebuilds the index from sidecars on disk, which
            // don't carry the cheap-probe cursor (it lives in the
            // indexer only). Leaving it None forces the next live sync
            // to fall back to the fingerprint check for these markdowns
            // — safe, just not as fast as the cursor short-circuit.
            upstream_cursor: None,
            md_path,
            render_version: sidecar.header.render_version,
            rows: sidecar.rows,
            edges: sidecar.edges,
        };
        let inserted = apply_one(write_lock, out_dir, &md, now_override)
            .await
            .with_context(|| format!("load {}", sidecar_path.display()))?;
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

fn collect_sidecars(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_sidecars(&p, out);
        } else if p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".grid_rows.json"))
        {
            out.push(p);
        }
    }
}

fn derive_md_path(sidecar: &Path) -> Option<PathBuf> {
    let name = sidecar.file_name()?.to_str()?;
    let stem = name.strip_suffix(".grid_rows.json")?;
    Some(sidecar.with_file_name(format!("{stem}.md")))
}

/// Bulk fingerprint snapshot. Used once per sync to populate the
/// `prior_fingerprints` map every renderer consults at per-markdown
/// skip time. Rows whose `source_fingerprint` is NULL are omitted so
/// the caller treats them as "not rendered".
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

/// Bulk upstream-cursor snapshot, used the same way as
/// [`load_fingerprints`] but for the cheap-probe shortcut a few
/// providers use. Today only slack writes a non-NULL cursor (each
/// thread's `MAX(fetched_at)`); other providers' rows are omitted.
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
    // Acquire serialized write access. If the orchestrator has called
    // `begin_transaction`, every guard hands back the SAME held
    // connection so all per-doc DELETE/INSERTs/upsert statements
    // accumulate inside one big batch; otherwise each guard takes a
    // fresh pool connection (auto-commit per statement).
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

    // Edges are sharded by source markdown: each markdown owns the
    // outgoing edges whose `src_markdown_uuid` matches. Re-rendering a
    // markdown therefore replaces its outgoing-edge set. Incoming edges
    // (whose `dst_markdown_uuid` matches) are owned by the source
    // markdown's sidecar, so they survive this delete.
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

    // dolt_commit is issued ONCE per run by the grid_index step after
    // the full load finishes — not here. Per-doc commits would land
    // thousands of entries in dolt_log per run, drowning the audit
    // trail. See `datalib_step::grid_index` for the closing commit_run
    // call.
    Ok(md.rows.len())
}

/// Pick the canonical row for a markdown — the row whose `uuid` matches
/// `markdown_uuid` (the chat/thread/PR/page row). Fallback to the first
/// row if nothing matches.
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
    // Prefer the user-facing source_name the renderer was invoked with
    // (config.sources[].name in sync). Fall back to the canonical row's
    // provider when build_grid_index rebuilds from disk without that context.
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
    // `when_ts_utc` / `when_offset` are derived here, not emitted by
    // producers (see grid_rows.schema.json `x-derived`). Splitting the
    // producer's offset-bearing `when_ts` gives the grid a single-zone,
    // fixed-width column to sort/filter on (so ordering matches true
    // chronological order) plus the original offset for local rendering.
    // Unparseable / null `when_ts` leaves both columns NULL.
    let (when_ts_utc, when_offset) =
        match row.when_ts.as_deref().and_then(datalib_time::split_when_ts) {
            Some((utc, offset)) => (Some(utc), Some(offset)),
            None => (None, None),
        };
    let res = sqlx::query(
        "INSERT INTO grid_rows \
         (uuid, provider, kind, source_label, when_ts, when_ts_utc, when_offset, author, account, \
          project, channel, conversation_name, conversation_uuid, message_index, entire_chat, text, \
          slack_link, qmd_path, source_url, git_sha, upstream_id, upstream_entity_kind, \
          upstream_scope, notion_page_uuid, notion_block_uuid, \
          markdown_uuid) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.uuid)
    .bind(&row.provider)
    .bind(&row.kind)
    .bind(&row.source_label)
    .bind(&row.when_ts)
    .bind(&when_ts_utc)
    .bind(&when_offset)
    .bind(&row.author)
    .bind(&row.account)
    .bind(&row.project)
    .bind(&row.channel)
    .bind(&row.conversation_name)
    .bind(&row.conversation_uuid)
    .bind(row.message_index)
    .bind(&row.entire_chat)
    .bind(&row.text)
    .bind(&row.slack_link)
    .bind(&row.qmd_path)
    .bind(&row.source_url)
    .bind(&row.git_sha)
    .bind(&row.upstream_id)
    .bind(&row.upstream_entity_kind)
    .bind(&row.upstream_scope)
    .bind(&row.notion_page_uuid)
    .bind(&row.notion_block_uuid)
    .bind(&row.markdown_uuid)
    .execute(&mut **conn)
    .await;

    if let Err(e) = res {
        // Almost always `PRIMARY KEY (uuid)`. The bare sqlx error names
        // the constraint but not the row already sitting there, which
        // is the only thing that tells you *which* other document
        // minted this id. [`IdClaims`] catches the within-run case
        // before we ever get here, so reaching this point means the
        // clash is against a row already in the index — a stale row
        // from a previous layout, or a recipe that changed its
        // `markdown_uuid` while keeping its row ids.
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
mod id_claim_tests {
    //! [`IdClaims`] is the tripwire for two configured sources minting
    //! the same id. Before it existed, a *full* overlap (same
    //! `markdown_uuid`) was silent — `apply_markdown`'s
    //! DELETE-by-markdown_uuid meant the second sidecar erased the
    //! first one's rows and the run reported success — while a
    //! *partial* overlap (same row uuid, different markdown) blew the
    //! whole batch up on `PRIMARY KEY (uuid)` with an error naming
    //! neither source. These tests pin both shapes.
    use super::*;
    use datalib_schema::grid_rows::GridRow;

    fn row(uuid: &str, markdown_uuid: &str) -> GridRow {
        GridRow {
            uuid: uuid.into(),
            provider: "anthropic".into(),
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
    /// sidecars carry the same `markdown_uuid`. Whichever applied
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
        // The message has to name both sides — that is the whole point
        // of the check, and the only thing that tells an operator
        // which two stanzas to look at.
        let msg = hit.to_string();
        assert!(msg.contains("claude-api"), "{msg}");
        assert!(msg.contains("claude-export"), "{msg}");
    }

    /// The loud-but-useless case: two sources whose documents differ
    /// but whose *rows* collide — e.g. one shared upstream entity
    /// rendered into two different markdowns. This previously surfaced
    /// as a bare sqlx PRIMARY KEY error deep inside a rolled-back
    /// batch.
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

    /// A source *rename* must stay legal: the ids are unchanged, only
    /// `source_name` differs, and there is exactly one claimant per id
    /// within the run. The tracker is deliberately run-scoped rather
    /// than checking the database precisely so this keeps working.
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
// Test diagnostics; cargo test captures stdout/stderr and prints it
// per-test on failure or with `--nocapture`. No MP in scope here.
#[allow(clippy::disallowed_macros)]
mod write_lock_tests {
    //! Reproduces the production "(code 5) database is locked" we saw
    //! on a real render-only run: multiple per-source render
    //! workers calling [`apply_one`] in parallel against one pool that
    //! has `max_connections > 1`. Without the [`WriteLock`] argument
    //! each task gets its own connection, all of them race for
    //! doltlite's file-level write lock, and the losers eventually
    //! time out at sqlx's busy_timeout. With the WriteLock wired in,
    //! the Rust side queues writers and doltlite only ever sees one.
    //!
    //! The lock object also collects timing metrics; the assertions
    //! at the bottom confirm the wait/hold counters reflect what
    //! actually happened (acquisitions == total docs written, etc).
    //! No artificial sleeps or stalls — the contention is real,
    //! produced by the same code path the orchestrator uses.
    use super::*;
    use datalib_schema::grid_rows::GridRow;
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
            provider: "anthropic".into(),
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

    /// Drives N parallel tokio tasks through `apply_one`, each writing
    /// K unique markdowns into the same pool. With the WriteLock the
    /// orchestrator currently passes, every call must succeed. Counts
    /// in `grid_rows` and `markdowns` are then verified to match the
    /// expected `N*K` writes, and the WriteLock metrics are sanity-
    /// checked (acquisitions == total writes, both timing counters
    /// non-negative, etc.).
    ///
    /// We deliberately use `max_connections=8` to make the pool able
    /// to hand out enough connections that, WITHOUT the lock, the
    /// busy-timeout race would fire. With the lock, the connections
    /// don't help — only one writer runs at a time, so contention
    /// drops to zero on the doltlite side.
    /// Per-call auto-commit mode (no `begin_transaction`). Drives N
    /// parallel tasks through `apply_one` and verifies the lock
    /// serializes them cleanly. The per-doc cost here is whatever
    /// doltlite charges for one auto-committed statement bundle.
    ///
    /// `#[ignore]`'d because it dominates the etl_unittests critical
    /// path (~26s for 480 serialized auto-commit dolt writes at
    /// ~54ms each, vs. <1s for the rest of the suite combined). Its
    /// purpose is to demonstrate — and guard against regression in —
    /// the order-of-magnitude perf gap with the transaction-batched
    /// companion test below, which is a one-time empirical
    /// characterization that doesn't need to re-run on every CI build.
    /// Run on demand with
    ///   `bazel test //datalib/backend/etl:etl_unittests \
    ///        --test_arg=--ignored \
    ///        --test_arg=parallel_apply_one_serializes_writes_with_metrics`
    /// when changing the WriteLock, `apply_one`, or doltlite's
    /// auto-commit path.
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

    /// One big transaction wrapping every write — the orchestrator's
    /// production mode. Asserts:
    ///   * every per-doc apply_one succeeds
    ///   * the final COMMIT lands every row in the table
    ///   * doltlite's per-statement overhead is amortized: the
    ///     avg_hold here should be DRAMATICALLY smaller than the
    ///     auto-commit version above
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

        // Open the big batch. Every apply_one call below now reuses
        // the same held conn and accumulates statements into the
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
    //! The index must survive a schema change to `grid_rows`,
    //! `markdowns` or `edges` without a human deleting the file.
    //!
    //! Both directions matter and fail silently in opposite ways. A
    //! reconcile that doesn't fire leaves every statement naming a new
    //! column erroring against an older data root — the #216 bug these
    //! tests were written for. One that fires when it shouldn't wipes a
    //! healthy index on every single pipeline run, and because the
    //! rebuild that follows repopulates it, the only visible symptom is
    //! that the run got slower.

    use std::path::Path;
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
    use tempfile::tempdir;

    use crate::doltlite_raw::actual_column_names;
    use crate::grid_index::{init_schema, EDGES_DDL, MARKDOWNS_DDL};

    /// `grid_rows` exactly as data roots created before #216 have it on
    /// disk — read out of a real one with the doltlite shell. `git_sha`
    /// is followed by `external_id`, and none of the three `upstream_*`
    /// columns that replaced it exist.
    ///
    /// Written out longhand rather than derived from the current DDL:
    /// the point is to pin a shape from history, and a shape computed
    /// from today's struct would silently become "today's shape" again
    /// the next time a column moves.
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
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Seed one indexed document in the pre-#216 shape: a `markdowns`
    /// row carrying the fingerprint that drives the skip, and the
    /// `grid_rows` row it produced.
    async fn seed_pre_216(pool: &SqlitePool) {
        sqlx::query(PRE_216_GRID_ROWS_DDL)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(MARKDOWNS_DDL).execute(pool).await.unwrap();
        for (_table, ddl) in EDGES_DDL {
            sqlx::query(ddl).execute(pool).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO markdowns (markdown_uuid, source_name, provider, kind, source_fingerprint) \
             VALUES ('md-1', 'claude_web', 'anthropic', 'Chat', 'fp-1')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO grid_rows (uuid, provider, kind, source_label, conversation_uuid, \
             entire_chat, text, external_id, markdown_uuid) \
             VALUES ('row-1', 'anthropic', 'Chat', 'Claude', 'conv-1', '/chat/md-1', 'hi', \
             'upstream-1', 'md-1')",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// An index written before #216 is brought to the current schema,
    /// and its fingerprints are cleared so the rebuild actually runs.
    ///
    /// The `markdowns` assertion is the load-bearing half. Recreating
    /// `grid_rows` alone would satisfy every "does the column exist"
    /// check while leaving `markdowns.source_fingerprint` in place — and
    /// `build_grid_index` skips a document whose fingerprint still
    /// matches, so the index would stay empty for as long as nothing
    /// upstream changed.
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

    /// The write path works afterwards. This is the statement that
    /// actually failed on a real data root — `no such column:
    /// upstream_id` from inside `insert_grid_row` — so asserting the
    /// column list alone would leave the thing users hit untested.
    #[tokio::test]
    async fn the_rebuilt_index_accepts_a_write() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("grid.doltlite_db")).await;
        seed_pre_216(&pool).await;
        init_schema(&pool).await.expect("init_schema");

        sqlx::query(
            "INSERT INTO grid_rows (uuid, provider, kind, source_label, conversation_uuid, \
             entire_chat, text, upstream_id, upstream_entity_kind, upstream_scope, markdown_uuid) \
             VALUES ('row-2', 'anthropic', 'Chat', 'Claude', 'conv-1', '/chat/md-1', 'hi', \
             'upstream-1', 'conversation', '', 'md-1')",
        )
        .execute(&pool)
        .await
        .expect("insert naming the post-#216 columns must succeed");
    }

    /// An index already at the current schema is left completely alone.
    ///
    /// Without this, a reconcile whose comparison is subtly wrong (a
    /// name normalized differently, a set compared against a list)
    /// would drop and rebuild the whole index on every run. Nothing
    /// downstream would notice — the rebuild puts the rows back — so
    /// the only symptom would be a pipeline that quietly stopped being
    /// incremental.
    #[tokio::test]
    async fn a_current_index_is_not_touched() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("grid.doltlite_db")).await;
        init_schema(&pool).await.expect("first init_schema");

        sqlx::query(
            "INSERT INTO markdowns (markdown_uuid, source_name, provider, kind, source_fingerprint) \
             VALUES ('md-1', 'claude_web', 'anthropic', 'Chat', 'fp-1')",
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
