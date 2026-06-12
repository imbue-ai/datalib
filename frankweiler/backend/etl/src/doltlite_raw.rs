//! Shared utilities for provider-specific doltlite-backed raw stores.
//!
//! Every provider that ports its raw download to doltlite (notion,
//! chatgpt, anthropic, …) ends up needing the same bookkeeping:
//! identical `blob_refs` / `sync_runs` tables,
//! identical "open this file with `journal_mode=DELETE`" boilerplate,
//! identical bookkeeping columns on every object table
//! (`payload TEXT NULL`, `fetched_at`, `attempt_count`, …), and the
//! same primary-key policy spelled out below.
//!
//! This module owns all of that so the provider crates only have to
//! describe the *provider-specific* object tables (pages/blocks for
//! notion, conversations for chatgpt, …) and the upserts that
//! populate them.
//!
//! ─────────────────────────────────────────────────────────────────
//!
//! ## PRIMARY KEY POLICY — read this before adding a new table.
//!
//! Every row in a raw-store database represents a *thing that exists
//! upstream*. Each object table's PK is the **upstream identifier for
//! that thing**, stored as TEXT. NO SURROGATE AUTOINCREMENT INTEGERS
//! and no ROWID-as-PK tricks. The reasons are load-bearing:
//!
//! 1. **`dolt diff` stability.** The raw store sits on top of doltlite;
//!    `dolt diff` compares rows by PK. Re-fetching the same upstream
//!    row on a different day must land at the *same* row, so the diff
//!    reflects content change only — not row-id churn.
//!
//! 2. **Idempotent upserts.** `ON CONFLICT(id) DO UPDATE` is meaningful
//!    only when `id` is the upstream id. A surrogate would force a
//!    "find then update or insert" two-query dance.
//!
//! 3. **Pre-seeding.** The design supports inserting `(id, NULL payload)`
//!    rows when we know upstream that an object exists but haven't
//!    fetched its body yet. The pre-seeded row and the eventual
//!    detail-fetched row must collapse into the same row — only works
//!    if both writers know the PK up front.
//!
//! 4. **Cross-table references** (e.g. `blocks.parent_id`,
//!    `messages.conversation_id`) only mean something if they point at
//!    upstream ids.
//!
//! Within-parent ordering (e.g. blocks within a page) is a SEPARATE
//! concern from identity. When it matters, carry an explicit integer
//! column. NEVER borrow the PK for ordering. Don't `ORDER BY rowid`
//! either — doltlite hides it.
//!
//! Exception: [`SYNC_RUNS_DDL`] uses `AUTOINCREMENT INTEGER` because a
//! sync invocation has no upstream identity — it's a local event.
//!
//! ─────────────────────────────────────────────────────────────────
//!
//! ## JSONB storage
//!
//! Per-row `payload` columns store JSON as SQLite **JSONB** (binary
//! representation, added in SQLite 3.45 / doltlite 0.11.2+). INSERTs
//! wrap the bound text payload in `jsonb(?)`; loads use
//! `SELECT json(payload) AS payload` so the Rust side keeps getting
//! text it can hand to `serde_json::from_str`.
//!
//! The on-wire JSON value is preserved (jsonb is a faithful binary
//! encoding); `dolt diff` still shows row-level changes at the right
//! granularity, since dolt diffs whole rows by PK rather than reaching
//! inside the JSON document. `sqlite3` ad-hoc queries should select
//! `json(payload)` rather than the raw column.
//!
//! `sync_runs.config` / `summary`
//! stay as plain TEXT — they're tiny single-row bookkeeping where
//! debug-friendly `sqlite3` SELECT matters more than parse perf.
//!
//! ─────────────────────────────────────────────────────────────────
//!
//! ## Connection pool size: ALWAYS 1 for doltlite files
//!
//! Doltlite's session has a per-connection HEAD pointer / working
//! set. Connecting through a `SqlitePool` with
//! `max_connections > 1` means individual statements in your
//! Rust code can land on different pool connections, each of which
//! sees its own working tree. Symptoms we've hit in practice:
//!
//!   * a `SELECT dolt_commit('-Am', '...')` that returns a fresh
//!     hash but doesn't appear in the next `SELECT message FROM
//!     dolt_log()` (read landed on a connection whose HEAD hadn't
//!     refreshed), and
//!   * `commit conflict: another connection committed to this
//!     branch. Please retry your transaction.` errors when
//!     interleaved INSERT/DELETE/`dolt_commit` calls happen to be
//!     scheduled across two connections.
//!
//! The dolt maintainers confirm (2026-06-03 conversation): "we have
//! the problem in Dolt too — connection pools are tricky, you can
//! get around it by setting the pool size to 1". For our workload
//! that's the right answer anyway: every doltlite file in this
//! codebase has at most one writer at a time and one reader at a
//! time, and the [`crate::load::WriteLock`] already serializes
//! cross-task writers at the application layer.
//!
//! [`open`] therefore pins `max_connections(1)`. All other code
//! that opens a `SqlitePool` against a `.doltlite_db` file MUST
//! do the same. If you find a callsite that doesn't, fix it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

// ─────────────────────────────────────────────────────────────────────
// Canonical column names
// ─────────────────────────────────────────────────────────────────────
//
// Spelled out as constants so every provider agrees, and so a future
// rename has one search target instead of N. Used only in the
// constant DDL fragments below today — provider code references the
// columns by name in inline SQL.
pub const COL_ID: &str = "id";
pub const COL_PAYLOAD: &str = "payload";
pub const COL_FETCHED_AT: &str = "fetched_at";
pub const COL_ATTEMPT_COUNT: &str = "attempt_count";
pub const COL_LAST_ATTEMPT_AT: &str = "last_attempt_at";
pub const COL_LAST_ERROR: &str = "last_error";

/// `payload` is content — stays on the object table. The four
/// bookkeeping fields (`fetched_at`, `attempt_count`,
/// `last_attempt_at`, `last_error`) live in a sidecar
/// `<table>_bookkeeping` table — see [`bookkeeping_ddl_for`].
///
/// Splitting them out means `dolt diff` over the data tables
/// reflects only upstream content change, not bookkeeping churn
/// from re-fetches. This is what makes the
/// `--reset-and-redownload` sync flag's "did anything actually
/// change?" assertion meaningful.
///
/// Provider DDL example:
/// ```ignore
/// const CREATE_PAGES: &str = "CREATE TABLE IF NOT EXISTS pages (
///     id TEXT PRIMARY KEY,
///     parent_id TEXT NULL,
///     last_edited_time TEXT NULL,
///     payload TEXT NULL
/// )";
/// // Plus, in the provider's DDL list:
/// //   bookkeeping_ddl_for("pages")
/// ```
/// CREATE TABLE text for the sidecar bookkeeping table paired with
/// `<table>`. PK matches the parent table's `id` so the sidecar
/// inner-joins trivially.
///
/// Per the "always-paired" lifecycle: every row inserted into the
/// object table gets a matching sidecar row in the same
/// transaction (use [`ensure_object_row`] to seed both). The
/// sidecar starts with `attempt_count=0` and the other columns
/// NULL; the first fetch attempt updates them via
/// [`record_object_attempt`].
pub fn bookkeeping_ddl_for(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table}_bookkeeping (
            id TEXT PRIMARY KEY,
            fetched_at TEXT NULL,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_attempt_at TEXT NULL,
            last_error TEXT NULL
        )"
    )
}

