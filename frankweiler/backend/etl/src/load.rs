//! Generic Load step: walk a `rendered_md/` tree of `.grid_rows.json`
//! sidecars and upsert their rows into Dolt.
//!
//! Two entry points:
//!
//!   * [`apply_one`] writes a single rendered document into `grid_rows`
//!     and stamps the `documents` row. Called per-doc by sync's render
//!     callback so render+index commit atomically.
//!   * [`load_all`] walks a `rendered_md/` tree and calls `apply_one`
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
//! any provider's Translate step can produce a sidecar tree this loader
//! consumes verbatim.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use frankweiler_schema::grid_rows::{GridRow, DDL as GRID_ROWS_DDL};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tokio::sync::Mutex;

use crate::sidecar::Sidecar;

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
/// per-source parallel translate hits this in production.
///
/// We also discovered (via the wait/hold counters this struct
/// reports) that each per-doc auto-commit costs ~50ms because every
/// statement boundary materializes the prolly tree's manifest. At
/// 488 docs that's ~24s of wall-clock time spent serializing tiny
/// writes through doltlite's per-commit overhead. Wrapping the whole
/// translate phase in ONE `BEGIN ... COMMIT` collapses that overhead
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
    /// translate phase per run and one ROLLBACK target.
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

    /// True iff a `BEGIN ... COMMIT` batch is currently open.
    pub async fn in_transaction(&self) -> bool {
        self.inner.lock().await.tx_conn.is_some()
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
pub struct LoadSummary {
    pub markdowns_total: usize,
    pub markdowns_loaded: usize,
    pub markdowns_skipped: usize,
    pub rows_inserted: usize,
}

/// Apply DDL for `grid_rows` and `markdowns`. The schema is the truth
/// for fresh DBs; no migration support is provided because we don't
/// promise back-compat with pre-`markdowns` databases — wipe and
/// re-ingest if you're upgrading from an older release.
pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
    for (_table, ddl) in GRID_ROWS_DDL {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .context("create grid_rows")?;
    }
    sqlx::query(MARKDOWNS_DDL)
        .execute(pool)
        .await
        .context("create markdowns")?;
    Ok(())
}

/// Renderer-side cache stamp. Bump when the canonical-tuple shape in
/// `compute_row_set_hash` or the rendered `.md` layout changes — every
/// `documents.row_set_hash` is invalidated and the next ingest will
/// re-render. `rust-v1` is the clean break from the Python `"v1"` since
/// the hash encoding differs.
pub const RENDERER_VERSION: &str = "rust-v1";

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
        push(&mut h, Some(&r.when_ts));
        push(&mut h, r.author.as_deref());
        push_i(&mut h, r.message_index);
        push(&mut h, Some(&r.text));
        push(&mut h, r.source_url.as_deref());
        push(&mut h, r.slack_link.as_deref());
        push(&mut h, r.git_sha.as_deref());
        push(&mut h, r.external_id.as_deref());
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

