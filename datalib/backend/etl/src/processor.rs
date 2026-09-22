//! The download processor and its run context — Program A's uniform
//! pipeline unit for the fetch side. The render side's counterpart is
//! `datalib_etl_render::processor`.

use std::path::Path;
use std::sync::Arc;

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

    /// May a consumer read this processor's output *while it is being
    /// written* — P2 of the sink contract in
    /// `docs/dev/plans/streaming_steps_plan.md`.
    ///
    /// Default `false`, and it should stay that way until someone has
    /// looked. The question is not "is the store doltlite" — every raw
    /// store is — but **what the download does to it between
    /// checkpoints**. A download that empties a table before refilling it
    /// publishes, at every moment in between, a store that reads as a
    /// source which lost its data. Answering `true` there is how a
    /// consumer comes to delete rendered documents for rows that are
    /// about to come back.
    fn streams_output(&self) -> bool {
        false
    }
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

/// Orchestrator-owned context handed to every [`DataProcessor::run`]. Carries
/// only storage-agnostic concerns; anything about *how* a source persists
/// stays inside the source.
pub struct RunCtx<'a> {
    /// Source name (`sources[].name`).
    pub name: &'a str,
    /// Workspace root — the parent of the `render_markdown/` tree render
    /// processors write into.
    pub root: &'a Path,
    /// Run timestamp, threaded through for deterministic stamping.
    pub now: &'a str,
    /// Per-source progress hook.
    pub progress: &'a Progress,
    /// Cross-provider download knobs (the checkpoint cadence, the stop flag).
    pub control: &'a DownloadControl,
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
        metrics: Arc<DownloadMetrics>,
        diagnostics: Arc<Diagnostics>,
    ) -> Self {
        Self {
            name,
            root,
            now,
            progress,
            control,
            metrics,
            diagnostics,
        }
    }

    /// How often this run seals partial output.
    pub fn checkpoint_cadence(&self) -> crate::checkpointer::Cadence {
        self.control.checkpoint_cadence.unwrap_or_default()
    }

    /// Open a doltlite [`RawStoreSession`](crate::raw_store::RawStoreSession)
    /// over a source's write `pool`. The processor calls
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

    /// The same session with the source's blob CAS attached, so every
    /// seal commits the bytes before the rows that name them. A source
    /// that keeps a CAS opens its session this way; one whose CAS is
    /// optional passes `None` when it has none.
    pub async fn open_store_with_blobs(
        &self,
        pool: sqlx::sqlite::SqlitePool,
        cas_pool: Option<sqlx::sqlite::SqlitePool>,
        entity_path: std::path::PathBuf,
    ) -> crate::raw_store::RawStoreSession {
        crate::raw_store::RawStoreSession::open_with_blobs(pool, cas_pool, entity_path, self).await
    }

    pub fn metrics(&self) -> Arc<DownloadMetrics> {
        self.metrics.clone()
    }

    pub fn diagnostics(&self) -> Arc<Diagnostics> {
        self.diagnostics.clone()
    }
}
