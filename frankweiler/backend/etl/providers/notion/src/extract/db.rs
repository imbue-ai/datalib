//! Doltlite-backed raw store for the Notion provider.
//!
//! Replaces the per-entity JSONL trees with a single sqlx-managed sqlite
//! (eventually doltlite) file at `<data_root>/raw/<name>.doltlite_db`.
//! Schema is owned by this provider; DDL lives in [`DDL`] as `CREATE TABLE
//! IF NOT EXISTS` so an existing file is re-opened without reset.
//!
//! See `DOLTLITE_RAW.md` next to this crate for the design rationale.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

/// All object tables share these bookkeeping columns. A NULL `payload`
/// means "we know this id exists upstream but we haven't fetched it yet"
/// (either pre-seeded or a previous fetch failed). `attempt_count` is
/// bumped on every fetch attempt; `last_error` is cleared on success.
///
/// Primary keys are the **upstream Notion UUIDs**, not surrogate
/// integers. That's load-bearing for dolt: the same row needs to land
/// at the same place on every re-fetch so `dolt diff` reflects actual
/// content changes, not accidental row-id churn. Where ordering matters
/// (`blocks` within a page), we carry an explicit ordering column.
pub const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS pages (
        id TEXT PRIMARY KEY,
        parent_id TEXT NULL,
        last_edited_time TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS pages_last_edited ON pages(last_edited_time)",
    // `page_order` is the block's index in BFS discovery order within
    // its owning `page_id`. Render relies on it for section / toggle
    // layout. NULL is tolerated (pre-seeded rows; never-fetched rows)
    // and sorts last.
    "CREATE TABLE IF NOT EXISTS blocks (
        id TEXT PRIMARY KEY,
        parent_id TEXT NULL,
        page_id TEXT NULL,
        page_order INTEGER NULL,
        last_edited_time TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS blocks_page ON blocks(page_id, page_order)",
    "CREATE TABLE IF NOT EXISTS databases (
        id TEXT PRIMARY KEY,
        parent_id TEXT NULL,
        last_edited_time TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS users (
        id TEXT PRIMARY KEY,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS comments (
        id TEXT PRIMARY KEY,
        parent_id TEXT NOT NULL,
        page_id TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS comments_page ON comments(page_id)",
    "CREATE TABLE IF NOT EXISTS blobs (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL CHECK(kind IN ('uploaded','external','notion_hosted')),
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
    )",
    "CREATE TABLE IF NOT EXISTS endpoint_shapes (
        endpoint TEXT PRIMARY KEY,
        example_headers TEXT NULL,
        example_envelope_skeleton TEXT NULL,
        captured_at TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS sync_runs (
        run_id INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at TEXT NOT NULL,
        finished_at TEXT NULL,
        config TEXT NOT NULL,
        status TEXT NOT NULL,
        summary TEXT NULL
    )",
];

/// Resolve the doltlite database path for a given Notion source.
///
/// Accepts either an explicit file path (`*.doltlite_db`) or the legacy
/// directory shape (`<data_root>/raw/<name>`), which is rewritten to a
/// sibling `<name>.doltlite_db` file. Trailing slashes are tolerated.
pub fn db_path_for(p: &Path) -> PathBuf {
    if p.extension().and_then(|s| s.to_str()) == Some("doltlite_db") {
        return p.to_path_buf();
    }
    p.with_extension("doltlite_db")
}

/// Handle on the raw-store sqlite file. Cheap to clone via the pool.
#[derive(Clone)]
pub struct RawDb {
    pool: SqlitePool,
}

/// What the extract loop wants to know about a page before it decides
/// whether to issue a detail fetch.
#[derive(Debug, Clone)]
pub struct PageState {
    pub last_edited_time: Option<String>,
    pub has_payload: bool,
}

/// One row of input to [`RawDb::upsert_blocks`]. `page_order` is the
/// 0-based index of this block within its owning page's BFS walk.
#[derive(Debug, Clone)]
pub struct BlockUpsert {
    pub id: String,
    pub parent_id: Option<String>,
    pub page_id: Option<String>,
    pub page_order: Option<i64>,
    pub last_edited_time: Option<String>,
    pub payload: Option<String>,
}

impl RawDb {
    /// Open (or create) the file at `db_path`, apply DDL idempotently.
    pub async fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
            .with_context(|| format!("sqlite uri for {}", db_path.display()))?
            .create_if_missing(true)
            // The raw store has a single writer (the extract pass) and
            // a single reader (translate/synth, well after extract has
            // exited). No concurrent access → no need for WAL, and we
            // get deterministic on-disk layout: no `-wal` / `-shm`
            // sidecars to make golden snapshots flaky.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .context("open sqlite pool")?;
        for stmt in DDL {
            sqlx::query(stmt)
                .execute(&pool)
                .await
                .with_context(|| format!("apply DDL: {}", stmt.split_once('(').map(|p| p.0).unwrap_or(stmt)))?;
        }
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Record the start of a sync run; returns the new `run_id`.
    pub async fn start_run(&self, config: &Value) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        let cfg = serde_json::to_string(config).context("serialize run config")?;
        let row = sqlx::query(
            "INSERT INTO sync_runs (started_at, config, status) VALUES (?, ?, 'running') RETURNING run_id",
        )
        .bind(&now)
        .bind(&cfg)
        .fetch_one(&self.pool)
        .await
        .context("insert sync_runs")?;
        let id: i64 = row.try_get("run_id").context("read run_id")?;
        Ok(id)
    }

    pub async fn finish_run(
        &self,
        run_id: i64,
        status: &str,
        summary: &Value,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let s = serde_json::to_string(summary).context("serialize run summary")?;
        sqlx::query(
            "UPDATE sync_runs SET finished_at = ?, status = ?, summary = ? WHERE run_id = ?",
        )
        .bind(&now)
        .bind(status)
        .bind(&s)
        .bind(run_id)
        .execute(&self.pool)
        .await
        .context("update sync_runs")?;
        Ok(())
    }

    /// Snapshot every page's last_edited_time + payload-presence flag.
    /// Used at the start of a sync to decide which detail fetches we can
    /// skip.
    pub async fn page_states(&self) -> Result<std::collections::HashMap<String, PageState>> {
        let rows =
            sqlx::query("SELECT id, last_edited_time, payload IS NOT NULL AS has_payload FROM pages")
                .fetch_all(&self.pool)
                .await
                .context("select page_states")?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let last: Option<String> = r.try_get("last_edited_time").ok();
            let has: i64 = r.try_get("has_payload").unwrap_or(0);
            out.insert(
                id,
                PageState {
                    last_edited_time: last,
                    has_payload: has != 0,
                },
            );
        }
        Ok(out)
    }

    /// Pre-seed an `id`-only row (NULL payload) into a table. Used when
    /// we know an entity exists upstream but haven't fetched its body
    /// yet. Existing rows are left untouched (no clobber of payload).
    pub async fn ensure_id(&self, table: &str, id: &str) -> Result<()> {
        let sql = format!(
            "INSERT INTO {table} (id, parent_id) VALUES (?, NULL) ON CONFLICT(id) DO NOTHING"
        );
        // `users` and `comments` have different shapes — caller picks the
        // right table; the only column we touch on conflict is none, and
        // all object tables accept `(id)` as the minimal insert.
        sqlx::query(&sql)
            .bind(id)
            .execute(&self.pool)
            .await
            .with_context(|| format!("ensure_id {table}={id}"))?;
        Ok(())
    }

    /// Batch upsert pages. `rows` is `(id, parent_id, last_edited_time,
    /// payload_json)`. We compare-on-upsert: if the stored
    /// `last_edited_time` already matches, we leave payload alone (the
    /// list pass shouldn't clobber a freshly-fetched detail body with a
    /// truncated list-only payload). When the incoming
    /// `last_edited_time` differs, payload is overwritten verbatim.
    pub async fn upsert_pages(
        &self,
        rows: &[(String, Option<String>, Option<String>, Option<String>)],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin pages tx")?;
        for (id, parent_id, last_edited_time, payload) in rows {
            // When payload is Some, this row came from a detail fetch
            // (full body). When None, it's a discovery upsert that
            // shouldn't blow away a previously-fetched body.
            let sql = if payload.is_some() {
                "INSERT INTO pages (id, parent_id, last_edited_time, payload, fetched_at, last_attempt_at, last_error)
                 VALUES (?, ?, ?, ?, ?, ?, NULL)
                 ON CONFLICT(id) DO UPDATE SET
                    parent_id = COALESCE(excluded.parent_id, pages.parent_id),
                    last_edited_time = excluded.last_edited_time,
                    payload = excluded.payload,
                    fetched_at = excluded.fetched_at,
                    last_attempt_at = excluded.last_attempt_at,
                    last_error = NULL"
            } else {
                "INSERT INTO pages (id, parent_id, last_edited_time)
                 VALUES (?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    parent_id = COALESCE(excluded.parent_id, pages.parent_id),
                    last_edited_time = COALESCE(excluded.last_edited_time, pages.last_edited_time)"
            };
            let mut q = sqlx::query(sql).bind(id).bind(parent_id).bind(last_edited_time);
            if payload.is_some() {
                q = q.bind(payload).bind(&now).bind(&now);
            }
            q.execute(&mut *tx).await.with_context(|| format!("upsert page {id}"))?;
        }
        tx.commit().await.context("commit pages tx")?;
        Ok(())
    }

    /// Batch upsert blocks. Detail-only — list+detail in one shot since
    /// Notion's `/blocks/{id}/children` returns full block bodies.
    /// `page_order` is the block's index in BFS discovery order within
    /// its owning page; render uses it to lay out sections/toggles.
    pub async fn upsert_blocks(&self, rows: &[BlockUpsert]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin blocks tx")?;
        for BlockUpsert {
            id,
            parent_id,
            page_id,
            page_order,
            last_edited_time,
            payload,
        } in rows
        {
            sqlx::query(
                "INSERT INTO blocks (id, parent_id, page_id, page_order, last_edited_time, payload, fetched_at, last_attempt_at, last_error)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL)
                 ON CONFLICT(id) DO UPDATE SET
                    parent_id = COALESCE(excluded.parent_id, blocks.parent_id),
                    page_id = COALESCE(excluded.page_id, blocks.page_id),
                    page_order = COALESCE(excluded.page_order, blocks.page_order),
                    last_edited_time = excluded.last_edited_time,
                    payload = excluded.payload,
                    fetched_at = excluded.fetched_at,
                    last_attempt_at = excluded.last_attempt_at,
                    last_error = NULL",
            )
            .bind(id)
            .bind(parent_id)
            .bind(page_id)
            .bind(page_order)
            .bind(last_edited_time)
            .bind(payload)
            .bind(&now)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("upsert block {id}"))?;
        }
        tx.commit().await.context("commit blocks tx")?;
        Ok(())
    }

    /// Batch upsert comments — also detail-in-list.
    pub async fn upsert_comments(
        &self,
        rows: &[(String, String, Option<String>, String)],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin comments tx")?;
        for (id, parent_id, page_id, payload) in rows {
            sqlx::query(
                "INSERT INTO comments (id, parent_id, page_id, payload, fetched_at, last_attempt_at, last_error)
                 VALUES (?, ?, ?, ?, ?, ?, NULL)
                 ON CONFLICT(id) DO UPDATE SET
                    parent_id = excluded.parent_id,
                    page_id = COALESCE(excluded.page_id, comments.page_id),
                    payload = excluded.payload,
                    fetched_at = excluded.fetched_at,
                    last_attempt_at = excluded.last_attempt_at,
                    last_error = NULL",
            )
            .bind(id)
            .bind(parent_id)
            .bind(page_id)
            .bind(payload)
            .bind(&now)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("upsert comment {id}"))?;
        }
        tx.commit().await.context("commit comments tx")?;
        Ok(())
    }

    /// Bump attempt counters + record an error against a page id. Leaves
    /// any previously-fetched payload intact.
    pub async fn record_page_error(&self, id: &str, err: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO pages (id, attempt_count, last_attempt_at, last_error)
             VALUES (?, 1, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                attempt_count = pages.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at,
                last_error = excluded.last_error",
        )
        .bind(id)
        .bind(&now)
        .bind(err)
        .execute(&self.pool)
        .await
        .with_context(|| format!("record_page_error {id}"))?;
        Ok(())
    }

    /// Page ids that should be re-fetched on a `--retry-failed` run:
    /// rows whose last fetch attempt left an error set, or that have a
    /// NULL payload after at least one attempt.
    pub async fn failed_page_ids(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM pages \
             WHERE last_error IS NOT NULL OR (payload IS NULL AND attempt_count > 0)",
        )
        .fetch_all(&self.pool)
        .await
        .context("select failed_page_ids")?;
        Ok(rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("id").ok())
            .collect())
    }

    /// Snapshot every page's payload JSON. Used by translate/synthesize
    /// to rebuild the in-memory view that the JSONL pipeline produced.
    pub async fn load_pages(&self) -> Result<Vec<Value>> {
        load_payloads(&self.pool, "pages").await
    }

    pub async fn load_blocks(&self) -> Result<Vec<(Value, Option<String>)>> {
        // ORDER BY (page_id, page_order) reproduces BFS discovery
        // order from extract/mod.rs::walk_page_blocks; render relies
        // on this for section / toggle layout. `id` ties the tail so
        // results stay deterministic when page_order is NULL (pre-
        // seeded rows or legacy data fetched before the column existed).
        let rows = sqlx::query(
            "SELECT payload, page_id FROM blocks WHERE payload IS NOT NULL \
             ORDER BY page_id, page_order, id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select blocks")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let page_id: Option<String> = r.try_get("page_id").ok();
            if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                out.push((v, page_id));
            }
        }
        Ok(out)
    }

    pub async fn load_comments(&self) -> Result<Vec<(Value, Option<String>)>> {
        let rows = sqlx::query(
            // Comments don't have a within-page index — render sorts
            // them by created_time anyway — so just deterministic by id.
            "SELECT payload, page_id FROM comments WHERE payload IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select comments")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let page_id: Option<String> = r.try_get("page_id").ok();
            if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                out.push((v, page_id));
            }
        }
        Ok(out)
    }

    /// True iff a blob row with this id already has its bytes stored.
    /// Used to short-circuit refetch: per the design doc, once we have
    /// a copy we trust it (signed URLs rotate; bytes don't).
    pub async fn blob_exists(&self, id: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM blobs WHERE id = ? AND bytes IS NOT NULL LIMIT 1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .context("blob_exists")?;
        Ok(row.is_some())
    }

    /// Insert (or refresh) a blob row with its bytes. `id` is the blob
    /// key the caller chose (today: `{block_id}:{slot}` for inline
    /// references). Errors during fetch should call
    /// [`Self::record_blob_error`] instead.
    pub async fn upsert_blob_bytes(
        &self,
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
        .execute(&self.pool)
        .await
        .with_context(|| format!("upsert_blob_bytes {id}"))?;
        Ok(())
    }

    pub async fn record_blob_error(&self, id: &str, err: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO blobs (id, kind, owning_id, slot, attempt_count, last_attempt_at, last_error)
             VALUES (?, 'external', '', '', 1, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                attempt_count = blobs.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at,
                last_error = excluded.last_error",
        )
        .bind(id)
        .bind(&now)
        .bind(err)
        .execute(&self.pool)
        .await
        .with_context(|| format!("record_blob_error {id}"))?;
        Ok(())
    }

    /// Load every blob row's bytes keyed by owning block id. Used by
    /// translate to write blob bytes alongside the rendered markdown.
    pub async fn load_blobs_by_owner(
        &self,
    ) -> Result<std::collections::HashMap<String, BlobBytes>> {
        let rows = sqlx::query(
            "SELECT id, owning_id, slot, content_type, bytes, source_url \
             FROM blobs WHERE bytes IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("load_blobs_by_owner")?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for r in rows {
            let owning_id: String = match r.try_get("owning_id") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let bytes: Vec<u8> = match r.try_get("bytes") {
                Ok(b) => b,
                Err(_) => continue,
            };
            let id: String = r.try_get("id").unwrap_or_default();
            let slot: String = r.try_get("slot").unwrap_or_default();
            let content_type: Option<String> = r.try_get("content_type").ok();
            let source_url: Option<String> = r.try_get("source_url").ok();
            out.insert(
                owning_id,
                BlobBytes {
                    id,
                    slot,
                    content_type,
                    bytes,
                    source_url,
                },
            );
        }
        Ok(out)
    }

    /// Record (or refresh) the wire-shape skeleton for one endpoint.
    /// Caller is responsible for blanking out data fields in
    /// `envelope_skeleton`.
    pub async fn record_endpoint_shape(
        &self,
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
        .execute(&self.pool)
        .await
        .context("upsert endpoint_shapes")?;
        Ok(())
    }
}