/// The two columns every wire-payload entity table requires: `id` PK
/// and the `payload` JSONB blob holding the upstream wire bytes. Embed
/// this struct as the **first** field of any row type that maps to a
/// wire-payload table; the `#[derive(WirePayloadRow)]` macro (in
/// `frankweiler-etl-macros`) recognizes it by *type*, not by field
/// name, so a rename or typo is a compile error rather than a runtime
/// SQL mismatch.
///
/// Per-row content fingerprints used to live next to these as a
/// `payload_blake3` hex hash, hand-maintained by every extract site
/// and consumed by translate to drive incremental skip. That column is
/// gone: translate now asks doltlite directly via `dolt_diff_<table>`
/// what changed since the last render, which is both cheaper (the
/// prolly-tree diff is already in dolt's hot path) and the single
/// source of truth — see [`crate::render_cursor`] and the per-provider
/// `translate::parse` for the new shape.
///
/// Pair with [`wire_payload_table_ddl`] (the hand-written DDL helper)
/// or — for the canonical path — the derive macro, which generates
/// the DDL straight off the row struct's field list.
#[derive(Debug, Clone)]
pub struct WirePayload {
    pub id: String,
    pub payload: String,
}

/// Implemented for any row type whose table shape is "wire-payload":
/// id + payload + a handful of promoted columns. The single method
/// returns the table's DDL, suitable for splicing into a provider's
/// `full_ddl()` vector.
///
/// Hand-implementing this trait is possible but unusual; the
/// `#[derive(WirePayloadRow)]` macro in `frankweiler-etl-macros`
/// generates it (and the matching `BulkUpsertable` impl) from a row
/// struct in one shot. See `signal::extract::schema_raw` for the
/// canonical applications.
pub trait WirePayloadRow {
    /// `CREATE TABLE IF NOT EXISTS …` for this row type's table.
    /// Equivalent to calling [`wire_payload_table_ddl`] with the
    /// promoted-column declarations derived from the struct's
    /// non-`WirePayload` fields.
    fn ddl() -> String;
}

/// Build a `CREATE TABLE` statement for an event-shaped raw table
/// that stores its upstream wire bytes as a `payload` JSONB blob.
/// Every such table shares the same shape — the `id`/`payload` pair
/// at the top, the entity's promoted columns underneath:
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS <table> (
///     id             TEXT PRIMARY KEY,
///     payload        TEXT NULL,
///     <promoted columns>
/// )
/// ```
///
/// Callers pass `promoted_columns` as one column-declaration per slice
/// entry, *without* commas — the helper joins them and handles the
/// splicing so individual call sites can't drift on the comma/newline
/// convention. Pass `&[]` when the entity has no promoted columns
/// (`account`'s single-row case).
///
/// A per-row `payload_blake3` hex column used to live here for
/// fingerprint-driven incremental render skips; it's been removed in
/// favor of `dolt_diff_<table>`-driven incremental render, which uses
/// doltlite's prolly-tree diff as the single source of truth. Existing
/// rows on disk still carry the column as dead weight — `--reset-and-
/// redownload` cleans it up.
pub fn wire_payload_table_ddl(table: &str, promoted_columns: &[&str]) -> String {
    let promoted_block = if promoted_columns.is_empty() {
        String::new()
    } else {
        format!(",\n    {}", promoted_columns.join(",\n    "))
    };
    format!(
        "CREATE TABLE IF NOT EXISTS {table} (
    id             TEXT PRIMARY KEY,
    payload        TEXT NULL{promoted_block}
)"
    )
}

// ─────────────────────────────────────────────────────────────────────
// Shared DDL
// ─────────────────────────────────────────────────────────────────────

/// Append-only log of sync invocations. One row per `extract::fetch`
/// call, stamped via [`start_run`] / [`finish_run`]. A crash mid-sync
/// still leaves a row with `status='running'`.
pub const SYNC_RUNS_DDL: &str = "CREATE TABLE IF NOT EXISTS sync_runs (
    run_id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    finished_at TEXT NULL,
    config TEXT NOT NULL,
    status TEXT NOT NULL,
    summary TEXT NULL
)";

/// Per-scope incremental-sync cursor table. Used by providers (github,
/// gitlab) whose discovery is keyed by a search scope ("author:@me",
/// "assigned_to_me", …) and which want to narrow each subsequent run
/// via `updated:>=since` / `updated_after`. PK is the scope string; the
/// `last_seen_at` value is a free-form provider-chosen timestamp (RFC
/// 3339 in practice) that gets compared back to the configured refresh
/// window when the next run picks a `since` floor.
pub const SYNC_SCOPE_STATE_DDL: &str = "CREATE TABLE IF NOT EXISTS sync_scope_state (
    scope TEXT PRIMARY KEY,
    last_seen_at TEXT NOT NULL
)";

/// DDL every provider gets for free. Concatenated after the
/// provider-specific table list inside [`open`].
pub const SHARED_DDL: &[&str] = &[
    SYNC_RUNS_DDL,
    crate::blob_cas::BLOB_REFS_DDL,
    crate::blob_cas::BLOB_REFS_BLAKE3_INDEX_DDL,
    crate::blob_cas::BLOB_REFS_OWNING_INDEX_DDL,
    crate::blob_cas::BLOB_REFS_BOOKKEEPING_DDL,
    SYNC_SCOPE_STATE_DDL,
];

// ─────────────────────────────────────────────────────────────────────
// Path helper
// ─────────────────────────────────────────────────────────────────────

/// Resolve the doltlite database path for a given source.
///
/// Accepts either an explicit `.doltlite_db` file or the legacy
/// directory shape (`<data_root>/raw/<name>`), which is rewritten to a
/// sibling `<name>.doltlite_db` file. This lets the sync
/// orchestrator's `resolved_input_path` contract stay unchanged.
pub fn db_path_for(p: &Path) -> PathBuf {
    if p.extension().and_then(|s| s.to_str()) == Some("doltlite_db") {
        return p.to_path_buf();
    }
    p.with_extension("doltlite_db")
}

