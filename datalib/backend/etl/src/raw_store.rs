//! Shared helpers for doltlite-backed data sources — the "easy button" that
//! lets every such source follow one storage-ownership pattern under the
//! [`crate::processor`] model.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;

use crate::processor::{Checkpoint, RunCtx};
use crate::store_handle::RawStoreHandle;

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
#[derive(datalib_etl_macros::RawStoreHandle)]
struct SealState {
    pool: SqlitePool,
    source_id: String,
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
            .field("source", &self.state.source_id)
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
    pub async fn wrote(&self, rows: u64) {
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
                source_id: ctx.name.to_string(),
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
            source_id: self.state.source_id.clone(),
        })
    }

    /// Clean-completion finish: commit the source's `dolt_commit` (appending
    /// the `commit=<hash>` suffix to `summary`) and `close()` every store so
    /// render can re-open them. Best-effort commit — a failure logs and
    /// returns the bare summary.
    ///
    /// `close_all` is derived from the struct's fields, so the blob CAS
    /// goes with the entity pool without this having to name either.
    pub async fn finish(self, _ctx: &RunCtx<'_>, summary: String) -> String {
        let final_summary =
            commit_with_suffix(&self.state.pool, &self.state.source_id, summary).await;
        self.state.close_all().await;
        final_summary
    }
}

impl SealState {
    fn wrote(&self, rows: u64) -> impl std::future::Future<Output = ()> + '_ {
        let due = {
            let mut c = self.checkpointer.lock().unwrap();
            c.wrote(rows);
            c.should_seal()
        };
        async move {
            if !due {
                return;
            }
            // **The run does not fail because a checkpoint did not.** The
            // rows are already on disk and `finish` will commit them; a
            // checkpoint only decides how early a consumer may see them.
            // Loud, though -- a checkpoint that never lands means streaming
            // silently stops happening, which is the shape of failure
            // AGENTS.md's "prefer failing loudly" section is about.
            if let Err(e) = self.seal().await {
                tracing::warn!(
                    source = %self.source_id,
                    error = %format!("{e:#}"),
                    "checkpoint commit failed; the rows stay pending until the run's final commit",
                );
            }
        }
    }

    async fn seal(&self) -> Result<()> {
        // Counted as done whatever happens below, so a store that cannot
        // commit is retried on the next cadence rather than on every row.
        self.checkpointer.lock().unwrap().sealed();
        // **Blobs before entities, always.** An entity names a blob by its
        // blake3, so sealing entities first admits a reader pinned at that
        // commit seeing a row whose bytes are not yet committed — a dangling
        // attachment. The other order admits only an unreferenced blob, which
        // is already routine: the CAS is content-addressed and written with
        // `INSERT OR IGNORE`.
        if let Some(cas) = self.cas_pool.as_ref() {
            let msg = format!("checkpoint {}: blobs", self.source_id);
            crate::doltlite_raw::commit_run(cas, &msg).await?;
        }
        let msg = format!("checkpoint {}: entities", self.source_id);
        let sealed = crate::doltlite_raw::commit_run(&self.pool, &msg).await?;
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
    source_id: String,
}

#[async_trait]
impl Checkpoint for RawStoreCheckpoint {
    async fn checkpoint(&self) -> Result<()> {
        let msg = format!("download {}: interrupted (Ctrl-C)", self.source_id);
        crate::doltlite_raw::commit_run(&self.pool, &msg).await?;
        Ok(())
    }
}

