//! General per-extract-step "what changed" metrics.
//!
//! The goal is a source-agnostic sense of scale for every sync run:
//! how many API calls a source made, how many rows it wrote, and how
//! the on-disk raw store grew — *without* a single line of
//! data-source-specific counting code. Two complementary mechanisms get
//! us there:
//!
//!   1. **Live counters at the shared write/HTTP chokepoints.** A
//!      [`tokio::task_local`] holds an [`ExtractMetrics`] for the
//!      duration of one source's extract (installed by [`scope`]). The
//!      three chokepoints every provider funnels through —
//!      [`crate::http::latchkey_curl`] (API calls),
//!      [`crate::bulk::bulk_upsert_entity_in_tx`] (entity rows), and
//!      [`crate::blob_cas::BlobCas::put_many`]/`put` (CAS blobs) — call
//!      [`record_api_request`] / [`record_upserts`], which add into the
//!      ambient context if one is installed and are a silent no-op
//!      otherwise (tests, standalone CLIs, the translate phase). No
//!      provider knows these exist.
//!
//!   2. **before/after snapshots of the db files themselves.**
//!      [`snapshot_db_file`] opens a throwaway read-only connection and
//!      `COUNT(*)`s every table (plus the file's byte size). Taken once
//!      before any writer opens and once after the source commits, the
//!      delta (`rows_after - rows_before`, i.e. [`TableStats::rows_net`])
//!      is the authoritative, universal "what changed" — it captures
//!      *every* table, including ones a provider writes with hand-rolled
//!      SQL that bypasses the bulk chokepoint, and the bookkeeping
//!      sidecars.
//!
//! `rows_upserted` (mechanism 1) is therefore "rows written through the
//! shared bulk/CAS chokepoints" and may read 0 for a table a provider
//! populates with its own `INSERT`; `rows_net` (mechanism 2) always
//! reflects the real change. Both are reported per table.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

use crate::progress::ProgressSink;

// ─────────────────────────────────────────────────────────────────────
// Live counters + ambient task-local context
// ─────────────────────────────────────────────────────────────────────

/// Per-source live counters, accumulated at the shared chokepoints for
/// the duration of one source's extract. Cheap to clone behind the
/// `Arc` the orchestrator hands out.
#[derive(Default)]
pub struct ExtractMetrics {
    /// Total requests issued through [`crate::http::latchkey_curl`].
    /// Stays 0 for file-based ingestion (mbox, vCard, Signal, WhatsApp),
    /// which never touches the network transport.
    api_requests: AtomicU64,
    /// Rows passed through the entity/CAS upsert chokepoints, keyed by
    /// table (`cas_objects` for the blob store). Counts *attempts* —
    /// some are no-op updates / `INSERT OR IGNORE` dupes — which is the
    /// requested "rows_upserted (some upserts may be updates)" signal.
    rows_upserted: Mutex<BTreeMap<String, u64>>,
    /// The real progress sink for this source's top-level bar, stored so
    /// a chokepoint can re-render the live suffix as counters move.
    /// `None` until [`ExtractMetrics::attach_bar`] runs (e.g. headless).
    bar: Mutex<Option<Arc<dyn ProgressSink>>>,
    /// The latest message the provider set on its bar, so re-renders
    /// triggered by counter updates don't clobber it.
    provider_msg: Mutex<String>,
}

