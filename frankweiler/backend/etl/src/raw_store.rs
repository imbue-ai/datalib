//! Shared helpers for doltlite-backed data sources — the "easy button" that
//! lets every such source follow one storage-ownership pattern under the
//! [`crate::processor`] model.
//!
//! Program A's rule is that the orchestrator is storage-agnostic: a source that
//! keeps a doltlite store owns it end to end (open, schema, write, commit,
//! before/after snapshot, "what changed" report) and exposes only opaque seams
//! — an interrupt [`Checkpoint`] and a published [`ExtractReport`] — so the
//! orchestrator never reads the store.
//!
//! [`RawStoreSession`] is that easy button: open it over a source's write pool
//! (captures the before-snapshot, registers the interrupt hook), then
//! `finish(ctx, summary)` after the fetch (commit + after-snapshot + assemble
//! the report + publish it + close). The interrupt hook ([`Checkpoint`]) does
//! the same commit + report on Ctrl-C, so both paths are source-side.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use frankweiler_obs::diagnostics::Diagnostics;
use sqlx::sqlite::SqlitePool;

use crate::extract_metrics::{
    assemble_report, snapshot_source, DbSnapshot, ExtractMetrics, ExtractReport,
};
use crate::processor::{Checkpoint, RunCtx};

/// A doltlite raw-store session owned by a single extract processor. Captures
/// the before-snapshot at [`open`](RawStoreSession::open), commits + snapshots +
/// reports at [`finish`](RawStoreSession::finish), and exposes an interrupt
/// [`Checkpoint`] that does the same on Ctrl-C — all source-side.
pub struct RawStoreSession {
    pool: SqlitePool,
    entity_path: PathBuf,
    before_events: DbSnapshot,
    before_blobs: DbSnapshot,
    source_name: String,
    metrics: Arc<ExtractMetrics>,
    diagnostics: Arc<Diagnostics>,
}

impl RawStoreSession {
    /// Open over a source's write `pool` (entity doltlite at `entity_path`):
    /// capture the before-snapshot and register the interrupt-commit
    /// `Checkpoint`. Prefer [`RunCtx::open_store`](crate::processor::RunCtx::open_store).
    pub async fn open(pool: SqlitePool, entity_path: PathBuf, ctx: &RunCtx<'_>) -> Self {
        let (before_events, before_blobs) = snapshot_source(&entity_path).await;
        let session = Self {
            pool,
            entity_path,
            before_events,
            before_blobs,
            source_name: ctx.name.to_string(),
            metrics: ctx.metrics(),
            diagnostics: ctx.diagnostics(),
        };
        ctx.register_checkpoint(ctx.name, session.checkpoint_hook());
        session
    }

    fn checkpoint_hook(&self) -> Arc<dyn Checkpoint> {
        Arc::new(RawStoreCheckpoint {
            pool: self.pool.clone(),
            entity_path: self.entity_path.clone(),
            before_events: self.before_events.clone(),
            before_blobs: self.before_blobs.clone(),
            source_name: self.source_name.clone(),
            metrics: self.metrics.clone(),
            diagnostics: self.diagnostics.clone(),
        })
    }

    /// Clean-completion finish: commit the source's `dolt_commit` (appending the
    /// `commit=<hash>` suffix to `summary`), snapshot-after + assemble the
    /// [`ExtractReport`], publish it through `ctx`, and `close()` the pool so
    /// translate can re-open the file. Best-effort commit — a failure logs and
    /// returns the bare summary.
    pub async fn finish(self, ctx: &RunCtx<'_>, summary: String) -> String {
        let final_summary = commit_with_suffix(&self.pool, &self.source_name, summary).await;
        let report = assemble_report(
            &self.entity_path,
            &self.before_events,
            &self.before_blobs,
            &self.metrics,
            &self.diagnostics,
        )
        .await;
        ctx.publish_report(report);
        self.pool.close().await;
        final_summary
    }
}

/// The interrupt-commit hook a [`RawStoreSession`] registers. On Ctrl-C it
/// commits the partial state AND assembles the partial report — both source-side
/// — so the orchestrator collects an opaque report and never reads the store.
struct RawStoreCheckpoint {
    pool: SqlitePool,
    entity_path: PathBuf,
    before_events: DbSnapshot,
    before_blobs: DbSnapshot,
    source_name: String,
    metrics: Arc<ExtractMetrics>,
    diagnostics: Arc<Diagnostics>,
}

#[async_trait]
impl Checkpoint for RawStoreCheckpoint {
    async fn checkpoint(&self) -> Result<Option<ExtractReport>> {
        let msg = format!("extract {}: interrupted (Ctrl-C)", self.source_name);
        crate::doltlite_raw::commit_run(&self.pool, &msg).await?;
        let report = assemble_report(
            &self.entity_path,
            &self.before_events,
            &self.before_blobs,
            &self.metrics,
            &self.diagnostics,
        )
        .await;
        Ok(Some(report))
    }
}

/// The source's post-extract commit: commit the write pool (`extract <name>:
/// <summary>`) and append the resulting `commit=<hash>` to the summary, exactly
/// as the old orchestrator did. Best-effort — a failure logs and returns the
/// bare summary (the data is already on disk). Does NOT close the pool.
async fn commit_with_suffix(pool: &SqlitePool, source_name: &str, summary: String) -> String {
    let msg = format!("extract {source_name}: {summary}");
    match crate::doltlite_raw::commit_run(pool, &msg).await {
        Ok(Some(h)) => format!("{summary} commit={h}"),
        Ok(None) => summary,
        Err(e) => {
            tracing::error!(
                source = %source_name,
                error = %format!("{e:#}"),
                "extract commit FAILED",
            );
            summary
        }
    }
}
