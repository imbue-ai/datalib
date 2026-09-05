//! The `DataProcessor` trait and its run context — Program A's uniform
//! pipeline unit.
//!
//! A *data source* contributes one or more `DataProcessor`s grouped into the
//! two waves (`plan_download` / `plan_render` per provider): a download processor (ingests
//! from the outside world), a render processor (reads artifacts, emits
//! rendered docs), or just one of them. Contributing only one is
//! **structural** — a missing processor — not a flag or a no-op default.
//! `media`, `fsindex` and `lightroom` are download-only: they mirror a
//! tree and there is nothing chat-shaped to render.
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
use crate::download_metrics::DownloadMetrics;
use crate::grid_index::RenderedMarkdown;
use crate::progress::Progress;
use datalib_obs::diagnostics::Diagnostics;

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

    /// The renderer version this processor stamps onto every document
    /// header it writes, when it writes sidecars at all.
    ///
    /// The render step uses it to tell a tree rendered by *this* build
    /// from one left behind by an older one. That distinction only
    /// matters because a version bump can move a document's output
    /// path: `rendered_md/<chat_uuid>/…` is named for an id the
    /// renderer mints, so a change to the id recipe writes the new
    /// document beside the old one instead of over it, and both then
    /// load into the index as separate rows. Re-rendering in place —
    /// what a bumped `source_fingerprint` gets you on its own — is only
    /// correct while the path is stable.
    ///
    /// **Every processor that writes sidecars must override this.** The
    /// `None` default is for the download half, which writes none; a
    /// processor that writes sidecars and returns `None` fails its
    /// render step, naming the source. That is deliberate. The default
    /// cannot be "opt out": a provider added later would inherit the opt
    /// out by writing no code at all, and the resulting duplicate rows
    /// are the kind of failure nothing downstream complains about.
    ///
    /// Return the same constant the render path passes to
    /// the store — one per source, since the
    /// thing being versioned is the whole `rendered_md` tree. The render
    /// step re-reads the tree afterwards and fails if it carries a
    /// version nobody declared, so reporting one number and writing
    /// another is caught on the first run rather than silently
    /// re-rendering the source from scratch forever.
    fn render_version(&self) -> Option<u32> {
        None
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
    /// far, so an interrupt doesn't lose it.
    async fn checkpoint(&self) -> Result<()>;
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
    /// observability the orchestrator installs as scopes. `None` on a render
    /// context.
    metrics: Option<Arc<DownloadMetrics>>,
    diagnostics: Option<Arc<Diagnostics>>,
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