async fn load_payloads(pool: &SqlitePool, table: &str) -> Result<Vec<Value>> {
    // Deterministic by id is fine for pages: render doesn't depend on
    // page-order, and dolt diff stability comes from the PK identity,
    // not the read order.
    let sql = format!("SELECT payload FROM {table} WHERE payload IS NOT NULL ORDER BY id");
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

/// Synchronous helper for non-async callers (translate, synthesize) that
/// already run under `#[tokio::main]`. Uses `block_in_place` + the
/// current Handle, so it must be invoked on a multi-thread runtime.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            let pages = db.load_pages().await?;
            let blocks = db.load_blocks().await?;
            let comments = db.load_comments().await?;
            let blobs_by_owner = db.load_blobs_by_owner().await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                pages,
                blocks,
                comments,
                blobs_by_owner,
            })
        })
    })
}

/// Bag of payload arrays returned by [`block_on_load_all`]; mirrors what
/// the old JSONL `parse_api_dir` produced from the latest-by-id walk.
#[derive(Debug, Default, Clone)]
pub struct LoadedRaw {
    pub pages: Vec<Value>,
    pub blocks: Vec<(Value, Option<String>)>,
    pub comments: Vec<(Value, Option<String>)>,
    /// Keyed by `owning_id` (= the block id that references the file).
    /// Today only image blocks populate this; other media kinds can
    /// follow the same shape.
    pub blobs_by_owner: std::collections::HashMap<String, BlobBytes>,
}

