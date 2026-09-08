//! Shared helpers for doltlite-backed data sources — the "easy button" that
//! lets every such source follow one storage-ownership pattern under the
//! [`crate::processor`] model.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;

use crate::processor::{Checkpoint, RunCtx};

/// Called with the new version each time a session seals.
type SealCallback = Box<dyn Fn(&str) + Send + Sync>;

/// A doltlite raw-store session owned by a single download processor. Commits
/// at [`finish`](RawStoreSession::finish) and exposes an interrupt
/// [`Checkpoint`] that commits on Ctrl-C — both source-side.
pub struct RawStoreSession {
    pool: SqlitePool,
    source_name: String,
    /// Sibling blob CAS, when this source has one. Sealed *before* the
    /// entities pool, always — see [`RawStoreSession::maybe_checkpoint`].
    cas_pool: Option<SqlitePool>,
    checkpointer: std::sync::Mutex<crate::checkpointer::Checkpointer>,
    /// Told about each seal, so the step can announce it.
    on_seal: Option<SealCallback>,
}

impl RawStoreSession {
    pub async fn open(pool: SqlitePool, _entity_path: PathBuf, ctx: &RunCtx<'_>) -> Self {
        let session = Self {
            pool,
            source_name: ctx.name.to_string(),
            cas_pool: None,
            checkpointer: std::sync::Mutex::new(crate::checkpointer::Checkpointer::new(
                ctx.checkpoint_policy(),
            )),
            on_seal: None,
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

    /// The sibling blob CAS, so a checkpoint can seal it too.
    pub fn with_cas(mut self, cas_pool: SqlitePool) -> Self {
        self.cas_pool = Some(cas_pool);
        self
    }

    /// Called with the new version each time this seals, so the step can
    /// announce it on its event stream.
    pub fn on_seal(mut self, f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_seal = Some(Box::new(f));
        self
    }

    /// Tell the session work landed, and seal if it is time.
    ///
    /// **Ask this only where the store is consistent.** It never fires on its
    /// own, because only the caller knows that: a commit landing mid-prune or
    /// mid-reconcile publishes a store missing data it will have again a
    /// moment later, and every consumer downstream would act on it.
    pub async fn wrote(&self, rows: u64) -> Result<()> {
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
        if let (Some(hash), Some(f)) = (sealed, self.on_seal.as_ref()) {
            f(&hash);
        }
        Ok(())
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