/// The source's post-download commit: commit the write pool (`download <name>:
/// <summary>`) and append the resulting `commit=<hash>` to the summary, exactly
/// as the old orchestrator did. Best-effort — a failure logs and returns the
/// bare summary (the data is already on disk). Does NOT close the pool.
async fn commit_with_suffix(pool: &SqlitePool, source_id: &str, summary: String) -> String {
    let msg = format!("download {source_id}: {summary}");
    match crate::doltlite_raw::commit_run(pool, &msg).await {
        Ok(Some(h)) => format!("{summary} commit={h}"),
        Ok(None) => summary,
        Err(e) => {
            tracing::error!(
                source = %source_id,
                error = %format!("{e:#}"),
                "download commit FAILED",
            );
            summary
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store(path: &std::path::Path) -> SqlitePool {
        crate::doltlite_raw::open(
            path,
            &["CREATE TABLE IF NOT EXISTS rows_t (id TEXT PRIMARY KEY)"],
        )
        .await
        .unwrap()
    }

    async fn commits(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM dolt_log()")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn state(pool: SqlitePool, cas: Option<SqlitePool>, p: crate::progress::Progress) -> SealState {
        SealState {
            pool,
            source_id: "t".into(),
            cas_pool: cas,
            checkpointer: std::sync::Mutex::new(crate::checkpointer::Checkpointer::new(
                crate::checkpointer::Policy::Every(crate::checkpointer::Cadence {
                    quiet_for: std::time::Duration::ZERO,
                    at_most_every: std::time::Duration::ZERO,
                }),
            )),
            progress: p,
        }
    }

    /// A source whose blobs live in a sibling file must have *both* sealed.
    /// Sealing only the entities store publishes a row naming bytes no
    /// reader can resolve — which is exactly what shipped: `open_with_blobs`
    /// existed, and claude, which has a CAS, was calling `open`.
    #[tokio::test]
    async fn a_seal_commits_the_blob_store_too() {
        let dir = tempfile::tempdir().unwrap();
        let entities = store(&dir.path().join("entities.doltlite_db")).await;
        let cas = store(&dir.path().join("blobs.doltlite_db")).await;
        if !crate::doltlite_raw::has_dolt_extensions(&entities).await {
            return;
        }
        let (before_e, before_c) = (commits(&entities).await, commits(&cas).await);

        sqlx::query("INSERT INTO rows_t VALUES ('e')")
            .execute(&entities)
            .await
            .unwrap();
        sqlx::query("INSERT INTO rows_t VALUES ('b')")
            .execute(&cas)
            .await
            .unwrap();

        state(
            entities.clone(),
            Some(cas.clone()),
            crate::progress::Progress::noop(),
        )
        .seal()
        .await
        .unwrap();

        assert_eq!(commits(&entities).await, before_e + 1, "entities must seal");
        assert_eq!(
            commits(&cas).await,
            before_c + 1,
            "the blob store must seal too, or a checkpoint publishes dangling attachments"
        );
    }

    /// The version announced has to be the commit the seal just made —
    /// that string is what a consumer pins to.
    #[tokio::test]
    async fn a_seal_announces_the_commit_it_made() {
        let dir = tempfile::tempdir().unwrap();
        let entities = store(&dir.path().join("entities.doltlite_db")).await;
        if !crate::doltlite_raw::has_dolt_extensions(&entities).await {
            return;
        }
        sqlx::query("INSERT INTO rows_t VALUES ('e')")
            .execute(&entities)
            .await
            .unwrap();

        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        struct S(Arc<std::sync::Mutex<Vec<String>>>);
        impl crate::progress::ProgressSink for S {
            fn checkpoint(&self, v: &str) {
                self.0.lock().unwrap().push(v.to_string());
            }
        }
        let progress = crate::progress::Progress::new(Arc::new(S(seen.clone())));

        state(entities.clone(), None, progress)
            .seal()
            .await
            .unwrap();

        let announced = seen.lock().unwrap().clone();
        let head = crate::pin::head(&entities).await.unwrap().unwrap();
        assert_eq!(
            announced,
            vec![head.commit().to_string()],
            "the announced version must be the new HEAD"
        );
    }

    /// `finish` has to release every store the session was handed, not just
    /// the entities one. The CAS stayed open because `finish` named the
    /// entity pool directly, and it went unnoticed while nothing committed
    /// through the CAS; #329 made it a committing writer.
    #[tokio::test]
    async fn finish_closes_every_store_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let entities = store(&dir.path().join("entities.doltlite_db")).await;
        let cas = store(&dir.path().join("blobs.doltlite_db")).await;

        state(
            entities.clone(),
            Some(cas.clone()),
            crate::progress::Progress::noop(),
        )
        .close_all()
        .await;

        assert!(entities.is_closed(), "the entity pool must be closed");
        assert!(
            cas.is_closed(),
            "the blob store must be closed too — a pool nothing closes is a \
             connection the next opener contends with"
        );
    }

    /// A store that cannot commit must not be retried on every single row.
    #[tokio::test]
    async fn a_failed_seal_waits_for_the_next_cadence_rather_than_every_row() {
        let dir = tempfile::tempdir().unwrap();
        let entities = store(&dir.path().join("entities.doltlite_db")).await;
        if !crate::doltlite_raw::has_dolt_extensions(&entities).await {
            return;
        }
        let st = state(entities.clone(), None, crate::progress::Progress::noop());
        // A seal that finds nothing dirty still counts as done.
        st.seal().await.unwrap();
        assert!(
            !st.checkpointer.lock().unwrap().should_seal(),
            "seal() must mark the checkpointer sealed even when it commits nothing"
        );
    }
}
