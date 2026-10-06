//! Shared helpers for doltlite-backed data sources — the "easy button" that
//! lets every such source follow one storage-ownership pattern under the
//! [`crate::processor`] model.

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
    /// Sibling blob CAS, when this source has one. Held only so `finish`
    /// closes it: it is plain SQLite, and each `put_many` commits itself
    /// before the edge rows naming its blobs are written.
    cas_pool: Option<SqlitePool>,
    checkpointer: std::sync::Mutex<crate::checkpointer::Checkpointer>,
    /// Where a seal is announced. `Progress` is the channel that already
    /// crosses from `etl` out to whatever is driving the step, so a
    /// checkpoint rides it rather than growing a second one.
    progress: crate::progress::Progress,
    stop: crate::stop::StopFlag,
    /// The running download's problems, written ahead of each seal so a
    /// checkpoint carries them.
    problems: std::sync::Mutex<Option<crate::run_problems::RunProblems>>,
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

    pub(crate) fn carry(&self, problems: Option<crate::run_problems::RunProblems>) {
        *self.state.problems.lock().unwrap() = problems;
    }
}

impl RawStoreSession {
    /// `cas_pool` is the source's sibling blob CAS, when it keeps one,
    /// which the session closes with the entities.
    pub(crate) async fn open(
        pool: SqlitePool,
        cas_pool: Option<SqlitePool>,
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
                problems: std::sync::Mutex::new(None),
            }),
        }
    }

    fn sealer(&self) -> Sealer {
        Sealer {
            state: self.state.clone(),
        }
    }

    /// Run a download's `body` against this session's store and end the
    /// session whichever way the body returns, so no path leaves a pool
    /// open. `Ok(summary)` is the run's last seal. `Err` commits nothing:
    /// the error may have struck between two writes that only make sense
    /// together (a row stamped current before its attachments, a file
    /// stamped read before its lookups), and a store sealed there would
    /// read as complete and never be finished. What the run wrote since
    /// its last seal, a point the download itself called consistent, the
    /// next writer's `open` discards. A download that can still keep what
    /// it fetched records a problem and returns `Ok`
    /// (docs/dev/data_architecture_ingestion.md §"Error handling").
    pub async fn run<Fut>(self, body: impl FnOnce(Sealer) -> Fut) -> Result<String>
    where
        Fut: std::future::Future<Output = Result<String>>,
    {
        let result = body(self.sealer()).await;
        // Counted before the stores close, and on the error path too: a
        // failed run's sealed checkpoints hold rows the Manage screen
        // should count.
        self.state.publish_problem_counts().await;
        match result {
            Ok(summary) => self.finish(summary).await,
            Err(e) => {
                self.state.close_all().await;
                Err(e)
            }
        }
    }

    /// The run's last seal, then `close()` every
    /// store so render can re-open them. The summary comes back with
    /// `commit=<hash>` appended when the entity store moved.
    ///
    /// A commit that fails fails the step: the rows are on disk, but the
    /// next writer's `open` discards whatever was never committed, so a
    /// pass that logged its commit failure and returned `Ok` would have
    /// done its work for nothing and said it succeeded. The step is
    /// idempotent; the retry refetches.
    async fn finish(self, summary: String) -> Result<String> {
        let committed = self.state.commit_final(summary).await;
        self.state.close_all().await;
        committed
    }
}

impl SealState {
    async fn commit_final(&self, summary: String) -> Result<String> {
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
        let carried = self.problems.lock().unwrap().clone();
        if let Some(problems) = carried {
            problems
                .write_or_say(&self.pool, crate::run_problems::Ran::PartWay)
                .await;
        }
        let msg = format!("checkpoint {}: entities", self.source_id);
        let sealed = crate::doltlite_raw::commit_run(&self.pool, &msg).await?;
        // `None` means there was nothing dirty after all; no version moved,
        // so there is nothing to announce.
        if let Some(hash) = sealed {
            self.publish_problem_counts().await;
            self.progress.checkpoint_rows(&hash, rows);
            crate::http::record_seal();
        }
        Ok(())
    }

