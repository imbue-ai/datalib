//! The `DataProcessor` trait and its run context — Program A's uniform
//! pipeline unit.
//!
//! A *data source* contributes one or more `DataProcessor`s grouped into the
//! two waves (`plan_download` / `plan_render` per provider): a download processor (ingests
//! from the outside world), a render processor (reads artifacts, emits
//! rendered docs), or just one of them. "Download-only" / "render-only" is
//! **structural** — a missing processor — not a flag or a no-op default.
//!
//! The defining rule of this layer is **storage-agnosticism**: the
//! orchestrator drives a processor purely through [`DataProcessor::run`] and
//! never learns how — or whether — the processor persists anything. A
//! processor that keeps a store owns it end to end (open, schema, write,
//! commit) and registers an opaque [`Checkpoint`] so an interrupt can flush
//! it *without the orchestrator knowing what "flush" means*. There is no
//! pool, no DDL, and no `dolt_commit` anywhere the orchestrator can see.
//!
//! The trait is single-method, so its shape is stable from Program A into
//! Program B — B adds a scheduler *around* it (deriving wave order from each
//! processor's declared inputs/outputs); the trait itself is never rewritten.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use crate::control::DownloadControl;
use crate::download_metrics::{DownloadMetrics, DownloadReport};
use crate::grid_index::RenderedMarkdown;
use crate::progress::Progress;
use crate::synthesize::Synthesizer;
use frankweiler_obs::diagnostics::Diagnostics;

/// One config-driven, monitorable unit of work the orchestrator runs.
/// Single method.
///
/// See the module docs for the storage-agnostic contract: a processor that
/// persists work owns its store and exposes only an opaque [`Checkpoint`]
/// (registered via [`RunCtx::register_checkpoint`]) for interrupt-safety.
#[async_trait]
pub trait DataProcessor: Send + Sync {
    /// Stable identifier for logs + progress, e.g. `"email/fastmail/download"`.
    fn id(&self) -> &str;

    /// Do the work. Returns a short human summary for the run log. (A
    /// *structured* outcome with a content-version is a Program-B concern;
    /// Program A keeps the string.)
    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String>;
}

/// The genuinely-runtime inputs a provider's `plan()` needs that are NOT part
/// of its (already-normalized) config: the source's orchestrator-owned identity
/// and, in synth/playback mode, the fixture root. Everything else a `plan()`
/// once received separately — the resolved paths, blob cap, event-tape flag,
/// download give-up bound — the provider now reads straight from `config.common`
/// (a resolved [`frankweiler_source_common::SourceCommon`]), since the
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
///
/// The orchestrator calls [`checkpoint`](Checkpoint::checkpoint) knowing ONLY
/// that it persists the processor's in-flight work — not that it is a
/// doltlite `dolt_commit`, not that a pool or even a file is involved. This
/// is the single seam through which interrupt-safety crosses the
/// storage-agnostic boundary.
#[async_trait]
pub trait Checkpoint: Send + Sync {
    /// Best-effort persist of whatever the owning processor has buffered so
    /// far, so an interrupt doesn't lose it. Returns the source's partial
    /// "what changed" [`DownloadReport`] when it has one — assembled
    /// source-side, so the orchestrator collects it without ever reading the
    /// store. `None` for sources that keep no reportable store.
    async fn checkpoint(&self) -> Result<Option<DownloadReport>>;
}

/// A one-slot mailbox a store-backed download processor publishes its
/// [`DownloadReport`] into; the orchestrator reads it back through the run
/// result. Interior-mutable so the source can publish through a shared
/// `&RunCtx`.
#[derive(Default)]
pub struct ReportCell {
    inner: Mutex<Option<DownloadReport>>,
}

impl ReportCell {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish the source's report (replaces any prior one — a source has at
    /// most one store-backed download processor).
    pub fn publish(&self, report: DownloadReport) {
        *self.inner.lock().unwrap() = Some(report);
    }

    /// Take the published report, if any.
    pub fn take(&self) -> Option<DownloadReport> {
        self.inner.lock().unwrap().take()
    }
}

/// An optional capability: a processor that can synthesize its own playback
/// fixtures (the `--synthesize-playback-root` mode). Kept OFF the universal
/// [`DataProcessor`] trait — only some download processors have it — so the
/// core trait stays about the thing every processor does.
pub trait HasSynthesizer {
    /// The provider's fixture synthesizer.
    fn synthesizer(&self) -> Box<dyn Synthesizer>;
}

/// A render processor emits each finished document through this callback;
/// Program A keeps Load fused into it (the orchestrator's sink upserts the
/// doc inline). `Send` so a render processor's `run` future stays `Send`
/// like every other processor's.
pub type DocCallback<'a> = dyn FnMut(RenderedMarkdown) -> Result<()> + Send + 'a;

