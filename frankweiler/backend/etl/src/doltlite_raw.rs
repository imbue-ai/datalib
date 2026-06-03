//! Shared utilities for provider-specific doltlite-backed raw stores.
//!
//! Every provider that ports its raw download to doltlite (notion,
//! chatgpt, anthropic, …) ends up needing the same bookkeeping:
//! identical `blobs` / `endpoint_shapes` / `sync_runs` tables,
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
//! `sync_runs.config` / `summary` and `endpoint_shapes.example_*`
//! stay as plain TEXT — they're tiny single-row bookkeeping where
//! debug-friendly `sqlite3` SELECT matters more than parse perf.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::Utc;
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

/// Last captured wire-shape for each endpoint we've talked to. PK is
/// the endpoint identifier itself (e.g. `GET /v1/blocks/{id}/children`);
/// re-running stamps over the same row.
pub const ENDPOINT_SHAPES_DDL: &str = "CREATE TABLE IF NOT EXISTS endpoint_shapes (
    endpoint TEXT PRIMARY KEY,
    example_headers TEXT NULL,
    example_envelope_skeleton TEXT NULL,
    captured_at TEXT NOT NULL
)";

/// Per-blob storage. PK is the upstream-stable identifier for the
/// file (e.g. `file_upload_id`); fall back to `{owning_id}:{slot}` when
/// none exists. NEVER key by `sha256(content)` — the PK must be known
/// BEFORE we fetch so failed-fetch rows can attach to the right slot.
/// `kind` is an open-ended provider-defined label; we used to check
/// it with `CHECK(kind IN ('uploaded','external','notion_hosted'))`
/// but every provider has its own vocabulary so the check is gone.
pub const BLOBS_DDL: &str = "CREATE TABLE IF NOT EXISTS blobs (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    owning_id TEXT NOT NULL,
    slot TEXT NOT NULL,
    content_type TEXT NULL,
    sha256 TEXT NULL,
    bytes BLOB NULL,
    source_url TEXT NULL
)";

/// Sidecar bookkeeping for `blobs`. Same shape as every other
/// object-table sidecar — see [`bookkeeping_ddl_for`].
pub const BLOBS_BOOKKEEPING_DDL: &str = "CREATE TABLE IF NOT EXISTS blobs_bookkeeping (
    id TEXT PRIMARY KEY,
    fetched_at TEXT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT NULL,
    last_error TEXT NULL
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
    ENDPOINT_SHAPES_DDL,
    BLOBS_DDL,
    BLOBS_BOOKKEEPING_DDL,
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
/// shared blobs / endpoint_shapes / sync_runs are appended after.
///
/// The connection is configured for our raw-store use:
///   - `journal_mode=DELETE`: single writer, single reader → no WAL
///     sidecars and a byte-stable file on disk (matters for golden
///     snapshots).
///   - `synchronous=Normal`: durability isn't critical; the upstream
///     API is the source of truth and we can always re-fetch.
pub async fn open(db_path: &Path, extra_ddl: &[&str]) -> Result<SqlitePool> {
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
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .context("open sqlite pool")?;
    for stmt in extra_ddl.iter().chain(SHARED_DDL.iter()) {
        sqlx::query(stmt).execute(&pool).await.with_context(|| {
            format!(
                "apply DDL: {}",
                stmt.split_once('(').map(|p| p.0).unwrap_or(stmt)
            )
        })?;
    }
    Ok(pool)
}

// ─────────────────────────────────────────────────────────────────────
// sync_runs
// ─────────────────────────────────────────────────────────────────────

/// Record the start of a sync run; returns the new `run_id`.
pub async fn start_run(pool: &SqlitePool, config: &Value) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
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
    let now = Utc::now().to_rfc3339();
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
    let hash: Option<String> = sqlx::query_scalar("SELECT dolt_commit('-Am', ?)")
        .bind(msg)
        .fetch_optional(pool)
        .await
        .context("dolt_commit")?;
    Ok(hash)
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
/// Whole-table bookkeeping (`sync_runs`, `endpoint_shapes`,
/// `sync_scope_state`) is preserved — that's audit log + API
/// discovery metadata + resume cursor, none of which is "content"
/// the reset is trying to re-pull.
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
    for sql in ["DELETE FROM blobs", "DELETE FROM blobs_bookkeeping"] {
        sqlx::query(sql)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("truncate {sql}"))?;
    }
    tx.commit().await.context("commit truncate tx")?;
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
    let now = Utc::now().to_rfc3339();
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