impl ExtractMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn record_api_request(&self) {
        self.api_requests.fetch_add(1, Ordering::Relaxed);
        self.render();
    }

    pub fn record_upserts(&self, table: &str, n: u64) {
        if n == 0 {
            return;
        }
        {
            let mut m = self.rows_upserted.lock().unwrap();
            *m.entry(table.to_string()).or_insert(0) += n;
        }
        self.render();
    }

    pub fn api_requests(&self) -> u64 {
        self.api_requests.load(Ordering::Relaxed)
    }

    pub fn rows_upserted_total(&self) -> u64 {
        self.rows_upserted.lock().unwrap().values().sum()
    }

    pub fn rows_upserted_snapshot(&self) -> BTreeMap<String, u64> {
        self.rows_upserted.lock().unwrap().clone()
    }

    /// Wire the source's real bar sink in so counter updates can refresh
    /// the live `api=… rows[…]` suffix. The orchestrator calls this once,
    /// before installing the [`MetricsSink`] wrapper on the bar.
    pub fn attach_bar(&self, sink: Arc<dyn ProgressSink>) {
        *self.bar.lock().unwrap() = Some(sink);
    }

    /// Record the provider's latest bar message (called by
    /// [`MetricsSink`]) and re-render with the metrics suffix appended.
    fn set_provider_message(&self, msg: &str) {
        *self.provider_msg.lock().unwrap() = msg.to_string();
        self.render();
    }

    /// Compose `"<provider msg>  ·  api=N rows[t=n …]"`, omitting empty
    /// pieces. The suffix is what makes the live counters visible on the
    /// per-source bar.
    fn compose(&self) -> String {
        let msg = self.provider_msg.lock().unwrap().clone();
        let api = self.api_requests.load(Ordering::Relaxed);
        let rows = self.rows_upserted.lock().unwrap();
        let mut suffix = String::new();
        if api > 0 {
            suffix.push_str(&format!("api={api}"));
        }
        if !rows.is_empty() {
            if !suffix.is_empty() {
                suffix.push(' ');
            }
            let parts: Vec<String> = rows.iter().map(|(t, n)| format!("{t}={n}")).collect();
            suffix.push_str(&format!("rows[{}]", parts.join(" ")));
        }
        match (msg.is_empty(), suffix.is_empty()) {
            (_, true) => msg,
            (true, false) => suffix,
            (false, false) => format!("{msg}  ·  {suffix}"),
        }
    }

    fn render(&self) {
        let sink = self.bar.lock().unwrap().clone();
        if let Some(sink) = sink {
            sink.set_message(&self.compose());
        }
    }
}

impl std::fmt::Debug for ExtractMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractMetrics")
            .field("api_requests", &self.api_requests.load(Ordering::Relaxed))
            .field("rows_upserted", &self.rows_upserted.lock().unwrap())
            .finish_non_exhaustive()
    }
}

tokio::task_local! {
    static CURRENT: Arc<ExtractMetrics>;
}

/// Install `metrics` as the ambient extract-metrics context for the
/// duration of `fut`. Chokepoints invoked anywhere within `fut` (on the
/// same task) record into it. Everything outside any `scope` is a no-op.
pub async fn scope<F>(metrics: Arc<ExtractMetrics>, fut: F) -> F::Output
where
    F: Future,
{
    CURRENT.scope(metrics, fut).await
}

fn with_current<R>(f: impl FnOnce(&ExtractMetrics) -> R) -> Option<R> {
    CURRENT.try_with(|m| f(m)).ok()
}

/// Count one outbound API request against the current source, if a
/// metrics context is installed. Called from [`crate::http::latchkey_curl`].
pub fn record_api_request() {
    let _ = with_current(ExtractMetrics::record_api_request);
}

/// Count `n` row upserts into `table` against the current source, if a
/// metrics context is installed. Called from the bulk/CAS chokepoints.
pub fn record_upserts(table: &str, n: usize) {
    let _ = with_current(|m| m.record_upserts(table, n as u64));
}

// ─────────────────────────────────────────────────────────────────────
// Live-suffix progress sink
// ─────────────────────────────────────────────────────────────────────

/// Wraps a source's top-level bar so every `set_message` the provider
/// emits gets the live `api=… rows[…]` suffix appended. All other calls
/// pass straight through, and `child` returns the unwrapped inner sink
/// so nested per-unit bars stay clean.
pub struct MetricsSink {
    inner: Arc<dyn ProgressSink>,
    metrics: Arc<ExtractMetrics>,
}