/// Interior-mutable wrapper around the orchestrator's fused-Load callback so
/// a render processor can emit through a shared `&RunCtx`. The `Mutex`
/// keeps [`RunCtx`] `Sync` (hence every `run` future `Send`); per-source
/// render is sequential, so the lock is never actually contended.
struct DocSink<'a> {
    cb: Mutex<&'a mut DocCallback<'a>>,
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

    /// Register an interrupt-commit hook. Normally called via
    /// [`RunCtx::register_checkpoint`]; public so tests can populate a sink
    /// directly.
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
    /// Prior-run per-markdown fingerprints, for fingerprint-driven
    /// incremental skips on the render side.
    pub prior_fingerprints: &'a HashMap<String, String>,
    /// Where download processors register their interrupt-commit hooks.
    checkpoints: &'a CheckpointSink,
    /// Per-source "what changed" counters + WARN/ERROR buffer — the ambient
    /// observability the source folds into its own [`DownloadReport`]. `None` on
    /// a render context. (Observability, not storage: the orchestrator
    /// installs these as ambient scopes; the source reads them to self-report.)
    metrics: Option<Arc<DownloadMetrics>>,
    diagnostics: Option<Arc<Diagnostics>>,
    /// Where a download processor publishes its source-assembled report; the
    /// orchestrator reads it back through the run result. `None` on render.
    report: Option<&'a ReportCell>,
    /// Where render processors send finished documents (fused Load).
    /// `None` on a download context.
    emit: Option<DocSink<'a>>,
}

impl<'a> RunCtx<'a> {
    /// Build a context for a **download** processor (no doc sink). The
    /// processor registers its store's interrupt hook via
    /// [`register_checkpoint`](RunCtx::register_checkpoint).
    #[allow(clippy::too_many_arguments)]
    pub fn for_download(
        name: &'a str,
        root: &'a Path,
        now: &'a str,
        progress: &'a Progress,
        control: &'a DownloadControl,
        prior_fingerprints: &'a HashMap<String, String>,
        checkpoints: &'a CheckpointSink,
        metrics: Arc<DownloadMetrics>,
        diagnostics: Arc<Diagnostics>,
        report: &'a ReportCell,
    ) -> Self {
        Self {
            name,
            root,
            now,
            progress,
            control,
            prior_fingerprints,
            checkpoints,
            metrics: Some(metrics),
            diagnostics: Some(diagnostics),
            report: Some(report),
            emit: None,
        }
    }

    /// Build a context for a **render** processor, carrying the fused-Load
    /// callback each rendered document is emitted through.
    #[allow(clippy::too_many_arguments)]
    pub fn for_render(
        name: &'a str,
        root: &'a Path,
        now: &'a str,
        progress: &'a Progress,
        control: &'a DownloadControl,
        prior_fingerprints: &'a HashMap<String, String>,
        checkpoints: &'a CheckpointSink,
        on_doc: &'a mut DocCallback<'a>,
    ) -> Self {
        Self {
            name,
            root,
            now,
            progress,
            control,
            prior_fingerprints,
            checkpoints,
            metrics: None,
            diagnostics: None,
            report: None,
            emit: Some(DocSink {
                cb: Mutex::new(on_doc),
            }),
        }
    }

    /// Register an opaque interrupt-commit hook for this processor's store.
    /// Called by a download processor right after it opens its store, so a
    /// Ctrl-C mid-download can still flush partial state.
    pub fn register_checkpoint(&self, name: &str, hook: Arc<dyn Checkpoint>) {
        self.checkpoints.register(name, hook);
    }

    /// Open a doltlite [`RawStoreSession`](crate::raw_store::RawStoreSession)
    /// over a source's write `pool`: captures the before-snapshot and registers
    /// the session's interrupt-commit `Checkpoint` (which also reports on
    /// interrupt). The processor calls `session.finish(self, summary)` after
    /// the fetch. This is the uniform "doltlite-backed source" entry point —
    /// all the snapshot/commit/report machinery lives in `etl`, not here and
    /// not in the orchestrator.
    pub async fn open_store(
        &self,
        pool: sqlx::sqlite::SqlitePool,
        entity_path: std::path::PathBuf,
    ) -> crate::raw_store::RawStoreSession {
        crate::raw_store::RawStoreSession::open(pool, entity_path, self).await
    }

    /// The ambient download metrics for this source (download context only).
    pub fn metrics(&self) -> Arc<DownloadMetrics> {
        self.metrics
            .clone()
            .expect("metrics() on a non-download RunCtx")
    }

    /// The ambient diagnostics buffer for this source (download context only).
    pub fn diagnostics(&self) -> Arc<Diagnostics> {
        self.diagnostics
            .clone()
            .expect("diagnostics() on a non-download RunCtx")
    }

    /// Publish the source-assembled [`DownloadReport`] for this source. No-op on
    /// a context without a report cell.
    pub fn publish_report(&self, report: DownloadReport) {
        if let Some(cell) = self.report {
            cell.publish(report);
        }
    }

    /// Emit a finished rendered document (render processors only). Errors
    /// if called on a download context — that is a programming bug, since
    /// download processors have nothing to render.
    pub fn emit_doc(&self, md: RenderedMarkdown) -> Result<()> {
        let sink = self
            .emit
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("emit_doc called on a non-render RunCtx"))?;
        let mut cb = sink.cb.lock().unwrap();
        (cb)(md)
    }
}
