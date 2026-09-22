//! Shared helpers for doltlite-backed data sources — the "easy button" that
//! lets every such source follow one storage-ownership pattern under the
//! [`crate::processor`] model.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;

use crate::processor::RunCtx;
use crate::store_handle::RawStoreHandle;

/// A doltlite raw-store session owned by a single download processor. Seals
/// on the [`Checkpointer`](crate::checkpointer::Checkpointer)'s cadence and
/// commits at [`finish`](RawStoreSession::finish) — both source-side. A stop
/// (Ctrl-C) makes the next consistent point a seal and `finish` commits
/// there; nothing commits from the signal handler, and what a killed run
/// wrote after its last commit the next `open` discards.
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
    stop: crate::stop::StopFlag,
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

    /// Whether the step has been asked to stop; the same flag as
    /// `DownloadControl::stop`, for a loop that holds only the sealer.
    pub fn stopping(&self) -> bool {
        self.state.stop.requested()
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
        Self {
            state: Arc::new(SealState {
                pool,
                source_id: ctx.name.to_string(),
                cas_pool,
                checkpointer: std::sync::Mutex::new(crate::checkpointer::Checkpointer::new(
                    ctx.checkpoint_cadence(),
                )),
                progress: ctx.progress.clone(),
                stop: ctx.control.stop.clone(),
            }),
        }
    }

    /// A handle the fetch loop can carry and clone.
    pub fn sealer(&self) -> Sealer {
        Sealer {
            state: self.state.clone(),
        }
    }

    /// Clean-completion finish: the run's last seal, then `close()` every
    /// store so render can re-open them. Blobs before entities, for the
    /// reason on [`SealState::seal`]. The summary comes back with
    /// `commit=<hash>` appended when the entity store moved.
    ///
    /// A commit that fails fails the step: the rows are on disk, but the
    /// next writer's `open` discards whatever was never committed, so a
    /// pass that logged its commit failure and returned `Ok` would have
    /// done its work for nothing and said it succeeded. The step is
    /// idempotent; the retry refetches.
    pub async fn finish(self, _ctx: &RunCtx<'_>, summary: String) -> Result<String> {
        let committed = self.state.commit_final(summary).await;
        self.state.close_all().await;
        committed
    }
}

impl SealState {
    async fn commit_final(&self, summary: String) -> Result<String> {
        if let Some(cas) = self.cas_pool.as_ref() {
            let msg = format!("download {}: blobs", self.source_id);
            crate::doltlite_raw::commit_run(cas, &msg)
                .await
                .with_context(|| format!("commit {}'s blob store", self.source_id))?;
        }
        let msg = format!("download {}: {summary}", self.source_id);
        let hash = crate::doltlite_raw::commit_run(&self.pool, &msg)
            .await
            .with_context(|| format!("commit {}'s entity store", self.source_id))?;
        Ok(match hash {
            Some(h) => format!("{summary} commit={h}"),
            None => summary,
        })
    }

    fn wrote(&self, rows: u64) -> impl std::future::Future<Output = ()> + '_ {
        // A stop makes every consistent point a seal: the caller is telling
        // us its store is consistent right now, and there may not be a next
        // time. The cadence is a latency dial, not a correctness one.
        let due = {
            let mut c = self.checkpointer.lock().unwrap();
            c.wrote(rows);
            c.should_seal()
        } || self.stop.requested();
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
        let rows = {
            let mut c = self.checkpointer.lock().unwrap();
            let pending = c.pending();
            c.sealed();
            pending
        };
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
            self.progress.checkpoint_rows(&hash, rows);
        }
        Ok(())
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
        state_with_cadence(
            pool,
            cas,
            p,
            crate::checkpointer::Cadence {
                at_most_every: std::time::Duration::ZERO,
            },
        )
    }

    fn state_with_cadence(
        pool: SqlitePool,
        cas: Option<SqlitePool>,
        p: crate::progress::Progress,
        cadence: crate::checkpointer::Cadence,
    ) -> SealState {
        SealState {
            pool,
            source_id: "t".into(),
            cas_pool: cas,
            checkpointer: std::sync::Mutex::new(crate::checkpointer::Checkpointer::new(cadence)),
            progress: p,
            stop: crate::stop::StopFlag::default(),
        }
    }

    /// Ctrl-C wants one last seal, and the only place a seal is safe is
    /// where the provider says its store is consistent — its `wrote`. So a
    /// stop makes that call seal whatever the cadence says; it is the
    /// last chance, and the caller just vouched for the state.
    #[tokio::test]
    async fn a_stop_seals_at_the_next_consistent_point_whatever_the_cadence() {
        let dir = tempfile::tempdir().unwrap();
        let entities = store(&dir.path().join("entities.doltlite_db")).await;
        if !crate::doltlite_raw::has_dolt_extensions(&entities).await {
            return;
        }
        let state = state_with_cadence(
            entities.clone(),
            None,
            crate::progress::Progress::noop(),
            crate::checkpointer::Cadence {
                at_most_every: std::time::Duration::from_secs(3600),
            },
        );
        let before = commits(&entities).await;
        sqlx::query("INSERT INTO rows_t VALUES ('a')")
            .execute(&entities)
            .await
            .unwrap();
        state.wrote(1).await;
        assert_eq!(
            commits(&entities).await,
            before,
            "an hour's cadence has not come round"
        );

        state.stop.request();
        state.wrote(1).await;
        assert_eq!(
            commits(&entities).await,
            before + 1,
            "the consistent point after a stop is the last seal"
        );
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