impl MetricsSink {
    pub fn new(inner: Arc<dyn ProgressSink>, metrics: Arc<ExtractMetrics>) -> Self {
        Self { inner, metrics }
    }
}

impl ProgressSink for MetricsSink {
    fn set_length(&self, total: Option<u64>) {
        self.inner.set_length(total);
    }
    fn inc(&self, delta: u64) {
        self.inner.inc(delta);
    }
    fn set_message(&self, msg: &str) {
        // Store + recompose, then emit via the metrics' own render path
        // (which targets the same inner sink) so the message and the
        // counter suffix always render together.
        self.metrics.set_provider_message(msg);
    }
    fn finish(&self, msg: &str) {
        self.inner.finish(msg);
    }
    fn finish_and_clear(&self) {
        self.inner.finish_and_clear();
    }
    fn child(&self, prefix: &str) -> Arc<dyn ProgressSink> {
        self.inner.child(prefix)
    }
}

// ─────────────────────────────────────────────────────────────────────
// db-file snapshots
// ─────────────────────────────────────────────────────────────────────

/// A point-in-time view of one doltlite db file: its byte size and the
/// row count of every table. Used as the before/after endpoints whose
/// delta is the universal "what changed".
#[derive(Debug, Clone, Default)]
pub struct DbSnapshot {
    pub bytes: u64,
    pub rows: BTreeMap<String, u64>,
}

/// Snapshot `path`: file size via `metadata`, per-table `COUNT(*)` via a
/// throwaway **read-only** connection (so it never contends with the
/// writer's single connection for the file lock). Entirely best-effort —
/// a missing file yields an empty snapshot, and any query error degrades
/// to "what we got so far" rather than failing the extract.
pub async fn snapshot_db_file(path: &Path) -> DbSnapshot {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut rows = BTreeMap::new();
    if !path.exists() {
        return DbSnapshot { bytes, rows };
    }
    match read_only_pool(path).await {
        Ok(pool) => {
            if let Ok(tables) = list_tables(&pool).await {
                for t in tables {
                    if let Some(n) = count_rows(&pool, &t).await {
                        rows.insert(t, n);
                    }
                }
            }
            pool.close().await;
        }
        Err(e) => {
            tracing::debug!(
                path = %path.display(),
                error = %format!("{e:#}"),
                "extract_metrics: read-only snapshot open failed; bytes-only"
            );
        }
    }
    DbSnapshot { bytes, rows }
}

async fn read_only_pool(path: &Path) -> anyhow::Result<sqlx::SqlitePool> {
    // No journal_mode / pragmas: doltlite rejects them. read_only opens
    // with SQLITE_OPEN_READONLY (a flag, not a pragma) so we coexist
    // with the writer instead of grabbing its lock.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .read_only(true)
        .create_if_missing(false);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect_with(opts)
        .await?;
    Ok(pool)
}

async fn list_tables(pool: &sqlx::SqlitePool) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(|r| r.try_get(0).ok()).collect())
}

async fn count_rows(pool: &sqlx::SqlitePool, table: &str) -> Option<u64> {
    // Table names come from sqlite_master, but double-quote anyway so an
    // exotic identifier can't break the statement.
    let sql = format!("SELECT COUNT(*) FROM \"{}\"", table.replace('"', "\"\""));
    let row = sqlx::query(&sql).fetch_one(pool).await.ok()?;
    let n: i64 = row.try_get(0).ok()?;
    Some(n.max(0) as u64)
}

/// Snapshot a source's pair of raw-store files (entity + sibling CAS).
/// `entity_path` is the `<name>.doltlite_db`; the CAS path is derived.
pub async fn snapshot_source(entity_path: &Path) -> (DbSnapshot, DbSnapshot) {
    let blobs_path = crate::blob_cas::cas_path_for(entity_path);
    let events = snapshot_db_file(entity_path).await;
    let blobs = snapshot_db_file(&blobs_path).await;
    (events, blobs)
}

// ─────────────────────────────────────────────────────────────────────
// Report
// ─────────────────────────────────────────────────────────────────────

