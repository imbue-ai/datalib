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

/// Standard bookkeeping columns every object table carries. Splice
/// into the table's `CREATE TABLE` after the provider-specific columns:
///
/// ```ignore
/// const CREATE_PAGES: &str = const_format::concatcp!(
///     "CREATE TABLE IF NOT EXISTS pages (
///         id TEXT PRIMARY KEY,
///         parent_id TEXT NULL,
///         last_edited_time TEXT NULL,
///         ",
///     OBJECT_BOOKKEEPING_COLUMNS,
///     ")"
/// );
/// ```
///
/// (We don't use `const_format` in practice — providers just inline
/// the same SQL text. This constant is the canonical reference.)
pub const OBJECT_BOOKKEEPING_COLUMNS: &str = "\
    payload TEXT NULL, \
    fetched_at TEXT NULL, \
    attempt_count INTEGER NOT NULL DEFAULT 0, \
    last_attempt_at TEXT NULL, \
    last_error TEXT NULL";

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
    source_url TEXT NULL,
    fetched_at TEXT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT NULL,
    last_error TEXT NULL
)";

/// DDL every provider gets for free. Concatenated after the
/// provider-specific table list inside [`open`].
pub const SHARED_DDL: &[&str] = &[SYNC_RUNS_DDL, ENDPOINT_SHAPES_DDL, BLOBS_DDL];

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
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .with_context(|| format!("sqlite uri for {}", db_path.display()))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
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
// Generic object-table ops
// ─────────────────────────────────────────────────────────────────────

/// Pre-seed an `id`-only row (NULL payload) into a table. Used when we
/// know an entity exists upstream but haven't fetched its body yet.
/// Existing rows are left untouched (no clobber of payload).
///
/// `table` is interpolated into the SQL string — callers must pass a
/// trusted identifier, not user input. (In practice, every callsite
/// passes a `&'static str` table name.)
pub async fn ensure_id(pool: &SqlitePool, table: &str, id: &str) -> Result<()> {
    let sql = format!("INSERT INTO {table} (id) VALUES (?) ON CONFLICT(id) DO NOTHING");
    sqlx::query(&sql)
        .bind(id)
        .execute(pool)
        .await
        .with_context(|| format!("ensure_id {table}={id}"))?;
    Ok(())
}

/// Bump attempt counters + record an error against an object row.
/// Leaves any previously-fetched payload intact. The row is inserted
/// if not already present.
pub async fn record_object_error(
    pool: &SqlitePool,
    table: &str,
    id: &str,
    err: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let sql = format!(
        "INSERT INTO {table} (id, attempt_count, last_attempt_at, last_error)
         VALUES (?, 1, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            attempt_count = {table}.attempt_count + 1,
            last_attempt_at = excluded.last_attempt_at,
            last_error = excluded.last_error"
    );
    sqlx::query(&sql)
        .bind(id)
        .bind(&now)
        .bind(err)
        .execute(pool)
        .await
        .with_context(|| format!("record_object_error {table}={id}"))?;
    Ok(())
}

/// Ids that should be re-fetched on a `--retry-failed` run: rows whose
/// last attempt left an error set, or that have a NULL payload after
/// at least one attempt.
pub async fn failed_ids(pool: &SqlitePool, table: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT id FROM {table} \
         WHERE last_error IS NOT NULL OR (payload IS NULL AND attempt_count > 0)"
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

/// Insert (or refresh) a blob row with its bytes.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_blob_bytes(
    pool: &SqlitePool,
    id: &str,
    kind: &str,
    owning_id: &str,
    slot: &str,
    content_type: Option<&str>,
    bytes: &[u8],
    source_url: Option<&str>,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO blobs (id, kind, owning_id, slot, content_type, bytes, source_url, fetched_at, last_attempt_at, last_error)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)
         ON CONFLICT(id) DO UPDATE SET
            kind = excluded.kind,
            owning_id = excluded.owning_id,
            slot = excluded.slot,
            content_type = COALESCE(excluded.content_type, blobs.content_type),
            bytes = excluded.bytes,
            source_url = COALESCE(excluded.source_url, blobs.source_url),
            fetched_at = excluded.fetched_at,
            last_attempt_at = excluded.last_attempt_at,
            last_error = NULL",
    )
    .bind(id)
    .bind(kind)
    .bind(owning_id)
    .bind(slot)
    .bind(content_type)
    .bind(bytes)
    .bind(source_url)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .with_context(|| format!("upsert_blob_bytes {id}"))?;
    Ok(())
}

/// Record a blob fetch failure. We need *some* values for the NOT
/// NULL columns even on first failure; callers pass `owning_id` and
/// `slot` so the row carries useful context for a later retry. `kind`
/// defaults to `"unknown"` since the caller often doesn't yet know
/// whether the file is external / uploaded / hosted.
pub async fn record_blob_error(
    pool: &SqlitePool,
    id: &str,
    owning_id: &str,
    slot: &str,
    err: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO blobs (id, kind, owning_id, slot, attempt_count, last_attempt_at, last_error)
         VALUES (?, 'unknown', ?, ?, 1, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            attempt_count = blobs.attempt_count + 1,
            last_attempt_at = excluded.last_attempt_at,
            last_error = excluded.last_error",
    )
    .bind(id)
    .bind(owning_id)
    .bind(slot)
    .bind(&now)
    .bind(err)
    .execute(pool)
    .await
    .with_context(|| format!("record_blob_error {id}"))?;
    Ok(())
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

    const TEST_DDL: &[&str] = &["CREATE TABLE IF NOT EXISTS widgets (
            id TEXT PRIMARY KEY,
            name TEXT NULL,
            payload TEXT NULL,
            fetched_at TEXT NULL,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_attempt_at TEXT NULL,
            last_error TEXT NULL
        )"];

    #[tokio::test]
    async fn open_creates_tables_idempotently() {
        let d = tempdir().unwrap();
        let p = d.path().join("x.doltlite_db");
        let _ = open(&p, TEST_DDL).await.unwrap();
        // Re-opening doesn't error (DDL is IF NOT EXISTS).
        let pool = open(&p, TEST_DDL).await.unwrap();
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

    #[tokio::test]
    async fn run_lifecycle() {
        let d = tempdir().unwrap();
        let pool = open(&d.path().join("y.doltlite_db"), TEST_DDL)
            .await
            .unwrap();
        let id = start_run(&pool, &json!({"x": 1})).await.unwrap();
        finish_run(&pool, id, "ok", &json!({"done": true}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn error_and_retry_flow() {
        let d = tempdir().unwrap();
        let pool = open(&d.path().join("z.doltlite_db"), TEST_DDL)
            .await
            .unwrap();
        record_object_error(&pool, "widgets", "w1", "boom")
            .await
            .unwrap();
        record_object_error(&pool, "widgets", "w1", "boom2")
            .await
            .unwrap();
        let failed = failed_ids(&pool, "widgets").await.unwrap();
        assert_eq!(failed, vec!["w1".to_string()]);
    }

    #[tokio::test]
    async fn blob_roundtrip() {
        let d = tempdir().unwrap();
        let pool = open(&d.path().join("b.doltlite_db"), TEST_DDL)
            .await
            .unwrap();
        upsert_blob_bytes(
            &pool,
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
        assert!(blob_exists(&pool, "id1").await.unwrap());
        let by_owner = load_blobs_by_owner(&pool).await.unwrap();
        assert_eq!(by_owner["owner1"].bytes, b"hello".to_vec());
        let by_id = load_blobs_by_id(&pool).await.unwrap();
        assert_eq!(by_id["id1"].owning_id, "owner1");
    }
}
