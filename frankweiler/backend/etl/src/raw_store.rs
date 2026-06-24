//! Shared helpers for doltlite-backed data sources — the "easy button" that
//! lets every such source follow one storage-ownership pattern under the
//! [`crate::processor`] model.
//!
//! Program A's rule is that the orchestrator is storage-agnostic: a source
//! that keeps a doltlite store owns it end to end (open, schema, write,
//! commit) and exposes only an opaque [`Checkpoint`] for interrupt-safety.
//! This module provides the reusable piece of that contract — [`PoolCheckpoint`],
//! which turns a source's write pool into the opaque interrupt-commit hook the
//! orchestrator fires on Ctrl-C without knowing it's a `dolt_commit`.
//!
//! The fuller `RawStoreSession` (before/after snapshot + report assembly) is
//! tracked in issue #37; the email pilot leaves report assembly
//! orchestrator-side for now.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;

use crate::processor::Checkpoint;

/// An opaque interrupt-commit hook backed by a doltlite write pool. A source
/// registers one of these (via [`crate::processor::RunCtx::register_checkpoint`])
/// right after it opens its store; on Ctrl-C the orchestrator calls
/// [`Checkpoint::checkpoint`] knowing only that it persists partial state —
/// the `dolt_commit` is entirely encapsulated here.
///
/// The pool is the *same* connection the source's extract is writing through
/// (doltlite pins `max_connections = 1`), so committing it from the SIGINT
/// task captures whatever the worker has written without a second-connection
/// lock race — the property the orchestrator's old pool-registration relied on,
/// now owned by the source.
pub struct PoolCheckpoint {
    pool: SqlitePool,
    message: String,
}

impl PoolCheckpoint {
    /// Wrap a write pool with the commit message to stamp on interrupt.
    pub fn new(pool: SqlitePool, message: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            pool,
            message: message.into(),
        })
    }
}

#[async_trait]
impl Checkpoint for PoolCheckpoint {
    async fn checkpoint(&self) -> Result<()> {
        crate::doltlite_raw::commit_run(&self.pool, &self.message).await?;
        Ok(())
    }
}
