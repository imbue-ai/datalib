//! [`ExtractRun`] — bookkeeping wrapper for provider `extract::fetch` calls.
//!
//! Every provider used to repeat the same ~12-line dance:
//!
//! ```ignore
//! let run_id = db.start_run(&run_config).await?;
//! let result = work.await;
//! let summary_json = serde_json::json!({ /* fields per provider */ });
//! let status = if result.is_ok() { "ok" } else { "error" };
//! let _ = db.finish_run(run_id, status, &summary_json).await;
//! result?;
//! Ok(summary)
//! ```
//!
//! `ExtractRun` collapses that to three lines:
//!
//! ```ignore
//! let run = ExtractRun::start(db.pool(), &run_config).await?;
//! let result = work.await;
//! run.finish(&result, &summary).await;
//! result?;
//! Ok(summary)
//! ```
//!
//! Beyond saving boilerplate, the wrapper:
//!
//! - Stamps `elapsed_ms` into every summary automatically. Per-source
//!   timing was previously only visible in extract-orchestrator logs;
//!   now it lives next to the structured summary in
//!   `sync_runs.summary`.
//! - Merges an `error` field into the summary on failure (preserving
//!   any partial summary fields the provider populated before the
//!   error). Stock providers used to drop the partial summary on
//!   error, which made post-mortem analysis harder than necessary.
//! - Provides a single place to hang future cross-provider concerns
//!   (per-table row deltas via `dolt_diff_<table>`, scope cursor
//!   tracking, etc.) without touching every provider.
//!
//! Failures in the bookkeeping path itself (the `finish_run` SQL
//! update) are **logged and swallowed**: we never want a bookkeeping
//! write to mask whatever error the work future actually returned.

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::doltlite_raw::{finish_run, start_run};

pub struct ExtractRun<'p> {
    run_id: i64,
    pool: &'p SqlitePool,
    started: std::time::Instant,
}

impl<'p> ExtractRun<'p> {
    /// Stamp a `running` row in `sync_runs` with `config`, capture
    /// `run_id` + the wall-clock start, and return the handle.
    pub async fn start(pool: &'p SqlitePool, config: &Value) -> Result<Self> {
        let run_id = start_run(pool, config).await?;
        Ok(Self {
            run_id,
            pool,
            started: std::time::Instant::now(),
        })
    }

    /// `run_id` of the `sync_runs` row. Most providers don't need
    /// this; exposed for the rare provider that wants to stamp it on
    /// per-row writes for audit-log correlation.
    pub fn run_id(&self) -> i64 {
        self.run_id
    }

    /// Finalize: update `sync_runs.{finished_at, status, summary}`.
    ///
    /// - `result` is the work future's outcome. Its variant decides
    ///   `status` (`"ok"` or `"error"`); on error the message is
    ///   merged into the summary as `"error": "<chain>"`.
    /// - `summary` is the provider's typed summary struct. It must be
    ///   `Serialize`; the serialized object is what lands in
    ///   `sync_runs.summary` (plus the auto `elapsed_ms` / `error`
    ///   merges).
    ///
    /// Consumes `self` so a finished run can't accidentally be
    /// finished twice.
    pub async fn finish<S, T>(self, result: &Result<T>, summary: &S)
    where
        S: Serialize,
    {
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let status = if result.is_ok() { "ok" } else { "error" };
        let mut summary_json = serde_json::to_value(summary).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut summary_json {
            map.insert("elapsed_ms".into(), Value::from(elapsed_ms));
            if let Err(e) = result {
                map.insert("error".into(), Value::from(format!("{e:#}")));
            }
        }
        if let Err(e) = finish_run(self.pool, self.run_id, status, &summary_json).await {
            tracing::warn!(
                run_id = self.run_id,
                error = %format!("{e:#}"),
                "extract_run: finish_run bookkeeping failed"
            );
        }
    }
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
        let run = ExtractRun::start(&pool, &json!({"k": "v"})).await.unwrap();
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
    async fn err_path_writes_error_status_and_keeps_partial_summary() {
        let (pool, _dir) = fresh_pool().await;
        let run = ExtractRun::start(&pool, &json!({"k": "v"})).await.unwrap();
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
}