// ─────────────────────────────────────────────────────────────────────
// Open
// ─────────────────────────────────────────────────────────────────────

/// Open (or create) the doltlite file and apply DDL idempotently.
///
/// `extra_ddl` carries the provider-specific tables (and indexes). The
/// shared blobs / sync_runs are appended after.
///
/// The connection is configured for our raw-store use:
///   - `journal_mode=DELETE`: single writer, single reader → no WAL
///     sidecars and a byte-stable file on disk (matters for golden
///     snapshots).
///   - `synchronous=Normal`: durability isn't critical; the upstream
///     API is the source of truth and we can always re-fetch.
pub async fn open(db_path: &Path, extra_ddl: &[&str]) -> Result<SqlitePool> {
    // Logged at every call so stray second-pool opens against an
    // already-open file are visible — max_connections=1 means a second
    // pool will surface as "database is locked" on dolt_commit, and
    // without this log it's hard to attribute. The elapsed time on
    // success also makes slow opens visible during long startup phases.
    let started = std::time::Instant::now();
    tracing::info!(path = %db_path.display(), "doltlite_raw::open: opening sqlite pool");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
    }
    // Don't set journal_mode here: doltlite manages its own storage
    // via the prolly chunk store, not SQLite's pager journal, and
    // rejects `PRAGMA journal_mode = …` with
    // "journal_mode is not configurable on doltlite-format databases".
    // synchronous is harmless on doltlite (it just maps to the
    // chunk-store fsync policy) but we leave it default to avoid
    // surprises.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .with_context(|| format!("sqlite uri for {}", db_path.display()))?
        .create_if_missing(true);
    // Pool size 1: doltlite's HEAD pointer + working tree are
    // per-connection. See the "Connection pool size" section in this
    // module's docs for the full story. Multiple pool connections
    // produce silent dolt_log dropouts and `commit conflict` errors
    // on interleaved writes.
    //
    // `acquire_timeout` is bumped well past sqlx's 30s default: cold
    // opens of multi-GB raw stores spend most of their time inside
    // `sqlite3_open_v2` (blake3-hashing the prolly root pages), and we
    // saw legitimate 4–10s opens against `slack.doltlite_db` even with
    // the `-O2` doltlite static archive. A 30s ceiling was tight
    // enough that a transient slowness manifested as a hard timeout
    // and 0-row sync. 5min is "obviously something else is wrong"
    // territory.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(300))
        .connect_with(opts)
        .await
        .context("open sqlite pool")?;
    // Rescue commit. If a prior run crashed mid-batch we'd inherit a
    // pile of uncommitted rows in `dolt_status`; the orchestrator's
    // "next run picks it up" recovery only kicks in if subsequent
    // writes actually succeed, and `dolt_log` ends up with the
    // crashed-run state silently folded into a much-later commit
    // (mixing audit-trail concerns). Seal it into its own commit at
    // the start of every open so each tool entry sees a clean tree.
    //
    // No-op when the status is already clean (which is the common
    // path). Failure to take the rescue is not fatal — the orchestrator
    // will fall back to the implicit "next commit folds it in"
    // behavior, which is what we had before.
    rescue_dirty_working_tree(&pool, db_path).await;
    for stmt in extra_ddl.iter().chain(SHARED_DDL.iter()) {
        sqlx::query(stmt).execute(&pool).await.with_context(|| {
            format!(
                "apply DDL: {}",
                stmt.split_once('(').map(|p| p.0).unwrap_or(stmt)
            )
        })?;
    }
    tracing::info!(
        path = %db_path.display(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "doltlite_raw::open: pool ready"
    );
    Ok(pool)
}