/// Per-table change for one db file.
#[derive(Debug, Clone)]
pub struct TableStats {
    pub table: String,
    pub rows_before: u64,
    /// Rows written via the shared bulk/CAS chokepoints (attempts).
    pub rows_upserted: u64,
    pub rows_after: u64,
}

impl TableStats {
    /// Net change in row count — the authoritative "what changed",
    /// independent of whether writes went through the chokepoint.
    pub fn rows_net(&self) -> i64 {
        self.rows_after as i64 - self.rows_before as i64
    }
    fn to_json(&self) -> Value {
        json!({
            "table": self.table,
            "rows_before": self.rows_before,
            "rows_upserted": self.rows_upserted,
            "rows_after": self.rows_after,
            "rows_net": self.rows_net(),
        })
    }
}

/// One db file's before/after picture.
#[derive(Debug, Clone)]
pub struct DbFileReport {
    pub label: String,
    pub path: PathBuf,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub tables: Vec<TableStats>,
}

impl DbFileReport {
    fn to_json(&self) -> Value {
        json!({
            "label": self.label,
            "path": self.path.display().to_string(),
            "bytes_before": self.bytes_before,
            "bytes_after": self.bytes_after,
            "bytes_delta": self.bytes_after as i64 - self.bytes_before as i64,
            "tables": self.tables.iter().map(TableStats::to_json).collect::<Vec<_>>(),
        })
    }
    fn is_empty(&self) -> bool {
        self.bytes_before == 0 && self.bytes_after == 0 && self.tables.is_empty()
    }
}

/// The full per-source extract report: API call count plus the
/// before/after picture of each raw-store db file.
#[derive(Debug, Clone)]
pub struct ExtractReport {
    pub api_requests: u64,
    pub rows_upserted_total: u64,
    pub dbs: Vec<DbFileReport>,
}

impl ExtractReport {
    pub fn to_json(&self) -> Value {
        json!({
            "api_requests": self.api_requests,
            "rows_upserted_total": self.rows_upserted_total,
            "dbs": self.dbs.iter().map(DbFileReport::to_json).collect::<Vec<_>>(),
        })
    }

    /// True when nothing was measured (no files, no api) — e.g. a
    /// file-tree-backed source with no doltlite store. The caller can
    /// drop the report entirely in that case.
    pub fn is_empty(&self) -> bool {
        self.api_requests == 0
            && self.rows_upserted_total == 0
            && self.dbs.iter().all(DbFileReport::is_empty)
    }

    /// A compact one-line summary for the INFO log emitted as a source's
    /// progress bar goes away.
    pub fn summary_line(&self) -> String {
        let mut parts = vec![format!("api={}", self.api_requests)];
        for db in &self.dbs {
            let net: i64 = db.tables.iter().map(TableStats::rows_net).sum();
            let upserted: u64 = db.tables.iter().map(|t| t.rows_upserted).sum();
            parts.push(format!(
                "{}(bytes {}->{} rows_net={:+} upserted={})",
                db.label, db.bytes_before, db.bytes_after, net, upserted
            ));
        }
        parts.join(" ")
    }
}

/// Build one [`DbFileReport`] from before/after snapshots, attributing
/// `upserts` to whichever tables actually exist in this file.
fn build_db_file_report(
    label: &str,
    path: &Path,
    before: &DbSnapshot,
    after: &DbSnapshot,
    upserts: &BTreeMap<String, u64>,
) -> DbFileReport {
    let mut names: BTreeSet<&String> = BTreeSet::new();
    names.extend(before.rows.keys());
    names.extend(after.rows.keys());
    // Upsert-only tables (created then dropped, or write that didn't
    // change the count) still belong if they name a table in this file.
    for k in upserts.keys() {
        if before.rows.contains_key(k) || after.rows.contains_key(k) {
            names.insert(k);
        }
    }
    let tables = names
        .into_iter()
        .map(|t| TableStats {
            table: t.clone(),
            rows_before: before.rows.get(t).copied().unwrap_or(0),
            rows_upserted: upserts.get(t).copied().unwrap_or(0),
            rows_after: after.rows.get(t).copied().unwrap_or(0),
        })
        .collect();
    DbFileReport {
        label: label.to_string(),
        path: path.to_path_buf(),
        bytes_before: before.bytes,
        bytes_after: after.bytes,
        tables,
    }
}

