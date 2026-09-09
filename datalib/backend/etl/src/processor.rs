//! The download processor and its run context — Program A's uniform
//! pipeline unit for the fetch side. The render side's counterpart is
//! `datalib_etl_render::processor`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use crate::control::DownloadControl;
use crate::download_metrics::DownloadMetrics;
use crate::progress::Progress;
use datalib_obs::diagnostics::Diagnostics;

/// One config-driven, monitorable unit of work the orchestrator runs.
/// Single method.
#[async_trait]
pub trait DataProcessor: Send + Sync {
    fn id(&self) -> &str;

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String>;
}

/// The genuinely-runtime inputs a provider's `plan()` needs that are NOT part
/// of its (already-normalized) config: the source's orchestrator-owned identity
/// and, in synth/playback mode, the fixture root. Everything else a `plan()`
/// once received separately — the resolved paths, blob cap, event-tape flag,
/// download give-up bound — the provider now reads straight from `config.common`
/// (a resolved [`datalib_source_common::SourceCommon`]), since the
/// orchestrator's `normalize()` resolved it at load. Built once per source.
#[derive(Debug, Clone)]
pub struct PlanContext {
    /// `sources[].name` — the source's identity in the orchestrator's list
    /// (used for processor IDs and labels). Orchestrator-owned; deliberately
    /// NOT part of any provider's config schema.
    pub name: String,
    /// Playback-fixture root, when the orchestrator is in synth/playback mode.
    /// Only notion consumes it (to derive BFS seeds); `None` on the live path.
    pub playback_root: Option<std::path::PathBuf>,
}

/// An opaque "persist what you have" hook. A processor that buffers work into
/// a store registers one of these at the moment it opens the store; the
/// orchestrator holds the registered hooks and fires them on SIGINT.
#[async_trait]
pub trait Checkpoint: Send + Sync {
    async fn checkpoint(&self) -> Result<()>;
}

/// One registered interrupt-commit hook, paired with its source name for
/// logging on the SIGINT path.
#[derive(Clone)]
pub struct RegisteredCheckpoint {
    pub name: String,
    pub hook: Arc<dyn Checkpoint>,
}

/// Thread-safe collector of interrupt-commit hooks, owned by the orchestrator
/// and shared into every download [`RunCtx`]. Download processors push their
/// hooks as they open their stores; the orchestrator's Ctrl-C path snapshots
/// and fires them.
#[derive(Default)]
pub struct CheckpointSink {
    inner: Mutex<Vec<RegisteredCheckpoint>>,
}

impl CheckpointSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, name: &str, hook: Arc<dyn Checkpoint>) {
        self.inner.lock().unwrap().push(RegisteredCheckpoint {
            name: name.to_string(),
            hook,
        });
    }

    /// A clone of every hook registered so far. Used by the orchestrator's
    /// interrupt path; cloning (rather than draining) lets registration keep
    /// running concurrently with an in-flight SIGINT flush.
    pub fn snapshot(&self) -> Vec<RegisteredCheckpoint> {
        self.inner.lock().unwrap().clone()
    }
}

/// Orchestrator-owned context handed to every [`DataProcessor::run`]. Carries
/// only storage-agnostic concerns; anything about *how* a source persists
/// stays inside the source.
pub struct RunCtx<'a> {
    /// Source name (`sources[].name`).
    pub name: &'a str,
    /// Workspace root — the parent of the `rendered_md/` tree render
    /// processors write into.
    pub root: &'a Path,
    /// Run timestamp, threaded through for deterministic stamping.
    pub now: &'a str,
    /// Per-source progress hook.
    pub progress: &'a Progress,
    /// Cross-provider download knobs (`--reset-and-redownload`, …).
    pub control: &'a DownloadControl,
    /// Where download processors register their interrupt-commit hooks.
    checkpoints: &'a CheckpointSink,
    /// Per-source "what changed" counters + WARN/ERROR buffer — the ambient
    /// observability the orchestrator installs as scopes.
    metrics: Arc<DownloadMetrics>,
    diagnostics: Arc<Diagnostics>,
}

impl<'a> RunCtx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &'a str,
        root: &'a Path,
        now: &'a str,
        progress: &'a Progress,
        control: &'a DownloadControl,
        checkpoints: &'a CheckpointSink,
        metrics: Arc<DownloadMetrics>,
        diagnostics: Arc<Diagnostics>,
    ) -> Self {
        Self {
            name,
            root,
            now,
            progress,
            control,
            checkpoints,
            metrics,
            diagnostics,
        }
    }

    /// Whether this run seals partial output, and how often.
    ///
    /// **A wipe-and-re-ingest run never does.** `reset_and_redownload`
    /// truncates every table and re-fetches, so at any point before it
    /// finishes the store holds a fraction of the source. Publishing that is
    /// not "partial progress" — half a re-ingest is indistinguishable from a
    /// source that lost most of its data, and every consumer downstream would
    /// act on it. The whole run is the atomic unit, so it commits once, at
    /// the end.
    ///
    /// The same reasoning covers any ingest that prunes to a snapshot; those
    /// pass `Never` themselves.
    pub fn checkpoint_policy(&self) -> crate::checkpointer::Policy {
        if self.control.reset_and_redownload {
            return crate::checkpointer::Policy::Never;
        }
        crate::checkpointer::Policy::Every(self.control.checkpoint_cadence.unwrap_or_default())
    }

    pub fn register_checkpoint(&self, name: &str, hook: Arc<dyn Checkpoint>) {
        self.checkpoints.register(name, hook);
    }

    /// Open a doltlite [`RawStoreSession`](crate::raw_store::RawStoreSession)
    /// over a source's write `pool` and register the session's
    /// interrupt-commit `Checkpoint`. The processor calls
    /// `session.finish(self, summary)` after the fetch. This is the uniform
    /// "doltlite-backed source" entry point — the commit machinery lives in
    /// `etl`, not here and not in the orchestrator.
    pub async fn open_store(
        &self,
        pool: sqlx::sqlite::SqlitePool,
        entity_path: std::path::PathBuf,
    ) -> crate::raw_store::RawStoreSession {
        crate::raw_store::RawStoreSession::open(pool, entity_path, self).await
    }

    pub fn metrics(&self) -> Arc<DownloadMetrics> {
        self.metrics.clone()
    }

    pub fn diagnostics(&self) -> Arc<Diagnostics> {
        self.diagnostics.clone()
    }
}
