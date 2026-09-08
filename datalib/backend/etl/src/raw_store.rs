//! Shared helpers for doltlite-backed data sources — the "easy button" that
//! lets every such source follow one storage-ownership pattern under the
//! [`crate::processor`] model.

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
    state: Arc<SealState>,
}

/// What sealing needs, shared between the session and every [`Sealer`] it
/// hands out. One [`Checkpointer`](crate::checkpointer::Checkpointer) behind
/// one lock: a source that writes from several tasks still gets one cadence,
/// not one per task.
struct SealState {
    pool: SqlitePool,
    source_name: String,
    /// Sibling blob CAS, when this source has one. Sealed *before* the
    /// entities pool, always — see [`SealState::seal`].
    cas_pool: Option<SqlitePool>,
    checkpointer: std::sync::Mutex<crate::checkpointer::Checkpointer>,
    /// Where a seal is announced. `Progress` is the channel that already
    /// crosses from `etl` out to whatever is driving the step, so a
    /// checkpoint rides it rather than growing a second one.
    progress: crate::progress::Progress,
}

/// A cheap, cloneable handle a fetch loop holds so it can seal at its own
/// batch boundary.
///
/// Separate from the session because the session is owned by the processor
/// and consumed by `finish`, while the loop that knows where the store is
/// consistent runs in between.
#[derive(Clone)]
pub struct Sealer {
    state: Arc<SealState>,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealer")
            .field("source", &self.state.source_name)
            .finish_non_exhaustive()
    }
}

impl Sealer {
    /// Tell the session work landed, and seal if it is time.
    ///
    /// **Call this only where the store is consistent.** It never fires on
    /// its own, because only the caller knows that: a commit landing
    /// mid-prune or mid-reconcile publishes a store missing data it will
    /// have again a moment later, and every consumer downstream would act
    /// on it.
    pub async fn wrote(&self, rows: u64) -> Result<()> {
        self.state.wrote(rows).await
    }
}

impl RawStoreSession {
    pub async fn open(pool: SqlitePool, entity_path: PathBuf, ctx: &RunCtx<'_>) -> Self {
        Self::open_with_blobs(pool, None, entity_path, ctx).await
    }

    /// As [`open`](Self::open), for a source whose blobs live in a sibling
    /// CAS file. Taken here rather than set later so the pair a seal has to
    /// commit is fixed before anyone can seal.
    pub async fn open_with_blobs(
        pool: SqlitePool,
        cas_pool: Option<SqlitePool>,
        _entity_path: PathBuf,
        ctx: &RunCtx<'_>,
    ) -> Self {
        let session = Self {
            state: Arc::new(SealState {
                pool,
                source_name: ctx.name.to_string(),
                cas_pool,
                checkpointer: std::sync::Mutex::new(crate::checkpointer::Checkpointer::new(
                    ctx.checkpoint_policy(),
                )),
                progress: ctx.progress.clone(),
            }),
        };
        ctx.register_checkpoint(ctx.name, session.checkpoint_hook());
        session
    }

    /// A handle the fetch loop can carry and clone.
    pub fn sealer(&self) -> Sealer {
        Sealer {
            state: self.state.clone(),
        }
    }

    fn checkpoint_hook(&self) -> Arc<dyn Checkpoint> {
        Arc::new(RawStoreCheckpoint {
            pool: self.state.pool.clone(),
            source_name: self.state.source_name.clone(),
        })
    }

    /// Tell the session work landed, and seal if it is time. See
    /// [`Sealer::wrote`].
    pub async fn wrote(&self, rows: u64) -> Result<()> {
        self.state.wrote(rows).await
    }

    /// Clean-completion finish: commit the source's `dolt_commit` (appending
    /// the `commit=<hash>` suffix to `summary`) and `close()` the pool so
    /// render can re-open the file. Best-effort commit — a failure logs and
    /// returns the bare summary.
    pub async fn finish(self, _ctx: &RunCtx<'_>, summary: String) -> String {
        let final_summary =
            commit_with_suffix(&self.state.pool, &self.state.source_name, summary).await;
        self.state.pool.close().await;
        final_summary
    }
}

impl SealState {
    async fn wrote(&self, rows: u64) -> Result<()> {
        {
            let mut c = self.checkpointer.lock().unwrap();
            c.wrote(rows);
            if !c.should_seal() {
                return Ok(());
            }
        }
        self.seal().await
    }

    async fn seal(&self) -> Result<()> {
        // **Blobs before entities, always.** An entity names a blob by its
        // blake3, so sealing entities first admits a reader pinned at that
        // commit seeing a row whose bytes are not yet committed — a dangling
        // attachment. The other order admits only an unreferenced blob, which
        // is already routine: the CAS is content-addressed and written with
        // `INSERT OR IGNORE`.
        if let Some(cas) = self.cas_pool.as_ref() {
            let msg = format!("checkpoint {}: blobs", self.source_name);
            crate::doltlite_raw::commit_run(cas, &msg).await?;
        }
        let msg = format!("checkpoint {}: entities", self.source_name);
        let sealed = crate::doltlite_raw::commit_run(&self.pool, &msg).await?;
        self.checkpointer.lock().unwrap().sealed();
        // `None` means there was nothing dirty after all; no version moved,
        // so there is nothing to announce.
        if let Some(hash) = sealed {
            self.progress.checkpoint(&hash);
        }
        Ok(())
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
