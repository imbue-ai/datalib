//! The `DataProcessor` trait and its run context — Program A's uniform
//! pipeline unit.
//!
//! A *data source* contributes one or more `DataProcessor`s grouped into the
//! two waves Program A keeps ([`SourcePlan`]): an extract processor (ingests
//! from the outside world), a translate processor (reads artifacts, emits
//! rendered docs), or just one of them. "Extract-only" / "translate-only" is
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

use crate::control::ExtractControl;
use crate::load::RenderedMarkdown;
use crate::progress::Progress;
use crate::synthesize::Synthesizer;

/// One config-driven, monitorable unit of work the orchestrator runs.
/// Single method.
///
/// See the module docs for the storage-agnostic contract: a processor that
/// persists work owns its store and exposes only an opaque [`Checkpoint`]
/// (registered via [`RunCtx::register_checkpoint`]) for interrupt-safety.
#[async_trait]
pub trait DataProcessor: Send + Sync {
    /// Stable identifier for logs + progress, e.g. `"email/fastmail/extract"`.
    fn id(&self) -> &str;

    /// Do the work. Returns a short human summary for the run log. (A
    /// *structured* outcome with a content-version is a Program-B concern;
    /// Program A keeps the string.)
    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String>;
}

/// What a provider's builder produces per configured source: its processors
/// grouped into the two waves Program A keeps. Program B replaces this
/// grouping with order derived from each processor's declared inputs/outputs.
///
/// Extract-only sources leave `translate` empty; translate-only sources
/// leave `extract` empty — no flag, no no-op method.
#[derive(Default)]
pub struct SourcePlan {
    /// Run in the extract wave (ingest from the outside world).
    pub extract: Vec<Box<dyn DataProcessor>>,
    /// Run in the translate wave (read artifacts, emit rendered docs).
    pub translate: Vec<Box<dyn DataProcessor>>,
}

impl SourcePlan {
    /// An empty plan — convenience for builders that add waves conditionally.
    pub fn new() -> Self {
        Self::default()
    }
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

/// An optional capability: a processor that can synthesize its own playback
/// fixtures (the `--synthesize-playback-root` mode). Kept OFF the universal
/// [`DataProcessor`] trait — only some extract processors have it — so the
/// core trait stays about the thing every processor does.
pub trait HasSynthesizer {
    /// The provider's fixture synthesizer.
    fn synthesizer(&self) -> Box<dyn Synthesizer>;
}

/// A translate processor emits each finished document through this callback;
/// Program A keeps Load fused into it (the orchestrator's sink upserts the
/// doc inline). `Send` so a translate processor's `run` future stays `Send`
/// like every other processor's.
pub type DocCallback<'a> = dyn FnMut(RenderedMarkdown) -> Result<()> + Send + 'a;

/// Interior-mutable wrapper around the orchestrator's fused-Load callback so
/// a translate processor can emit through a shared `&RunCtx`. The `Mutex`
/// keeps [`RunCtx`] `Sync` (hence every `run` future `Send`); per-source
/// translate is sequential, so the lock is never actually contended.
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
/// and shared into every extract [`RunCtx`]. Extract processors push their
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

    fn register(&self, name: &str, hook: Arc<dyn Checkpoint>) {
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
    /// Workspace root — the parent of the `rendered_md/` tree translate
    /// processors write into.
    pub root: &'a Path,
    /// Run timestamp, threaded through for deterministic stamping.
    pub now: &'a str,
    /// Per-source progress hook.
    pub progress: &'a Progress,
    /// Cross-provider extract knobs (`--reset-and-redownload`, …).
    pub control: &'a ExtractControl,
    /// Prior-run per-markdown fingerprints, for fingerprint-driven
    /// incremental skips on the translate side.
    pub prior_fingerprints: &'a HashMap<String, String>,
    /// Where extract processors register their interrupt-commit hooks.
    checkpoints: &'a CheckpointSink,
    /// Where translate processors send finished documents (fused Load).
    /// `None` on an extract context.
    emit: Option<DocSink<'a>>,
}

impl<'a> RunCtx<'a> {
    /// Build a context for an **extract** processor (no doc sink). The
    /// processor registers its store's interrupt hook via
    /// [`register_checkpoint`](RunCtx::register_checkpoint).
    #[allow(clippy::too_many_arguments)]
    pub fn for_extract(
        name: &'a str,
        root: &'a Path,
        now: &'a str,
        progress: &'a Progress,
        control: &'a ExtractControl,
        prior_fingerprints: &'a HashMap<String, String>,
        checkpoints: &'a CheckpointSink,
    ) -> Self {
        Self {
            name,
            root,
            now,
            progress,
            control,
            prior_fingerprints,
            checkpoints,
            emit: None,
        }
    }

    /// Build a context for a **translate** processor, carrying the fused-Load
    /// callback each rendered document is emitted through.
    #[allow(clippy::too_many_arguments)]
    pub fn for_translate(
        name: &'a str,
        root: &'a Path,
        now: &'a str,
        progress: &'a Progress,
        control: &'a ExtractControl,
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
            emit: Some(DocSink {
                cb: Mutex::new(on_doc),
            }),
        }
    }

    /// Register an opaque interrupt-commit hook for this processor's store.
    /// Called by an extract processor right after it opens its store, so a
    /// Ctrl-C mid-download can still flush partial state.
    pub fn register_checkpoint(&self, name: &str, hook: Arc<dyn Checkpoint>) {
        self.checkpoints.register(name, hook);
    }

    /// Emit a finished rendered document (translate processors only). Errors
    /// if called on an extract context — that is a programming bug, since
    /// extract processors have nothing to render.
    pub fn emit_doc(&self, md: RenderedMarkdown) -> Result<()> {
        let sink = self
            .emit
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("emit_doc called on a non-translate RunCtx"))?;
        let mut cb = sink.cb.lock().unwrap();
        (cb)(md)
    }
}
