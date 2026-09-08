//! The `DataProcessor` trait and its run context — Program A's uniform
//! pipeline unit.

use std::collections::{HashMap, HashSet};
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
#[async_trait]
pub trait DataProcessor: Send + Sync {
    fn id(&self) -> &str;

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String>;

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

/// Whether a render pass actually walked its source's documents.
///
/// [`RunCtx::retain_documents`] deletes every document the pass did not name,
/// which is right after a real walk and catastrophic after a bail: an empty
/// set from a renderer that never looked is indistinguishable, at the sweep,
/// from a source that genuinely lost everything. A renderer that returns
/// early — no store on disk, nothing committed to read — says so with
/// `Skipped`, and the sweep does not run.
///
/// It is a return value rather than a flag the caller sets because the
/// caller is not the one who knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderPass {
    /// The source's documents were enumerated; anything unnamed is gone.
    Walked,
    /// The pass returned before enumerating anything.
    Skipped,
}

/// An opaque "persist what you have" hook. A processor that buffers work into
/// a store registers one of these at the moment it opens the store; the
/// orchestrator holds the registered hooks and fires them on SIGINT.
#[async_trait]
pub trait Checkpoint: Send + Sync {
    async fn checkpoint(&self) -> Result<()>;
}

/// A render processor emits each finished document through this callback;
/// Program A keeps Load fused into it (the orchestrator's sink upserts the
/// doc inline). `Send` so a render processor's `run` future stays `Send`
/// like every other processor's.
pub type DocCallback<'a> = dyn FnMut(RenderedMarkdown) -> Result<()> + Send + 'a;

/// The counterpart to [`DocCallback`]: a render processor names a
/// conversation the raw store no longer has, and every document rendered
/// from it goes — rows and `.md` alike.
///
/// Keyed by conversation rather than by document because a periodizing
/// renderer produced several documents from one conversation and cannot
/// recompute how many once the conversation is gone. The store resolves it.
pub type RemoveCallback<'a> = dyn FnMut(&str) -> Result<usize> + Send + 'a;

/// The whole-store form of the same thing: a renderer that walked its
/// entire raw store names every document that store should produce, and
/// anything else the render store holds is a document whose source is gone.
///
/// For a renderer that walks everything this is both simpler and stronger
/// than naming vanished ids one at a time — it needs no diff, and it cannot
/// miss a deletion the diff failed to mention.
pub type RetainCallback<'a> = dyn FnMut(&HashSet<String>) + Send + 'a;

/// Interior-mutable wrapper around the orchestrator's fused-Load callback so
/// a render processor can emit through a shared `&RunCtx`. The `Mutex`
/// keeps [`RunCtx`] `Sync` (hence every `run` future `Send`); per-source
/// render is sequential, so the lock is never actually contended.
struct DocSink<'a> {
    cb: Mutex<&'a mut DocCallback<'a>>,
}

/// Same wrapper, for the removal half of the sink.
struct RemoveSink<'a> {
    cb: Mutex<&'a mut RemoveCallback<'a>>,
}

/// Same wrapper, for the whole-store retain half.
struct RetainSink<'a> {
    cb: Mutex<&'a mut RetainCallback<'a>>,
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
    /// Where render processors name conversations that went away.
    /// `None` on a download context.
    remove: Option<RemoveSink<'a>>,
    /// Where a whole-store renderer declares the complete document set.
    /// `None` on a download context.
    retain: Option<RetainSink<'a>>,
}