/// Walk `<out>/rendered_md/` for every `*.grid_rows.json` sidecar and
/// rebuild the index by calling [`apply_one`] for each. Off the hot
/// path now — sync's translate step writes through `apply_one` per doc
/// directly — but useful as a disaster-recovery / "reindex from disk"
/// tool.
pub async fn load_all(
    pool: &SqlitePool,
    out_dir: &Path,
    progress: impl Fn(&str),
    now_override: Option<&str>,
) -> Result<LoadSummary> {
    // load_all is single-threaded — there are no parallel workers
    // contending here. A fresh write lock owns the pool clone so
    // `apply_one` has somewhere to acquire connections. We could
    // wrap the whole loop in begin/commit_transaction to batch
    // writes, but load_all is a disaster-recovery tool, not on the
    // hot path; per-call auto-commit is fine.
    let write_lock = WriteLock::new(pool.clone());
    let rendered_root = out_dir.join("rendered_md");
    let mut sidecars: Vec<PathBuf> = Vec::new();
    if rendered_root.exists() {
        collect_sidecars(&rendered_root, &mut sidecars);
    }
    sidecars.sort();

    let mut summary = LoadSummary {
        markdowns_total: sidecars.len(),
        ..Default::default()
    };

    for sidecar_path in &sidecars {
        let raw = fs::read_to_string(sidecar_path)
            .with_context(|| format!("read {}", sidecar_path.display()))?;
        let sidecar: Sidecar = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", sidecar_path.display()))?;

        let md_path = derive_md_path(sidecar_path)
            .with_context(|| format!("derive .md path from {}", sidecar_path.display()))?;

        let markdown_uuid = sidecar.header.markdown_uuid.clone();
        let fingerprint = sidecar.header.source_fingerprint.clone();

        if existing_fingerprint(pool, &markdown_uuid).await? == Some(fingerprint.clone()) {
            summary.markdowns_skipped += 1;
            continue;
        }

        // load_all has no access to the config-level source name, so we
        // fall back to the canonical row's provider (same default as the
        // pre-callback code path).
        let source_name = sidecar
            .rows
            .first()
            .map(|r| r.provider.clone())
            .unwrap_or_default();
        let md = RenderedMarkdown {
            markdown_uuid,
            source_name,
            source_fingerprint: fingerprint,
            // load_all rebuilds the index from sidecars on disk, which
            // don't carry the cheap-probe cursor (it lives in the
            // indexer only). Leaving it None forces the next live sync
            // to fall back to the fingerprint check for these markdowns
            // — safe, just not as fast as the cursor short-circuit.
            upstream_cursor: None,
            md_path,
            render_version: sidecar.header.render_version,
            rows: sidecar.rows,
        };
        let inserted = apply_one(&write_lock, out_dir, &md, now_override)
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
    Ok(summary)
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

/// Look up the on-disk render's fingerprint for one markdown. Caller
/// uses this to decide whether to skip work (sync builds a bulk
/// `HashMap<uuid, fingerprint>` once per source via [`load_fingerprints`]
/// rather than calling this in a loop).
pub async fn existing_fingerprint(
    pool: &SqlitePool,
    markdown_uuid: &str,
) -> Result<Option<String>> {
    let row = sqlx::query("SELECT source_fingerprint FROM markdowns WHERE markdown_uuid = ?")
        .bind(markdown_uuid)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.try_get::<String, _>("source_fingerprint").ok()))
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

    let rendered_at = now_override
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    upsert_markdown(conn, md, qmd_path, &rendered_at)
        .await
        .context("upsert markdowns")?;

    // dolt_commit is issued ONCE per sync run by the orchestrator after
    // the full translate phase finishes — not here. Per-doc commits
    // would land thousands of entries in dolt_log per run, drowning the
    // audit trail. See `frankweiler-sync::main` for the post-translate
    // commit_run call.
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
        .map(|r| r.when_ts.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    let created_at = timestamps.iter().min().copied();
    let updated_at = timestamps.iter().max().copied();
    let row_set_hash = compute_row_set_hash(&md.rows);
    let version_str = format!("{RENDERER_VERSION}.{}", md.render_version);
    // Prefer the user-facing source_name the renderer was invoked with
    // (config.sources[].name in sync). Fall back to the canonical row's
    // provider when load_all rebuilds from disk without that context.
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

async fn insert_grid_row(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    row: &GridRow,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO grid_rows \
         (uuid, provider, kind, source_label, when_ts, author, account, project, channel, \
          conversation_name, conversation_uuid, message_index, entire_chat, text, slack_link, \
          qmd_path, source_url, git_sha, external_id, notion_page_uuid, notion_block_uuid, \
          markdown_uuid) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.uuid)
    .bind(&row.provider)
    .bind(&row.kind)
    .bind(&row.source_label)
    .bind(&row.when_ts)
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
    .bind(&row.external_id)
    .bind(&row.notion_page_uuid)
    .bind(&row.notion_block_uuid)
    .bind(&row.markdown_uuid)
    .execute(&mut **conn)
    .await
    .with_context(|| format!("insert grid_row {}", row.uuid))?;
    Ok(())
}

#[cfg(test)]
// Test diagnostics; cargo test captures stdout/stderr and prints it
// per-test on failure or with `--nocapture`. No MP in scope here.
#[allow(clippy::disallowed_macros)]
mod write_lock_tests {
    //! Reproduces the production "(code 5) database is locked" we saw
    //! on a real `--skip-extract` run: multiple per-source translate
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
    use frankweiler_schema::grid_rows::GridRow;
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
            when_ts: "2026-06-02T20:00:00+00:00".into(),
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
            external_id: None,
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
    ///   `bazel test //frankweiler/backend/etl:etl_unittests \
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