    /// The store's errors and warnings as the step's `problems` metric,
    /// zero included, so the Manage row counts what a checkpoint holds
    /// while the download is still running. Never a reason to fail.
    async fn publish_problem_counts(&self) {
        use datalib_problems::{Severity, METRIC};
        let counted: Result<Vec<(String, i64)>, _> =
            sqlx::query_as("SELECT severity, COUNT(*) FROM problems GROUP BY severity")
                .fetch_all(&self.pool)
                .await;
        match counted {
            Ok(rows) => {
                for severity in [Severity::Error, Severity::Warning] {
                    let n = rows
                        .iter()
                        .find(|(word, _)| word == severity.as_str())
                        .map_or(0, |(_, n)| *n);
                    self.progress.metric(METRIC, &[severity.metric_label()], n);
                }
            }
            Err(e) => tracing::warn!(
                source = %self.source_id,
                error = %e,
                "could not count the store's problems; the Manage row keeps its last count"
            ),
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
            problems: std::sync::Mutex::new(None),
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

    /// A consumer reading a checkpoint has to see what the download could
    /// not do for the rows that checkpoint publishes, not learn of it when
    /// the run ends.
    #[tokio::test]
    async fn a_seal_carries_the_problems_found_so_far() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entities.doltlite_db");
        let entities = store(&path).await;
        if !crate::doltlite_raw::has_dolt_extensions(&entities).await {
            return;
        }
        let counts = Arc::new(std::sync::Mutex::new(Vec::<(String, i64)>::new()));
        struct Counts(Arc<std::sync::Mutex<Vec<(String, i64)>>>);
        impl crate::progress::ProgressSink for Counts {
            fn metric(&self, name: &str, labels: &[(&str, &str)], value: i64) {
                self.0
                    .lock()
                    .unwrap()
                    .push((format!("{name}{{{}={}}}", labels[0].0, labels[0].1), value));
            }
        }
        let sealer = Sealer {
            state: Arc::new(state(
                entities.clone(),
                None,
                crate::progress::Progress::new(Arc::new(Counts(counts.clone()))),
            )),
        };
        let at_the_seal = crate::run_problems::collecting_sealed(
            &entities,
            &crate::stop::StopFlag::new(),
            Some(&sealer),
            |problems| async {
                let problems = problems;
                problems.listing("workouts", "HTTP 500");
                sealer.wrote(1).await;
                let head = crate::pin::head(&entities).await?.unwrap();
                let reader = crate::doltlite_raw::open_reader(&path, Some(head.commit()))
                    .await?
                    .unwrap();
                let keys: Vec<String> = sqlx::query_scalar("SELECT scope_key FROM problems")
                    .fetch_all(reader.pool())
                    .await?;
                reader.pool().close().await;
                Ok(keys)
            },
        )
        .await
        .unwrap();
        assert_eq!(at_the_seal, ["listing:workouts"]);
        assert_eq!(
            *counts.lock().unwrap(),
            [
                ("problems{severity=error}".to_string(), 1),
                ("problems{severity=warning}".to_string(), 0)
            ],
            "the count the Manage row draws moves with the seal"
        );
    }

    /// Five test helpers once committed after a failed fetch, and so
    /// showed work as kept that the processor throws away. An error
    /// commits nothing since the last seal, and still closes the store.
    #[tokio::test]
    async fn a_failed_run_commits_nothing_and_closes_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entities.doltlite_db");
        let entities = store(&path).await;
        let progress = crate::progress::Progress::noop();
        let control = crate::control::DownloadControl::default();
        let ctx = RunCtx::new(
            "t",
            dir.path(),
            "2369-04-14T00:00:00+00:00",
            &progress,
            &control,
            crate::download_metrics::DownloadMetrics::new(),
            datalib_obs::diagnostics::Diagnostics::new(),
        );
        let failed = ctx
            .run_store(entities.clone(), None, |_| async {
                sqlx::query("INSERT INTO rows_t VALUES ('picard')")
                    .execute(&entities)
                    .await?;
                anyhow::bail!("HTTP 401")
            })
            .await;
        assert!(failed.is_err(), "the step fails either way");
        // What the next writer finds once its open has discarded
        // whatever was never committed.
        let reopened = store(&path).await;
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM rows_t")
            .fetch_one(&reopened)
            .await
            .unwrap();
        reopened.close().await;
        assert_eq!(kept, 0, "nothing the failed run wrote is kept");
        assert!(entities.is_closed());
    }

    /// `finish` has to release every store the session was handed, not just
    /// the entities one. The CAS stayed open because `finish` named the
    /// entity pool directly.
    #[tokio::test]
    async fn finish_closes_every_store_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let entities = store(&dir.path().join("entities.doltlite_db")).await;
        let cas = crate::blob_cas::BlobCas::open(&dir.path().join("blobs.sqlite"))
            .await
            .unwrap()
            .pool()
            .clone();

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