/// Assemble the final report: re-snapshot the source's files (the
/// "after" endpoint) and fold in the live counters. Safe to call once
/// the source has committed and released its writer — the after-snapshot
/// uses an independent read-only connection.
pub async fn assemble_report(
    entity_path: &Path,
    before_events: &DbSnapshot,
    before_blobs: &DbSnapshot,
    metrics: &ExtractMetrics,
) -> ExtractReport {
    let blobs_path = crate::blob_cas::cas_path_for(entity_path);
    let after_events = snapshot_db_file(entity_path).await;
    let after_blobs = snapshot_db_file(&blobs_path).await;
    let upserts = metrics.rows_upserted_snapshot();
    ExtractReport {
        api_requests: metrics.api_requests(),
        rows_upserted_total: metrics.rows_upserted_total(),
        dbs: vec![
            build_db_file_report(
                "events",
                entity_path,
                before_events,
                &after_events,
                &upserts,
            ),
            build_db_file_report("blobs", &blobs_path, before_blobs, &after_blobs, &upserts),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_counts_tables_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite");
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE a (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE b (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        for i in 0..3 {
            sqlx::query("INSERT INTO a (id) VALUES (?)")
                .bind(i)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;

        let snap = snapshot_db_file(&path).await;
        assert!(snap.bytes > 0);
        assert_eq!(snap.rows.get("a").copied(), Some(3));
        assert_eq!(snap.rows.get("b").copied(), Some(0));
    }

    #[tokio::test]
    async fn missing_file_is_empty_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snap = snapshot_db_file(&dir.path().join("nope.sqlite")).await;
        assert_eq!(snap.bytes, 0);
        assert!(snap.rows.is_empty());
    }

    #[tokio::test]
    async fn counters_only_record_inside_scope() {
        // Outside any scope: silent no-op (must not panic).
        record_api_request();
        record_upserts("messages", 5);

        let metrics = ExtractMetrics::new();
        let m2 = metrics.clone();
        scope(metrics, async move {
            record_api_request();
            record_api_request();
            record_upserts("messages", 10);
            record_upserts("messages", 5);
            record_upserts("cas_objects", 2);
        })
        .await;
        assert_eq!(m2.api_requests(), 2);
        let snap = m2.rows_upserted_snapshot();
        assert_eq!(snap.get("messages").copied(), Some(15));
        assert_eq!(snap.get("cas_objects").copied(), Some(2));
        assert_eq!(m2.rows_upserted_total(), 17);
    }

    #[test]
    fn report_merges_upserts_and_net() {
        let before = DbSnapshot {
            bytes: 100,
            rows: BTreeMap::from([("messages".to_string(), 10)]),
        };
        let after = DbSnapshot {
            bytes: 200,
            rows: BTreeMap::from([("messages".to_string(), 25)]),
        };
        let mut upserts = BTreeMap::new();
        upserts.insert("messages".to_string(), 20u64);
        upserts.insert("cas_objects".to_string(), 9u64); // wrong file: ignored

        let r = build_db_file_report(
            "events",
            Path::new("/x.doltlite_db"),
            &before,
            &after,
            &upserts,
        );
        assert_eq!(r.bytes_before, 100);
        assert_eq!(r.bytes_after, 200);
        assert_eq!(r.tables.len(), 1);
        let t = &r.tables[0];
        assert_eq!(t.table, "messages");
        assert_eq!(t.rows_before, 10);
        assert_eq!(t.rows_after, 25);
        assert_eq!(t.rows_upserted, 20);
        assert_eq!(t.rows_net(), 15);
    }
}