// ─────────────────────────────────────────────────────────────────────
// endpoint_shapes
// ─────────────────────────────────────────────────────────────────────

/// Record (or refresh) the wire-shape skeleton for one endpoint.
/// Caller is responsible for blanking out data fields in
/// `envelope_skeleton`.
pub async fn record_endpoint_shape(
    pool: &SqlitePool,
    endpoint: &str,
    headers: &Value,
    envelope_skeleton: &Value,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let h = serde_json::to_string(headers).unwrap_or_else(|_| "{}".into());
    let e = serde_json::to_string(envelope_skeleton).unwrap_or_else(|_| "{}".into());
    sqlx::query(
        "INSERT INTO endpoint_shapes (endpoint, example_headers, example_envelope_skeleton, captured_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(endpoint) DO UPDATE SET
            example_headers = excluded.example_headers,
            example_envelope_skeleton = excluded.example_envelope_skeleton,
            captured_at = excluded.captured_at",
    )
    .bind(endpoint)
    .bind(&h)
    .bind(&e)
    .bind(&now)
    .execute(pool)
    .await
    .context("upsert endpoint_shapes")?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// blobs
// ─────────────────────────────────────────────────────────────────────

/// Bytes for one blob, paired with the metadata downstream renderers
/// need to write it back to disk and link to it.
#[derive(Debug, Clone)]
pub struct BlobBytes {
    pub id: String,
    pub owning_id: String,
    pub slot: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub source_url: Option<String>,
}

/// True iff a blob row with this id already has its bytes stored.
/// Used to short-circuit refetch: once we have a copy we trust it
/// (signed URLs rotate; bytes don't).
pub async fn blob_exists(pool: &SqlitePool, id: &str) -> Result<bool> {
    let row = sqlx::query("SELECT 1 FROM blobs WHERE id = ? AND bytes IS NOT NULL LIMIT 1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("blob_exists")?;
    Ok(row.is_some())
}

/// Pre-seed a blob row before its bytes have been fetched, AND
/// its matching bookkeeping sidecar row. Lets the caller record
/// "we know this file exists" as soon as the listing reveals it,
/// so a Ctrl-C / network failure leaves behind enough state to
/// count "known but undownloaded" in tooling. INSERT OR IGNORE on
/// both tables so an existing row with bytes (or an error history)
/// is never clobbered.
///
/// Takes a transaction so data and sidecar inserts are atomic.
pub async fn pre_seed_blob_stub(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    kind: &str,
    owning_id: &str,
    slot: &str,
    content_type: Option<&str>,
    source_url: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO blobs (id, kind, owning_id, slot, content_type, source_url)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(kind)
    .bind(owning_id)
    .bind(slot)
    .bind(content_type)
    .bind(source_url)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("pre_seed_blob_stub data {id}"))?;
    sqlx::query("INSERT OR IGNORE INTO blobs_bookkeeping (id) VALUES (?)")
        .bind(id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("pre_seed_blob_stub bookkeeping {id}"))?;
    Ok(())
}

/// Insert (or refresh) a blob row with its bytes, plus a
/// success entry in `blobs_bookkeeping`. Both writes in one
/// transaction.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_blob_bytes(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    kind: &str,
    owning_id: &str,
    slot: &str,
    content_type: Option<&str>,
    bytes: &[u8],
    source_url: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO blobs (id, kind, owning_id, slot, content_type, bytes, source_url)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            kind = excluded.kind,
            owning_id = excluded.owning_id,
            slot = excluded.slot,
            content_type = COALESCE(excluded.content_type, blobs.content_type),
            bytes = excluded.bytes,
            source_url = COALESCE(excluded.source_url, blobs.source_url)",
    )
    .bind(id)
    .bind(kind)
    .bind(owning_id)
    .bind(slot)
    .bind(content_type)
    .bind(bytes)
    .bind(source_url)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert_blob_bytes data {id}"))?;
    record_object_attempt(tx, "blobs", id, None).await
}

/// Record a blob fetch failure. We need *some* values for the NOT
/// NULL columns even on first failure; callers pass `owning_id` and
/// `slot` so the row carries useful context for a later retry. `kind`
/// defaults to `"unknown"` since the caller often doesn't yet know
/// whether the file is external / uploaded / hosted.
///
/// Inserts the (stub) blobs row if absent, then records the
/// failure attempt in `blobs_bookkeeping`. Both atomic.
pub async fn record_blob_error(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    owning_id: &str,
    slot: &str,
    err: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO blobs (id, kind, owning_id, slot)
         VALUES (?, 'unknown', ?, ?)",
    )
    .bind(id)
    .bind(owning_id)
    .bind(slot)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("record_blob_error data {id}"))?;
    record_object_attempt(tx, "blobs", id, Some(err)).await
}

