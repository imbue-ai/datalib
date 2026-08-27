//! Shared helpers for doltlite-backed data sources — the "easy button" that
//! lets every such source follow one storage-ownership pattern under the
//! [`crate::processor`] model.
//!
//! Program A's rule is that the orchestrator is storage-agnostic: a source that
//! keeps a doltlite store owns it end to end (open, schema, write, commit) and
//! exposes one opaque seam — an interrupt [`Checkpoint`] — so the orchestrator
//! never reads the store.
//!
//! [`RawStoreSession`] is that easy button: open it over a source's write pool
//! (registers the interrupt hook), then `finish(ctx, summary)` after the fetch
//! (commit + close). The interrupt hook ([`Checkpoint`]) does the same commit
//! on Ctrl-C, so both paths are source-side.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;

use crate::processor::{Checkpoint, RunCtx};

/// A doltlite raw-store session owned by a single download processor. Commits
/// at [`finish`](RawStoreSession::finish) and exposes an interrupt
/// [`Checkpoint`] that commits on Ctrl-C — both source-side.
pub struct RawStoreSession {
    pool: SqlitePool,
    source_name: String,
}

impl RawStoreSession {
    /// Open over a source's write `pool` (entity doltlite at `entity_path`)
    /// and register the interrupt-commit `Checkpoint`. Prefer
    /// [`RunCtx::open_store`](crate::processor::RunCtx::open_store).
    pub async fn open(pool: SqlitePool, _entity_path: PathBuf, ctx: &RunCtx<'_>) -> Self {
        let session = Self {
            pool,
            source_name: ctx.name.to_string(),
        };
        ctx.register_checkpoint(ctx.name, session.checkpoint_hook());
        session
    }

    fn checkpoint_hook(&self) -> Arc<dyn Checkpoint> {
        Arc::new(RawStoreCheckpoint {
            pool: self.pool.clone(),
            source_name: self.source_name.clone(),
        })
    }

    /// Clean-completion finish: commit the source's `dolt_commit` (appending
    /// the `commit=<hash>` suffix to `summary`) and `close()` the pool so
    /// render can re-open the file. Best-effort commit — a failure logs and
    /// returns the bare summary.
    pub async fn finish(self, _ctx: &RunCtx<'_>, summary: String) -> String {
        let final_summary = commit_with_suffix(&self.pool, &self.source_name, summary).await;
        self.pool.close().await;
        final_summary
    }
}

/// The interrupt-commit hook a [`RawStoreSession`] registers. On Ctrl-C it
/// commits the partial state, source-side, so the orchestrator never reads the
/// store.
struct RawStoreCheckpoint {
    pool: SqlitePool,
    source_name: String,
}

#[async_trait]
impl Checkpoint for RawStoreCheckpoint {
    async fn checkpoint(&self) -> Result<()> {
        let msg = format!("download {}: interrupted (Ctrl-C)", self.source_name);
        crate::doltlite_raw::commit_run(&self.pool, &msg).await?;
        Ok(())
    }
}

/// The source's post-download commit: commit the write pool (`download <name>:
/// <summary>`) and append the resulting `commit=<hash>` to the summary, exactly
/// as the old orchestrator did. Best-effort — a failure logs and returns the
/// bare summary (the data is already on disk). Does NOT close the pool.
async fn commit_with_suffix(pool: &SqlitePool, source_name: &str, summary: String) -> String {
    let msg = format!("download {source_name}: {summary}");
    match crate::doltlite_raw::commit_run(pool, &msg).await {
        Ok(Some(h)) => format!("{summary} commit={h}"),
        Ok(None) => summary,
        Err(e) => {
            tracing::error!(
                source = %source_name,
                error = %format!("{e:#}"),
                "download commit FAILED",
            );
            summary
        }
    }
}