impl<'a> RunCtx<'a> {
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
            remove: None,
            retain: None,
        }
    }

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
        on_remove: &'a mut RemoveCallback<'a>,
        on_retain: &'a mut RetainCallback<'a>,
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
            remove: Some(RemoveSink {
                cb: Mutex::new(on_remove),
            }),
            retain: Some(RetainSink {
                cb: Mutex::new(on_retain),
            }),
        }
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
        self.metrics
            .clone()
            .expect("metrics() on a non-download RunCtx")
    }

    pub fn diagnostics(&self) -> Arc<Diagnostics> {
        self.diagnostics
            .clone()
            .expect("diagnostics() on a non-download RunCtx")
    }

    pub fn emit_doc(&self, md: RenderedMarkdown) -> Result<()> {
        let sink = self
            .emit
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("emit_doc called on a non-render RunCtx"))?;
        let mut cb = sink.cb.lock().unwrap();
        (cb)(md)
    }

    /// This conversation is no longer in the raw store: drop every document
    /// rendered from it. Returns how many went.
    ///
    /// Call it only for a conversation the run actually looked for and did
    /// not find — an id the `dolt_diff` scan named, whose rows the parse then
    /// came back empty for. Absence from a bucket the run never examined
    /// means nothing.
    pub fn remove_conversation(&self, conversation_uuid: &str) -> Result<usize> {
        let sink = self
            .remove
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("remove_conversation called on a non-render RunCtx"))?;
        let mut cb = sink.cb.lock().unwrap();
        (cb)(conversation_uuid)
    }

    /// Declare the complete set of documents this source should hold.
    ///
    /// Only for a renderer that walked its **whole** raw store this run —
    /// then anything the render store holds and this set does not name is a
    /// document whose source is gone. A renderer narrowed by a `dolt_diff`
    /// scan must not call it: most of what it did not name this run it
    /// simply did not look at. That one wants `remove_conversation`.
    ///
    /// Include documents skipped on an unchanged fingerprint. "Considered
    /// and unchanged" and "no longer there" are the two states this call
    /// separates, and a renderer that reports only what it re-rendered
    /// deletes its own steady state.
    ///
    /// Calls accumulate: a source with several render processors builds the
    /// set across all of them, and the sweep runs once at the end.
    pub fn retain_documents(&self, pass: RenderPass, document_uuids: &HashSet<String>) {
        if pass == RenderPass::Skipped {
            // Nothing walked, so `document_uuids` is empty because nobody
            // looked — not because the source lost everything. Sweeping on
            // that deletes the whole source. See [`RenderPass`].
            tracing::info!(
                source = %self.name,
                "render did not walk this source; leaving its documents alone"
            );
            return;
        }
        let Some(sink) = self.retain.as_ref() else {
            return;
        };
        let mut cb = sink.cb.lock().unwrap();
        (cb)(document_uuids);
    }
}

#[cfg(test)]
mod retain_tests {
    use super::*;

    /// `retain_documents` deletes every document the pass did not name. That
    /// is right after a walk and catastrophic after a bail: a renderer that
    /// returned before looking hands over an empty set, which at the sweep is
    /// indistinguishable from a source that genuinely lost everything.
    ///
    /// The triggers are real — no store on disk, or (since render reads
    /// committed state only) a store with nothing committed. "The dolt
    /// extensions are missing" must not mean "delete this source".
    #[test]
    fn a_pass_that_did_not_walk_does_not_sweep() {
        let swept: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        let empty: HashMap<String, String> = HashMap::new();
        let checkpoints = CheckpointSink::new();
        let progress = Progress::noop();
        let control = DownloadControl::default();

        let mut on_doc: Box<DocCallback<'_>> = Box::new(|_| Ok(()));
        let mut on_remove: Box<RemoveCallback<'_>> = Box::new(|_| Ok(0));
        let mut on_retain: Box<RetainCallback<'_>> =
            Box::new(|ids: &HashSet<String>| swept.lock().unwrap().push(ids.len()));

        let ctx = RunCtx::for_render(
            "src",
            Path::new("/tmp"),
            "2026-01-01T00:00:00+00:00",
            &progress,
            &control,
            &empty,
            &checkpoints,
            &mut on_doc,
            &mut on_remove,
            &mut on_retain,
        );

        ctx.retain_documents(RenderPass::Skipped, &HashSet::new());
        assert!(
            swept.lock().unwrap().is_empty(),
            "a pass that never walked must not reach the sweep"
        );

        ctx.retain_documents(RenderPass::Walked, &HashSet::new());
        assert_eq!(
            *swept.lock().unwrap(),
            vec![0],
            "a real walk that named nothing still sweeps — that is a source \
             which genuinely lost everything"
        );
    }
}
