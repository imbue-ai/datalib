//! [`DownloadRun`] — bookkeeping wrapper for provider `download::fetch` calls.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::doltlite_raw::{finish_run, has_dolt_extensions, start_run};
use crate::scope_state;

#[derive(Debug, Default, Clone, Serialize)]
pub struct RowDelta {
    pub added: u64,
    pub modified: u64,
    pub removed: u64,
}

pub struct DownloadRun<'p> {
    run_id: i64,
    pool: &'p SqlitePool,
    started: std::time::Instant,
    /// `sync_scope_state` snapshot taken right after `start_run`;
    /// diffed against another snapshot at `finish` time so the
    /// resulting `cursors` summary records every scope that moved
    /// during this run.
    cursors_before: HashMap<String, String>,
}

impl<'p> DownloadRun<'p> {
    pub async fn start(pool: &'p SqlitePool, config: &Value) -> Result<Self> {
        let run_id = start_run(pool, config).await?;
        let cursors_before = scope_state::snapshot(pool).await.unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), "scope_state snapshot at start failed");
            HashMap::new()
        });
        Ok(Self {
            run_id,
            pool,
            started: std::time::Instant::now(),
            cursors_before,
        })
    }

    pub fn run_id(&self) -> i64 {
        self.run_id
    }

    pub async fn finish<S, T>(self, result: &Result<T>, summary: &S)
    where
        S: Serialize,
    {
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let status = if result.is_ok() { "ok" } else { "error" };
        let deltas = compute_deltas(self.pool).await;
        let cursors_after = scope_state::snapshot(self.pool).await.unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), "scope_state snapshot at finish failed");
            HashMap::new()
        });
        let cursor_moves = scope_state::diff(self.cursors_before, cursors_after);
        let mut summary_json = serde_json::to_value(summary).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut summary_json {
            map.insert("elapsed_ms".into(), Value::from(elapsed_ms));
            if let Some(d) = deltas {
                map.insert(
                    "deltas".into(),
                    serde_json::to_value(d).unwrap_or(Value::Null),
                );
            }
            if !cursor_moves.is_empty() {
                map.insert(
                    "cursors".into(),
                    serde_json::to_value(&cursor_moves).unwrap_or(Value::Null),
                );
            }
            if let Err(e) = result {
                map.insert("error".into(), Value::from(format!("{e:#}")));
            }
        }
        if let Err(e) = finish_run(self.pool, self.run_id, status, &summary_json).await {
            tracing::warn!(
                run_id = self.run_id,
                error = %format!("{e:#}"),
                "download_run: finish_run bookkeeping failed"
            );
        }
    }
}

/// Query `dolt_status` for dirty tables, then `dolt_diff_<table>` for
/// per-diff-type counts since the last `dolt_commit`. Returns `None`
/// against stock libsqlite3 (no dolt extensions); returns `Some({})`
/// against doltlite when nothing's dirty. Individual table queries
/// that fail are logged and skipped — we never want a single bad
/// virtual table read to drop the rest of the summary.
async fn compute_deltas(pool: &SqlitePool) -> Option<BTreeMap<String, RowDelta>> {
    if !has_dolt_extensions(pool).await {
        return None;
    }
    let dirty: Vec<(String, i64, String)> =
        match sqlx::query_as("SELECT table_name, staged, status FROM dolt_status")
            .fetch_all(pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "dolt_status read failed");
                return None;
            }
        };
    let mut out: BTreeMap<String, RowDelta> = BTreeMap::new();
    for (table, _staged, _status) in dirty {
        // Guard against any table name that wouldn't be a safe
        // identifier in the dynamic `dolt_diff_<table>` query. Dolt's
        // own naming for these virtual tables matches the underlying
        // table's identifier rules — so an unsafe name here would mean
        // an unsafe table name made it past our DDL, which is a
        // separate bug, but we still skip rather than risk an
        // injection-flavored failure mode.
        if !is_safe_identifier(&table) {
            tracing::warn!(table = %table, "skipping delta for unsafe-identifier table name");
            continue;
        }
        let sql = format!(
            "SELECT diff_type, COUNT(*) FROM dolt_diff_{table} \
             WHERE to_commit = 'WORKING' GROUP BY diff_type"
        );
        // Audited: `table` is interpolated into the `dolt_diff_<table>` vtab name,
        // and the `is_safe_identifier` guard above rejects anything that is not a
        // plain identifier before we get here.
        let rows: Vec<(String, i64)> = match sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_all(pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(table = %table, error = %format!("{e:#}"),
                    "dolt_diff_<table> read failed; delta dropped");
                continue;
            }
        };
        let mut d = RowDelta::default();
        for (kind, n) in rows {
            let n = n.max(0) as u64;
            match kind.as_str() {
                "added" => d.added = n,
                "modified" => d.modified = n,
                "removed" => d.removed = n,
                _ => {}
            }
        }
        out.insert(table, d);
    }
    Some(out)
}

fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Use the canonical `open()` helper (with a tempdir-backed file)
    // instead of `sqlite::memory:` — doltlite's libsqlite3 fork rejects
    // `:memory:` (the prolly storage engine needs a real path), but
    // stock libsqlite3-sys accepts it. A tempfile works under both.
    async fn fresh_pool() -> (SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("extract_run_test.doltlite_db");
        let pool = crate::doltlite_raw::open(&path, &[]).await.unwrap();
        (pool, dir)
    }

    #[derive(Serialize)]
    struct DummySummary {
        new: usize,
        skipped: usize,
    }

    #[tokio::test]
    async fn ok_path_writes_ok_status_with_elapsed_and_summary_fields() {
        let (pool, _dir) = fresh_pool().await;
        let run = DownloadRun::start(&pool, &json!({"k": "v"})).await.unwrap();
        let run_id = run.run_id();
        let work_result: Result<()> = Ok(());
        let summary = DummySummary { new: 7, skipped: 3 };
        run.finish(&work_result, &summary).await;

        let row: (String, String) =
            sqlx::query_as("SELECT status, summary FROM sync_runs WHERE run_id = ?")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "ok");
        let s: Value = serde_json::from_str(&row.1).unwrap();
        assert_eq!(s["new"], 7);
        assert_eq!(s["skipped"], 3);
        // elapsed_ms always merged in.
        assert!(s["elapsed_ms"].is_number());
        // No error field on the ok path.
        assert!(s.get("error").is_none());
    }

    #[tokio::test]
    async fn deltas_reflect_uncommitted_writes_when_dolt_extensions_present() {
        // Doltlite-only test: against stock libsqlite3 there are no
        // dolt_status / dolt_diff_<table> virtual tables, so
        // `deltas` would simply be absent (asserted in the ok-path
        // test above). Under bazel (where libsqlite3-sys is linked
        // against doltlite) it's the real thing.
        let (pool, _dir) = fresh_pool().await;
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return;
        }
        sqlx::query(crate::doltlite_raw::SYNC_SCOPE_STATE_DDL)
            .execute(&pool)
            .await
            .unwrap();
        // Establish a baseline commit so subsequent inserts show up
        // as a clean "added since HEAD" delta — without this, dolt's
        // view of "what changed" includes the schema-creation step
        // itself and the result is less predictable for assertion.
        crate::doltlite_raw::commit_run(&pool, "baseline")
            .await
            .unwrap();

        let run = DownloadRun::start(&pool, &json!({"k": "v"})).await.unwrap();
        let run_id = run.run_id();
        sqlx::query("INSERT INTO sync_scope_state (scope, last_seen_at) VALUES (?, ?)")
            .bind("test_scope")
            .bind("2026-01-01T00:00:00Z")
            .execute(&pool)
            .await
            .unwrap();
        let work_result: Result<()> = Ok(());
        let summary = DummySummary { new: 0, skipped: 0 };
        run.finish(&work_result, &summary).await;

        let row: (String,) = sqlx::query_as("SELECT summary FROM sync_runs WHERE run_id = ?")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let s: Value = serde_json::from_str(&row.0).unwrap();
        let deltas = s
            .get("deltas")
            .and_then(|v| v.as_object())
            .expect("deltas object present in summary");
        // sync_scope_state: 1 row inserted since baseline.
        let scope_state = deltas
            .get("sync_scope_state")
            .unwrap_or_else(|| panic!("sync_scope_state missing in deltas: {deltas:?}"));
        assert_eq!(
            scope_state["added"], 1,
            "sync_scope_state should show 1 added row; got {scope_state}"
        );
        // sync_runs: DownloadRun::start added one row, finish UPDATEd
        // it. Net for dolt is one added row.
        let sync_runs = deltas
            .get("sync_runs")
            .unwrap_or_else(|| panic!("sync_runs missing in deltas: {deltas:?}"));
        assert_eq!(
            sync_runs["added"], 1,
            "sync_runs should show 1 added row; got {sync_runs}"
        );
    }

    #[tokio::test]
    async fn err_path_writes_error_status_and_keeps_partial_summary() {
        let (pool, _dir) = fresh_pool().await;
        let run = DownloadRun::start(&pool, &json!({"k": "v"})).await.unwrap();
        let run_id = run.run_id();
        let work_result: Result<()> = Err(anyhow::anyhow!("upstream 503"));
        // Even on error, the partial summary the provider populated
        // before the failure is preserved — that's what makes
        // post-mortem analysis possible.
        let summary = DummySummary { new: 4, skipped: 0 };
        run.finish(&work_result, &summary).await;

        let row: (String, String) =
            sqlx::query_as("SELECT status, summary FROM sync_runs WHERE run_id = ?")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "error");
        let s: Value = serde_json::from_str(&row.1).unwrap();
        assert_eq!(s["new"], 4);
        assert_eq!(s["error"].as_str().unwrap(), "upstream 503");
    }

    #[tokio::test]
    async fn new_table_first_run_reports_added_rows_not_dropped_delta() {
        // Repro for the `dolt_diff_<table> read failed; delta dropped`
        // warning seen on every provider's *first* sync. A table created
        // during a run but not yet committed shows up in `dolt_status` as
        // "new table", but doltlite hasn't materialized its
        // `dolt_diff_<table>` virtual table yet (it only exists for tables
        // present at HEAD). The diff query in `compute_deltas` then errors
        // with "no such table: dolt_diff_<table>" and the row delta is
        // silently dropped.
        const NEW_TABLE_DDL: &str =
            "CREATE TABLE IF NOT EXISTS discussions (id TEXT PRIMARY KEY, payload TEXT)";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new_table_first_run.doltlite_db");
        let pool = crate::doltlite_raw::open(&path, &[NEW_TABLE_DDL])
            .await
            .unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            // Stock libsqlite3 has no dolt_* virtual tables; nothing to test.
            return;
        }
        // Simulate a first-run download: insert rows into the just-created
        // table WITHOUT an intervening commit.
        for i in 0..3 {
            sqlx::query("INSERT INTO discussions (id, payload) VALUES (?, ?)")
                .bind(format!("d{i}"))
                .bind("{}")
                .execute(&pool)
                .await
                .unwrap();
        }
        let deltas = compute_deltas(&pool)
            .await
            .expect("doltlite extensions present => Some(deltas)");
        let d = deltas.get("discussions").unwrap_or_else(|| {
            panic!("discussions delta was dropped instead of reported as added; got {deltas:?}")
        });
        assert_eq!(
            d.added, 3,
            "all 3 rows of the newly-created table should count as added; got {d:?}"
        );
    }
}