/// Load every blob row's bytes keyed by `owning_id`. When a single
/// owner has multiple blobs (e.g. one message with three attachments),
/// only the lexically-last `id` wins — fine for Notion (one blob per
/// block) but providers with multi-blob owners should use
/// [`load_blobs_by_id`] instead.
pub async fn load_blobs_by_owner(pool: &SqlitePool) -> Result<HashMap<String, BlobBytes>> {
    let by_id = load_blobs_by_id(pool).await?;
    let mut out: HashMap<String, BlobBytes> = HashMap::with_capacity(by_id.len());
    for (_id, b) in by_id {
        out.insert(b.owning_id.clone(), b);
    }
    Ok(out)
}

/// Load every blob row keyed by blob `id`. Use this when one `owning_id`
/// may carry many blobs (chatgpt/anthropic conversations have many
/// attachments per conversation).
pub async fn load_blobs_by_id(pool: &SqlitePool) -> Result<HashMap<String, BlobBytes>> {
    let rows = sqlx::query(
        "SELECT id, owning_id, slot, content_type, bytes, source_url \
         FROM blobs WHERE bytes IS NOT NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("load_blobs_by_id")?;
    let mut out: HashMap<String, BlobBytes> = HashMap::with_capacity(rows.len());
    for r in rows {
        let id: String = match r.try_get("id") {
            Ok(s) => s,
            Err(_) => continue,
        };
        let bytes: Vec<u8> = match r.try_get("bytes") {
            Ok(b) => b,
            Err(_) => continue,
        };
        let owning_id: String = r.try_get("owning_id").unwrap_or_default();
        let slot: String = r.try_get("slot").unwrap_or_default();
        let content_type: Option<String> = r.try_get("content_type").ok();
        let source_url: Option<String> = r.try_get("source_url").ok();
        out.insert(
            id.clone(),
            BlobBytes {
                id,
                owning_id,
                slot,
                content_type,
                bytes,
                source_url,
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
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
        sqlx::query("SELECT COUNT(*) FROM blobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT COUNT(*) FROM endpoint_shapes")
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

        // Phase 2: reopen via the orchestrator's hook and commit.
        let msg = "extract source: rows=1 commit_run_at_path test";
        let hash = commit_run_at_path(&db, msg)
            .await
            .expect("commit_run_at_path ok")
            .expect("doltlite linked but commit hash was None");

        // Phase 3: verify the commit is durable by reopening AGAIN
        // and querying dolt_log. This is the load-bearing assertion —
        // proves the commit actually persisted to disk, not just that
        // commit_run returned a hash.
        let verify = open_test(&db).await;
        let logged_msg: String =
            sqlx::query_scalar("SELECT message FROM dolt_log() WHERE commit_hash = ? LIMIT 1")
                .bind(&hash)
                .fetch_one(&verify)
                .await
                .expect("dolt_log lookup after reopen");
        assert_eq!(logged_msg, msg, "dolt_log message mismatch after reopen");

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

    #[tokio::test]
    async fn blob_roundtrip() {
        let d = tempdir().unwrap();
        let pool = open_test(&d.path().join("b.doltlite_db")).await;
        let mut tx = pool.begin().await.unwrap();
        upsert_blob_bytes(
            &mut tx,
            "id1",
            "external",
            "owner1",
            "image",
            Some("image/png"),
            b"hello",
            Some("https://x.test/i.png"),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(blob_exists(&pool, "id1").await.unwrap());
        let by_owner = load_blobs_by_owner(&pool).await.unwrap();
        assert_eq!(by_owner["owner1"].bytes, b"hello".to_vec());
        let by_id = load_blobs_by_id(&pool).await.unwrap();
        assert_eq!(by_id["id1"].owning_id, "owner1");

        // Sidecar should reflect one successful attempt.
        let row: (Option<String>, i64, Option<String>) = sqlx::query_as(
            "SELECT fetched_at, attempt_count, last_error FROM blobs_bookkeeping WHERE id = 'id1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(row.0.is_some(), "fetched_at should be set on success");
        assert_eq!(row.1, 1);
        assert!(row.2.is_none(), "last_error should be cleared on success");
    }
}