/// Stamp a dolt commit of any orphaned working-tree changes inherited
/// from a crashed prior run. The reasons we want this at every open:
///
/// 1. **Clean audit trail.** Without this, the next successful
///    `dolt_commit()` silently folds the crashed-run rows into its
///    own changelog entry, mixing two different runs' work under one
///    `dolt_log` message.
/// 2. **Health check.** A dirty tree at open is a signal worth logging
///    even when we can rescue it — it means somebody crashed.
///
/// We catch and swallow errors: even a stock-libsqlite3 build (no
/// doltlite extensions) lands here in CI, where `dolt_status` doesn't
/// exist and `dolt_commit()` is a missing function. Logging at warn is
/// enough — the caller still gets a usable pool.
async fn rescue_dirty_working_tree(pool: &SqlitePool, db_path: &Path) {
    // First: is there anything dirty? `dolt_status` is a vtab; on
    // stock SQLite it errors with "no such table".
    let dirty: std::result::Result<i64, sqlx::Error> =
        sqlx::query_scalar("SELECT count(*) FROM dolt_status")
            .fetch_one(pool)
            .await;
    let count = match dirty {
        Ok(n) => n,
        Err(e) => {
            // Differentiate "no doltlite extensions" (silent) from
            // "real error" (warn). The former shows up as a missing-
            // table error; everything else is interesting.
            let msg = e.to_string();
            if !msg.contains("no such table") {
                tracing::warn!(
                    path = %db_path.display(),
                    error = %e,
                    "rescue_dirty_working_tree: probe failed"
                );
            }
            return;
        }
    };
    if count == 0 {
        return;
    }
    let msg = format!(
        "rescue: pre-run snapshot of orphaned working tree ({count} dirty entries) at {}",
        frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339()
    );
    tracing::warn!(
        path = %db_path.display(),
        dirty_entries = count,
        "rescue_dirty_working_tree: prior run left {count} dirty entries; sealing into its own commit",
    );
    if let Err(e) = sqlx::query("SELECT dolt_commit('-Am', ?)")
        .bind(&msg)
        .execute(pool)
        .await
    {
        tracing::warn!(
            path = %db_path.display(),
            error = %e,
            "rescue_dirty_working_tree: dolt_commit failed; the next ETL commit will fold the dirty rows in implicitly"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// sync_runs
// ─────────────────────────────────────────────────────────────────────

/// Record the start of a sync run; returns the new `run_id`.
pub async fn start_run(pool: &SqlitePool, config: &Value) -> Result<i64> {
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let cfg = serde_json::to_string(config).context("serialize run config")?;
    let row = sqlx::query(
        "INSERT INTO sync_runs (started_at, config, status) VALUES (?, ?, 'running') RETURNING run_id",
    )
    .bind(&now)
    .bind(&cfg)
    .fetch_one(pool)
    .await
    .context("insert sync_runs")?;
    let id: i64 = row.try_get("run_id").context("read run_id")?;
    Ok(id)
}

/// Mark a sync run as finished with the given status (`ok` / `error`)
/// and an arbitrary JSON summary blob.
pub async fn finish_run(
    pool: &SqlitePool,
    run_id: i64,
    status: &str,
    summary: &Value,
) -> Result<()> {
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let s = serde_json::to_string(summary).context("serialize run summary")?;
    sqlx::query("UPDATE sync_runs SET finished_at = ?, status = ?, summary = ? WHERE run_id = ?")
        .bind(&now)
        .bind(status)
        .bind(&s)
        .bind(run_id)
        .execute(pool)
        .await
        .context("update sync_runs")?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// dolt commit
// ─────────────────────────────────────────────────────────────────────

/// True iff this connection's libsqlite3 has the `dolt_commit` scalar
/// function registered. Lets callers skip commit calls silently against
/// stock libsqlite3 (e.g. in unit tests that build the binary without
/// linking against doltlite).
pub async fn has_dolt_extensions(pool: &SqlitePool) -> bool {
    let res = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pragma_function_list WHERE name = 'dolt_commit'",
    )
    .fetch_one(pool)
    .await;
    matches!(res, Ok(n) if n > 0)
}

/// Open (or no-op) a doltlite file on disk and stamp it with one
/// commit. Returns the commit hash, `Ok(None)` if the file doesn't
/// exist (e.g. extract aborted before materializing any rows) or if
/// the linked libsqlite3 isn't doltlite. Errors only on a real
/// open/commit failure.
///
/// Used by `frankweiler-sync` after each extract source finishes
/// (`ExtractPlan::run`) AND from the SIGINT handler to flush
/// in-flight stores before exit. Tests live in this module.
///
/// The helper opens a brief pool with no extra DDL — the shared
/// tables (sync_runs, blobs, …) are already in the file from the
/// extract pool's lifetime; `open` is CREATE-IF-NOT-EXISTS so it's a
/// no-op for tables that already exist.
pub async fn commit_run_at_path(out_dir: &Path, msg: &str) -> Result<Option<String>> {
    let db_path = db_path_for(out_dir);
    if !db_path.exists() {
        return Ok(None);
    }
    let pool = open(&db_path, &[]).await.context("open for commit")?;
    let hash = commit_run(&pool, msg).await?;
    pool.close().await;
    Ok(hash)
}

/// Stamp the pool's doltlite DB with one commit and return the new
/// commit hash. No-op (returns `Ok(None)`) when the connection's
/// libsqlite3 doesn't expose the doltlite scalars — production runs
/// against doltlite will always populate dolt_log.
///
/// The commit picks up all uncommitted changes (`-A`), with `msg` as
/// the commit message. Callers should put run-summary stats in `msg`
/// (row counts etc.) so `dolt log` is human-auditable without
/// cross-referencing the JSON summary.
pub async fn commit_run(pool: &SqlitePool, msg: &str) -> Result<Option<String>> {
    if !has_dolt_extensions(pool).await {
        return Ok(None);
    }
    // `dolt_commit` errors with "nothing to commit, working tree clean"
    // when there's nothing dirty. That used to be a hard fail; with the
    // rescue commit in `open()` it's a legitimate post-condition (rescue
    // may have already swept everything into its own commit, leaving
    // the orchestrator's trailing commit nothing to do). Treat it as
    // Ok(None) so the caller can keep going.
    match sqlx::query_scalar::<_, Option<String>>("SELECT dolt_commit('-Am', ?)")
        .bind(msg)
        .fetch_optional(pool)
        .await
    {
        Ok(opt) => Ok(opt.flatten()),
        Err(e) if e.to_string().contains("nothing to commit") => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("dolt_commit")),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Reset
// ─────────────────────────────────────────────────────────────────────

/// Truncate every per-row table in the provider's raw store, so the
/// next `extract::fetch` re-downloads everything from upstream.
///
/// Wipes, in one transaction:
///   - each `<table>` in `data_tables`
///   - each `<table>_bookkeeping` paired sidecar
///   - the shared `blobs` table and `blobs_bookkeeping` sidecar
///
/// Whole-table bookkeeping (`sync_runs`, `sync_scope_state`) is
/// preserved — that's audit log + resume cursor, neither of which is
/// "content" the reset is trying to re-pull.
///
/// Tables names are interpolated into SQL; callers must pass
/// trusted identifiers, not user input.
pub async fn truncate_data_tables(pool: &SqlitePool, data_tables: &[&str]) -> Result<()> {
    let mut tx = pool.begin().await.context("begin truncate tx")?;
    for table in data_tables {
        for sql in [
            format!("DELETE FROM {table}"),
            format!("DELETE FROM {table}_bookkeeping"),
        ] {
            sqlx::query(&sql)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("truncate {sql}"))?;
        }
    }
    tx.commit().await.context("commit truncate tx")?;
    Ok(())
}

/// Wipe `blob_refs` + `blob_refs_bookkeeping`. NOT called by
/// [`truncate_data_tables`] (entity reset preserves the ref index so
/// the next extract skips re-fetching bytes we already have in the
/// sibling CAS). Use this when the user explicitly asks to invalidate
/// the cache key — driven by `--refetch-blobs` at the CLI.
///
/// The sibling `cas_objects` file is never touched: bytes are
/// byte-stable; only the
/// [`crate::blob_cas::gc_orphans`] sweep should delete from it.
pub async fn truncate_blob_refs(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin().await.context("begin truncate blob_refs tx")?;
    for sql in ["DELETE FROM blob_refs", "DELETE FROM blob_refs_bookkeeping"] {
        sqlx::query(sql)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("truncate {sql}"))?;
    }
    tx.commit().await.context("commit truncate blob_refs tx")?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Generic object-table ops
// ─────────────────────────────────────────────────────────────────────

/// Pre-seed an `id`-only row (NULL payload) into a table, AND its
/// matching sidecar bookkeeping row. Used when we know an entity
/// exists upstream but haven't fetched its body yet. Existing rows
/// are left untouched (no clobber of payload or attempt counters).
///
/// Always-paired lifecycle: every object row has a matching
/// `<table>_bookkeeping` row. The sidecar row starts with
/// `attempt_count=0` and other columns NULL until a fetch attempt
/// updates it via [`record_object_attempt`].
///
/// Takes a transaction (not a pool) so the data INSERT and the
/// sidecar INSERT land atomically.
///
/// `table` is interpolated into the SQL string — callers must pass a
/// trusted identifier, not user input. (In practice, every callsite
/// passes a `&'static str` table name.)
pub async fn ensure_object_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: &str,
) -> Result<()> {
    let data_sql = format!("INSERT INTO {table} (id) VALUES (?) ON CONFLICT(id) DO NOTHING");
    sqlx::query(&data_sql)
        .bind(id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("ensure_object_row data {table}={id}"))?;
    let bk_sql =
        format!("INSERT INTO {table}_bookkeeping (id) VALUES (?) ON CONFLICT(id) DO NOTHING");
    sqlx::query(&bk_sql)
        .bind(id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("ensure_object_row bookkeeping {table}={id}"))?;
    Ok(())
}

/// Record one fetch attempt against an object row's sidecar.
///
/// `result = None` → success: sets `fetched_at = now`, clears
/// `last_error`. `result = Some(err)` → failure: leaves
/// `fetched_at` untouched, sets `last_error = err`. Both branches
/// bump `attempt_count` and set `last_attempt_at = now`.
///
/// Pairs an `INSERT ... ON CONFLICT DO UPDATE` so it's safe even
/// when the sidecar row hasn't been pre-seeded by
/// [`ensure_object_row`] — but callers should normally pre-seed
/// the data row and its sidecar together for the always-paired
/// invariant.
pub async fn record_object_attempt(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: &str,
    result: Option<&str>,
) -> Result<()> {
    // Always-paired invariant: a sidecar row never exists without a
    // matching data row. The success-branch upsert above already
    // wrote the data row before this call; the failure-branch
    // callers (record_object_error before any successful fetch)
    // wouldn't have, so we INSERT OR IGNORE here. Cheap and idempotent.
    let stub_sql = format!("INSERT OR IGNORE INTO {table} (id) VALUES (?)");
    sqlx::query(&stub_sql)
        .bind(id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("record_object_attempt data stub {table}={id}"))?;
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let sql = match result {
        None => format!(
            "INSERT INTO {table}_bookkeeping (id, fetched_at, attempt_count, last_attempt_at, last_error)
             VALUES (?, ?, 1, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                fetched_at = excluded.fetched_at,
                attempt_count = {table}_bookkeeping.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL"
        ),
        Some(_) => format!(
            "INSERT INTO {table}_bookkeeping (id, attempt_count, last_attempt_at, last_error)
             VALUES (?, 1, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                attempt_count = {table}_bookkeeping.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at,
                last_error = excluded.last_error"
        ),
    };
    let q = sqlx::query(&sql).bind(id).bind(&now);
    let q = match result {
        None => q,
        Some(err) => q.bind(err),
    };
    q.execute(&mut **tx)
        .await
        .with_context(|| format!("record_object_attempt {table}={id}"))?;
    Ok(())
}

/// Persist a successful upsert into the raw storage layer: stamp
/// per-row bookkeeping, commit the transaction, and (if a tape is
/// attached) mirror the row as one JSONL line.
///
/// This is the single chokepoint a provider should call once its
/// table-specific `INSERT … ON CONFLICT(id) DO UPDATE` has run inside
/// `tx`. It exists so a provider author thinks about "write one event
/// to the raw storage layer" as one operation, not three steps that
/// have to be kept in lockstep — see `docs/data_architecture_ingestion.md`
/// § "Wire-event tape (JSONL)" for why doltlite and the tape are both
/// parts of "the raw storage layer."
///
/// Semantics:
/// - [`record_object_attempt`] runs for `(table, id)` inside `tx`.
/// - `tx` is committed.
/// - If `tape` is `Some`, the tape append fires AFTER the commit
///   succeeds, so a rolled-back tx never leaves an orphan tape line
///   describing a row that didn't land in doltlite.
/// - Tape append errors are logged at `error!` level but do not fail
///   the upsert. The doltlite row is already committed and is the
///   source of truth; the tape is a write-only mirror, so failing the
///   caller would be lying about whether the data landed. But a tape
///   failure here is anomalous (the directory is local, the file is
///   ours, there's no contention) — when it happens, we want it loud
///   and visible in logs so it gets investigated.
pub async fn write_event_to_raw_storage_layer(
    tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    tape: Option<&crate::event_tape::EventTape>,
    table: &str,
    id: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    write_events_to_raw_storage_layer(tx, tape, &[(table, id, payload)]).await
}

/// Batch sibling of [`write_event_to_raw_storage_layer`]. Use this
/// when one transaction covers many rows (e.g. one history / replies
/// page upserted in a single `fsync`).
pub async fn write_events_to_raw_storage_layer(
    mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    tape: Option<&crate::event_tape::EventTape>,
    events: &[(&str, &str, &serde_json::Value)],
) -> Result<()> {
    for (table, id, _) in events {
        record_object_attempt(&mut tx, table, id, None).await?;
    }
    tx.commit()
        .await
        .context("commit write_events_to_raw_storage_layer tx")?;
    if let Some(t) = tape {
        for (table, id, payload) in events {
            if let Err(e) = t.append(table, id, payload) {
                tracing::error!(
                    event = "event_tape_append_failed",
                    table = *table,
                    id = *id,
                    error = %format!("{e:#}"),
                    "event tape append failed after doltlite commit; row IS persisted, tape is missing a line — investigate"
                );
            }
        }
    }
    Ok(())
}

// `EventBatch` is the per-table batch shape — defined in
// `crate::bulk` because it is a load-bearing primitive of the bulk
// write path. The tape side (`EventTape::append_batch`) is a
// best-effort sidecar that uses the same struct. Re-exported here
// because providers reach for it next to the chokepoint.
pub use crate::bulk::EventBatch;

/// Chokepoint for the **successful bulk write** path against an
/// event-shaped table (i.e. one whose rows came off a wire and have
/// a meaningful payload to mirror). The provider has already issued
/// its chunked multi-row entity-table UPSERTs inside `tx`; this call:
///
///   1. stamps `<table>_bookkeeping` for every id (via
///      [`crate::bulk::bulk_upsert_bookkeeping`]) in the same tx,
///      one bookkeeping batch per `EventBatch`;
///   2. commits `tx`;
///   3. after the commit succeeds, appends one JSONL line per row to
///      the tape (if attached), one
///      [`crate::event_tape::EventTape::append_batch`] call per batch.
///
/// Post-commit tape errors log at `error!` but do not fail the call.
/// The doltlite rows are already persisted and are the source of
/// truth; the tape is a write-only mirror, so failing the caller
/// would be lying about whether the data landed (same contract as
/// [`write_events_to_raw_storage_layer`]).
///
/// Not the right tool for non-event tables (blob_refs, sidecars,
/// file-imported data with no wire) — those want
/// [`crate::bulk::bulk_upsert_bookkeeping`] called directly inside
/// the caller's tx, with no tape.
///
/// See `docs/data_architecture_ingestion.md` § "Bulk-upsert as the
/// standard write path" for why this is the standard chokepoint for
/// wire-event extracts.
pub async fn bulk_upsert_events(
    mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    tape: Option<&crate::event_tape::EventTape>,
    batches: &[EventBatch<'_>],
    now: &str,
) -> Result<()> {
    for b in batches {
        crate::bulk::bulk_upsert_bookkeeping(
            &mut tx,
            b.table,
            b.rows.iter().map(|(id, _)| *id),
            now,
        )
        .await?;
    }
    tx.commit().await.context("commit bulk_upsert_events tx")?;
    if let Some(t) = tape {
        for b in batches {
            if b.rows.is_empty() {
                continue;
            }
            if let Err(e) = t.append_batch(b) {
                tracing::error!(
                    event = "event_tape_append_failed",
                    table = b.table,
                    count = b.rows.len(),
                    error = %format!("{e:#}"),
                    "event tape append_batch failed after doltlite commit; rows ARE persisted, tape is missing lines — investigate"
                );
            }
        }
    }
    Ok(())
}

/// Convenience: failure branch of [`record_object_attempt`].
/// Kept for callsite readability — same semantics as
/// `record_object_attempt(tx, table, id, Some(err))`.
pub async fn record_object_error(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: &str,
    err: &str,
) -> Result<()> {
    record_object_attempt(tx, table, id, Some(err)).await
}

/// Ids that should be re-fetched on a `--retry-failed` run: rows whose
/// last attempt left an error set, or that have a NULL payload after
/// at least one attempt.
///
/// Joins `<table>` (for payload) and `<table>_bookkeeping` (for
/// attempt_count / last_error). Uses LEFT JOIN so a data row
/// missing its sidecar (shouldn't happen post-migration, but
/// defensively) still surfaces if payload is NULL.
pub async fn failed_ids(pool: &SqlitePool, table: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT t.id FROM {table} t \
         LEFT JOIN {table}_bookkeeping b ON b.id = t.id \
         WHERE b.last_error IS NOT NULL \
            OR (t.payload IS NULL AND COALESCE(b.attempt_count, 0) > 0)"
    );
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .with_context(|| format!("select failed_ids({table})"))?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get::<String, _>("id").ok())
        .collect())
}

/// Snapshot every payload in `table` as a parsed JSON [`Value`],
/// deterministically ordered by `id`. Rows with NULL payload are
/// skipped — they're pre-seeded entries that haven't been fetched yet.
pub async fn load_payloads(pool: &SqlitePool, table: &str) -> Result<Vec<Value>> {
    // Wrap in `json(payload)` so we get text JSON back regardless of
    // whether the column stores a JSONB blob or a JSON text literal.
    // See "JSONB storage" in `doltlite_raw.rs` module docs.
    let sql = format!(
        "SELECT json(payload) AS payload FROM {table} WHERE payload IS NOT NULL ORDER BY id"
    );
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .with_context(|| format!("select {table} payloads"))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let payload: String = match r.try_get("payload") {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Ok(v) = serde_json::from_str::<Value>(&payload) {
            out.push(v);
        }
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────
// sync_scope_state
// ─────────────────────────────────────────────────────────────────────

/// Snapshot every scope's last-seen timestamp. Returns an empty map if
/// the table has no rows (i.e. first run).
pub async fn load_scope_state(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query("SELECT scope, last_seen_at FROM sync_scope_state")
        .fetch_all(pool)
        .await
        .context("select sync_scope_state")?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in rows {
        let scope: String = r.try_get("scope").unwrap_or_default();
        let ts: String = r.try_get("last_seen_at").unwrap_or_default();
        if !scope.is_empty() && !ts.is_empty() {
            out.insert(scope, ts);
        }
    }
    Ok(out)
}

/// Upsert one scope's `last_seen_at` cursor. The value is a free-form
/// timestamp string — callers typically pass RFC 3339.
pub async fn upsert_scope_state(pool: &SqlitePool, scope: &str, last_seen_at: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO sync_scope_state (scope, last_seen_at) VALUES (?, ?)
         ON CONFLICT(scope) DO UPDATE SET last_seen_at = excluded.last_seen_at",
    )
    .bind(scope)
    .bind(last_seen_at)
    .execute(pool)
    .await
    .with_context(|| format!("upsert sync_scope_state {scope}"))?;
    Ok(())
}

#[cfg(test)]
// Test diagnostics + intentional probe-failure prints under stock
// libsqlite3 (no doltlite). cargo-test captures stderr; no MP in scope.
#[allow(clippy::disallowed_macros)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    const WIDGETS_DDL: &str = "CREATE TABLE IF NOT EXISTS widgets (
            id TEXT PRIMARY KEY,
            name TEXT NULL,
            payload TEXT NULL
        )";

    fn test_ddl() -> Vec<String> {
        vec![WIDGETS_DDL.to_string(), bookkeeping_ddl_for("widgets")]
    }

    async fn open_test(p: &Path) -> SqlitePool {
        let owned = test_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        open(p, &slices).await.unwrap()
    }

    #[tokio::test]
    async fn open_creates_tables_idempotently() {
        let d = tempdir().unwrap();
        let p = d.path().join("x.doltlite_db");
        let _ = open_test(&p).await;
        // Re-opening doesn't error (DDL is IF NOT EXISTS).
        let pool = open_test(&p).await;
        // Shared tables exist.
        sqlx::query("SELECT COUNT(*) FROM sync_runs")
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT COUNT(*) FROM blob_refs")
            .fetch_one(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn db_path_for_handles_legacy_dir() {
        let p = Path::new("/tmp/raw/whatever");
        assert_eq!(
            db_path_for(p),
            PathBuf::from("/tmp/raw/whatever.doltlite_db")
        );
        let q = Path::new("/tmp/raw/whatever.doltlite_db");
        assert_eq!(db_path_for(q), q);
    }

    /// `commit_run` against a connection without dolt extensions
    /// returns `Ok(None)` rather than failing — the production
    /// behavior on stock libsqlite3 (e.g. cargo-only unit tests).
    ///
    /// Under bazel (with doltlite linked) this test exercises the
    /// full path: the call returns a real hash, `dolt_log` carries
    /// the new entry with that hash, and the commit message we passed
    /// is the one stored.
    /// Diagnostic — prints what the linked libsqlite3 actually is.
    /// Helps catch the "we thought we were on doltlite but the build
    /// is actually stock SQLite" failure mode.
    #[tokio::test]
    async fn diagnostic_print_sqlite_identity() {
        let d = tempdir().unwrap();
        let pool = open_test(&d.path().join("probe.doltlite_db")).await;
        let ver: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let src: String = sqlx::query_scalar("SELECT sqlite_source_id()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let scalar_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pragma_function_list WHERE name LIKE 'dolt_%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // Also try a direct call against `dolt_commit` — virtual tables
        // and eponymous functions don't always appear in
        // pragma_function_list.
        let direct_call = sqlx::query("SELECT dolt_commit('-Am', 'probe')")
            .execute(&pool)
            .await;
        eprintln!(
            "[sqlite probe] version={ver} source_id={src} dolt_funcs_in_pragma={scalar_count} direct_dolt_commit_ok={}",
            direct_call.is_ok(),
        );
        if let Err(e) = direct_call {
            eprintln!("[sqlite probe] direct_call error: {e}");
        }
    }

    #[tokio::test]
    async fn commit_run_returns_hash_and_dolt_log_entry_or_skips() {
        let d = tempdir().unwrap();
        let pool = open_test(&d.path().join("commit.doltlite_db")).await;

        if !has_dolt_extensions(&pool).await {
            // Stock SQLite path: commit_run should return None without
            // error, and there's no dolt_log to inspect.
            let hash = commit_run(&pool, "stock-sqlite probe")
                .await
                .expect("commit_run ok");
            assert!(
                hash.is_none(),
                "expected None on stock SQLite, got {hash:?}"
            );
            eprintln!("[commit_run test] stock libsqlite3 — dolt_log not asserted");
            return;
        }

        // Doltlite path. Configure committer identity (per-session,
        // not persisted) so dolt_commit doesn't error on a missing
        // user.email when it tries to stamp the commit author.
        sqlx::query("SELECT dolt_config('user.name', 'frankweiler-test')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT dolt_config('user.email', 'test@frankweiler.local')")
            .execute(&pool)
            .await
            .unwrap();

        // Make an uncommitted change so dolt has something to record.
        sqlx::query("INSERT INTO widgets (id, name) VALUES ('w1', 'first')")
            .execute(&pool)
            .await
            .unwrap();

        let msg = "test commit: rows=1";
        let hash = commit_run(&pool, msg)
            .await
            .expect("commit_run ok")
            .expect("doltlite linked but commit_run returned None");
        assert!(!hash.is_empty(), "doltlite returned empty commit hash");

        // The hash dolt_commit returns must appear in dolt_log with
        // the message we passed — confirms the version-control SQL
        // surface is really live (not just that the function exists).
        let logged_msg: String =
            sqlx::query_scalar("SELECT message FROM dolt_log() WHERE commit_hash = ? LIMIT 1")
                .bind(&hash)
                .fetch_one(&pool)
                .await
                .expect("dolt_log lookup");
        assert_eq!(logged_msg, msg, "dolt_log message mismatch");
    }

    /// End-to-end test of the per-source extract commit path:
    /// `frankweiler-sync::ExtractPlan::run` calls `commit_run_at_path`
    /// after each provider's extract finishes, against the doltlite_db
    /// the provider wrote during the run. This test mirrors that
    /// pattern: stage a row via `open` + `start_run` + insert + drop
    /// pool (simulating an extract closing its pool), then reopen via
    /// `commit_run_at_path` (the orchestrator's hook) and verify the
    /// commit lands in dolt_log with the expected message.
    ///
    /// Also exercises `commit_run_at_path`'s no-op behavior for a
    /// non-existent path (extract aborted before the file was created
    /// — `interrupt_commit_all` walks every enabled-source path and
    /// some may not yet exist).
    #[tokio::test]
    async fn commit_run_at_path_persists_across_pool_lifetimes() {
        let d = tempdir().unwrap();
        let db = d.path().join("source.doltlite_db");

        // Phase 1: simulate an extract — open, write, close.
        {
            let pool = open_test(&db).await;
            if !has_dolt_extensions(&pool).await {
                eprintln!("[commit_run_at_path test] stock libsqlite3 — full assertion skipped");
                // Still exercise the no-op-on-missing-file path; it
                // shouldn't depend on doltlite being linked.
                let missing = d.path().join("never_created.doltlite_db");
                let hash = commit_run_at_path(&missing, "ignored")
                    .await
                    .expect("missing-path open should succeed");
                assert!(hash.is_none(), "expected None on missing path");
                return;
            }
            // Per-session committer identity (doltlite requires this).
            sqlx::query("SELECT dolt_config('user.name', 'frankweiler-extract-test')")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("SELECT dolt_config('user.email', 'extract@frankweiler.local')")
                .execute(&pool)
                .await
                .unwrap();

            let run_id = start_run(&pool, &json!({"source": "test"})).await.unwrap();
            sqlx::query("INSERT INTO widgets (id, name) VALUES ('w-extract', 'staged')")
                .execute(&pool)
                .await
                .unwrap();
            finish_run(&pool, run_id, "ok", &json!({"rows": 1}))
                .await
                .unwrap();
            pool.close().await;
        }

        // Phase 2: reopen via the orchestrator's hook and commit. Under
        // the rescue-commit-on-open policy in `open()`, phase 1's
        // orphaned writes are already sealed by the time `commit_run`
        // runs here, so the orchestrator's trailing commit is a no-op
        // (returns None). This is the documented post-condition: a
        // crashed run produces a `rescue: ...` commit in dolt_log, and
        // the next run's trailing commit_run is allowed to find
        // nothing dirty.
        let msg = "extract source: rows=1 commit_run_at_path test";
        let trailing = commit_run_at_path(&db, msg)
            .await
            .expect("commit_run_at_path ok");
        assert!(
            trailing.is_none(),
            "trailing commit should be a no-op after rescue swept the orphaned writes; got {trailing:?}"
        );

        // Phase 3: verify the rescue commit is durable by reopening
        // AGAIN and querying dolt_log. This is the load-bearing
        // assertion — proves the orphaned writes were sealed by the
        // rescue at phase 2's open(), not lost.
        let verify = open_test(&db).await;
        let logged: Vec<String> =
            sqlx::query_scalar("SELECT message FROM dolt_log() ORDER BY date DESC")
                .fetch_all(&verify)
                .await
                .expect("dolt_log lookup after reopen");
        assert!(
            logged.iter().any(|m| m.starts_with("rescue: ")),
            "expected a rescue commit in dolt_log; got {logged:?}"
        );

        // No-op path: pointing at a never-created file should NOT
        // create one and should NOT error.
        let missing = d.path().join("never_created.doltlite_db");
        let h2 = commit_run_at_path(&missing, "ignored")
            .await
            .expect("missing-path open should succeed");
        assert!(h2.is_none(), "expected None on missing path");
        assert!(
            !missing.exists(),
            "missing-path call must not create the file"
        );
    }

    #[tokio::test]
    async fn run_lifecycle() {
        let d = tempdir().unwrap();
        let pool = open_test(&d.path().join("y.doltlite_db")).await;
        let id = start_run(&pool, &json!({"x": 1})).await.unwrap();
        finish_run(&pool, id, "ok", &json!({"done": true}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn error_and_retry_flow() {
        let d = tempdir().unwrap();
        let pool = open_test(&d.path().join("z.doltlite_db")).await;
        // Pre-seed the data row + sidecar via ensure_object_row, then
        // record two failures in their own transactions.
        {
            let mut tx = pool.begin().await.unwrap();
            ensure_object_row(&mut tx, "widgets", "w1").await.unwrap();
            tx.commit().await.unwrap();
        }
        for err in ["boom", "boom2"] {
            let mut tx = pool.begin().await.unwrap();
            record_object_error(&mut tx, "widgets", "w1", err)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        let failed = failed_ids(&pool, "widgets").await.unwrap();
        assert_eq!(failed, vec!["w1".to_string()]);

        // Verify the sidecar carries the expected attempt count.
        let attempts: i64 =
            sqlx::query_scalar("SELECT attempt_count FROM widgets_bookkeeping WHERE id = 'w1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 2);
    }

    /// Regression guard for the "always pool size 1 against
    /// doltlite" rule (see this module's docs for the full story
    /// and the dolt-team-confirmed advice).
    ///
    /// At `max_connections=1`: a `dolt_commit` followed by a
    /// `dolt_log()` query produces a consistent view — the new
    /// commit's message appears in the log. ASSERT this — if it
    /// ever stops being true, our connection-pool assumption has
    /// regressed.
    ///
    /// At `max_connections=2` and `4`: we also exercise the path
    /// and just OBSERVE the failure mode (via eprintln) so a
    /// future doltlite upgrade can be diff'd against the
    /// historical shape. Two outcomes we've seen empirically:
    ///   - `commit conflict: another connection committed to
    ///     this branch` errors on the second commit,
    ///   - the commit succeeding but its message not appearing in
    ///     `dolt_log()` (stale-HEAD reader connection).
    /// We DO NOT assert on these — that'd codify a bug as a test
    /// requirement.
    ///
    /// A skip-out path covers stock libsqlite3 (cargo-only runs).
    #[tokio::test]
    async fn dolt_log_visibility_across_pool_sizes() {
        for max_conns in [1u32, 2, 4] {
            let d = tempdir().unwrap();
            let db_path = d.path().join(format!("probe_{max_conns}.doltlite_db"));
            // Apply DDL (incl. shared blobs) via the normal open() so
            // we get the canonical shape, then close and re-open with
            // a tunable pool size.
            let _ = open_test(&db_path).await;

            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
                .unwrap()
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(max_conns)
                .connect_with(opts)
                .await
                .unwrap();

            if !has_dolt_extensions(&pool).await {
                eprintln!("[pool_probe] stock libsqlite3 — skipping (max_conns={max_conns})");
                continue;
            }
            // Per-session committer identity.
            sqlx::query("SELECT dolt_config('user.name', 'pool-probe')")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("SELECT dolt_config('user.email', 'pool-probe@x')")
                .execute(&pool)
                .await
                .unwrap();

            // Helper closure: stage a write + return any sqlx error
            // instead of panicking, so we can observe the failure
            // mode at each pool size.
            let try_exec = |sql: &'static str| {
                let pool = pool.clone();
                async move {
                    sqlx::query(sql)
                        .execute(&pool)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            };

            // Stage row + sidecar.
            let mut errs: Vec<String> = Vec::new();
            for sql in [
                "INSERT INTO widgets (id, name, payload) VALUES ('w1', 'one', NULL)",
                "INSERT INTO widgets_bookkeeping (id, fetched_at) VALUES ('w1', '2026-06-03T00:00:00Z')",
            ] {
                if let Err(e) = try_exec(sql).await {
                    errs.push(format!("setup `{sql}`: {e}"));
                }
            }

            // First commit.
            let h1: Result<Option<String>, String> =
                sqlx::query_scalar("SELECT dolt_commit('-Am', 'pool-probe-first')")
                    .fetch_optional(&pool)
                    .await
                    .map_err(|e| e.to_string());

            // Reset: delete + reinsert IDENTICAL data, plus new
            // fetched_at on the sidecar — the integration-test shape.
            for sql in [
                "DELETE FROM widgets",
                "DELETE FROM widgets_bookkeeping",
                "INSERT INTO widgets (id, name, payload) VALUES ('w1', 'one', NULL)",
                "INSERT INTO widgets_bookkeeping (id, fetched_at) VALUES ('w1', '2026-06-03T00:00:05Z')",
            ] {
                if let Err(e) = try_exec(sql).await {
                    errs.push(format!("reset `{sql}`: {e}"));
                }
            }

            // Second commit — the call that errored at max_conns>=2.
            let h2: Result<Option<String>, String> =
                sqlx::query_scalar("SELECT dolt_commit('-Am', 'pool-probe-second')")
                    .fetch_optional(&pool)
                    .await
                    .map_err(|e| e.to_string());

            // dolt_log readback.
            let messages: Result<Vec<String>, String> =
                sqlx::query_scalar("SELECT message FROM dolt_log() ORDER BY date ASC")
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| e.to_string());

            eprintln!(
                "[pool_probe max_conns={max_conns}]\n  \
                 setup_errors={errs:?}\n  \
                 h1={h1:?}\n  \
                 h2={h2:?}\n  \
                 messages={messages:?}"
            );

            // Regression guard only for the supported configuration.
            if max_conns == 1 {
                assert!(
                    errs.is_empty(),
                    "max_conns=1: no setup errors should fire; got {errs:?}"
                );
                let h1 = h1
                    .clone()
                    .expect("max_conns=1: first dolt_commit should not error")
                    .expect("max_conns=1: first dolt_commit should return a hash");
                let h2 = h2
                    .clone()
                    .expect("max_conns=1: second dolt_commit should not error")
                    .expect("max_conns=1: second dolt_commit should return a hash");
                assert_ne!(
                    h1, h2,
                    "max_conns=1: second commit hash should differ from first"
                );
                let msgs = messages
                    .clone()
                    .expect("max_conns=1: dolt_log read should succeed");
                assert!(
                    msgs.iter().any(|m| m == "pool-probe-first"),
                    "max_conns=1: first commit message missing from dolt_log: {msgs:?}"
                );
                assert!(
                    msgs.iter().any(|m| m == "pool-probe-second"),
                    "max_conns=1: second commit message missing from dolt_log: {msgs:?}"
                );
            }
            // For max_conns ∈ {2, 4} we deliberately don't assert —
            // the eprintln above logs whatever doltlite happens to do.

            pool.close().await;
        }
    }
}