/// Bytes for one blob, paired with the metadata downstream renderers
/// need to write it back to disk and link to it.
#[derive(Debug, Clone)]
pub struct BlobBytes {
    pub id: String,
    pub slot: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub source_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn open_creates_file_and_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        assert!(db_file.exists());
        // Should be empty.
        let pages = db.load_pages().await.unwrap();
        assert!(pages.is_empty());
    }

    #[tokio::test]
    async fn upsert_page_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("x.doltlite_db")).await.unwrap();
        db.upsert_pages(&[(
            "p1".into(),
            Some("root".into()),
            Some("2026-05-21T19:37:00Z".into()),
            Some(serde_json::to_string(&json!({"id": "p1", "title": "hi"})).unwrap()),
        )])
        .await
        .unwrap();
        let states = db.page_states().await.unwrap();
        assert_eq!(states.get("p1").unwrap().has_payload, true);
        let pages = db.load_pages().await.unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["title"], "hi");
    }

    #[tokio::test]
    async fn record_page_error_bumps_attempt_count() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("y.doltlite_db")).await.unwrap();
        db.record_page_error("p1", "boom").await.unwrap();
        db.record_page_error("p1", "boom2").await.unwrap();
        let failed = db.failed_page_ids().await.unwrap();
        assert_eq!(failed, vec!["p1".to_string()]);
    }

    #[tokio::test]
    async fn successful_upsert_clears_last_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("z.doltlite_db")).await.unwrap();
        db.record_page_error("p1", "fail").await.unwrap();
        db.upsert_pages(&[(
            "p1".into(),
            None,
            Some("2026-01-01T00:00:00Z".into()),
            Some("{}".into()),
        )])
        .await
        .unwrap();
        let failed = db.failed_page_ids().await.unwrap();
        assert!(failed.is_empty());
    }

    #[test]
    fn db_path_for_handles_legacy_directory() {
        let p = std::path::Path::new("/tmp/raw/notion-api");
        assert_eq!(
            db_path_for(p),
            std::path::PathBuf::from("/tmp/raw/notion-api.doltlite_db")
        );
        let p2 = std::path::Path::new("/tmp/raw/notion-api.doltlite_db");
        assert_eq!(db_path_for(p2), p2);
    }
}
