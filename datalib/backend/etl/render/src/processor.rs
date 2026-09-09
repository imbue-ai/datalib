//! The render processor and its run context.
//!
//! Split from [`datalib_etl::processor`], which keeps the download
//! half. The two contexts share no fields: a render pass wants prior
//! fingerprints and the three document sinks, a download wants the
//! store handle, the metrics scope and the interrupt hooks. Fusing
//! them into one struct meant every field was `Option` and half the
//! accessors panicked on the wrong phase — and, because the sinks
//! carry `RenderedMarkdown`, it also put `datalib_schema` in front of
//! every downloader.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::progress::Progress;

use crate::grid_index::RenderedMarkdown;

/// One source's render wave, as a unit the step driver can run.
#[async_trait]
pub trait RenderProcessor: Send + Sync {
    fn id(&self) -> &str;

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String>;

    /// The renderer version this processor writes. `None` from a
    /// processor that does not version its output; the driver treats a
    /// source with any undeclared processor as undeclared overall.
    fn render_version(&self) -> Option<u32> {
        None
    }
}

/// Whether a render pass actually walked its source's documents.
///
/// [`RenderCtx::retain_documents`] deletes every document the pass did not
/// name, which is right after a real walk and catastrophic after a bail: an
/// empty set from a renderer that never looked is indistinguishable, at the
/// sweep, from a source that genuinely lost everything. A renderer that
/// returns early — no store on disk, nothing committed to read — says so with
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
/// a render processor can emit through a shared `&RenderCtx`. The `Mutex`
/// keeps [`RenderCtx`] `Sync` (hence every `run` future `Send`); per-source
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

/// Driver-owned context handed to every [`RenderProcessor::run`].
pub struct RenderCtx<'a> {
    /// Source name (`sources[].name`).
    pub name: &'a str,
    /// Workspace root — the parent of the `rendered_md/` tree render
    /// processors write into.
    pub root: &'a Path,
    /// Run timestamp, threaded through for deterministic stamping.
    pub now: &'a str,
    /// Per-source progress hook.
    pub progress: &'a Progress,
    /// Prior-run per-markdown fingerprints, for fingerprint-driven
    /// incremental skips.
    pub prior_fingerprints: &'a HashMap<String, String>,
    emit: DocSink<'a>,
    remove: RemoveSink<'a>,
    retain: RetainSink<'a>,
}

impl<'a> RenderCtx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &'a str,
        root: &'a Path,
        now: &'a str,
        progress: &'a Progress,
        prior_fingerprints: &'a HashMap<String, String>,
        on_doc: &'a mut DocCallback<'a>,
        on_remove: &'a mut RemoveCallback<'a>,
        on_retain: &'a mut RetainCallback<'a>,
    ) -> Self {
        Self {
            name,
            root,
            now,
            progress,
            prior_fingerprints,
            emit: DocSink {
                cb: Mutex::new(on_doc),
            },
            remove: RemoveSink {
                cb: Mutex::new(on_remove),
            },
            retain: RetainSink {
                cb: Mutex::new(on_retain),
            },
        }
    }

    pub fn emit_doc(&self, md: RenderedMarkdown) -> Result<()> {
        let mut cb = self.emit.cb.lock().unwrap();
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
        let mut cb = self.remove.cb.lock().unwrap();
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
        let mut cb = self.retain.cb.lock().unwrap();
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
        let progress = Progress::noop();

        let mut on_doc: Box<DocCallback<'_>> = Box::new(|_| Ok(()));
        let mut on_remove: Box<RemoveCallback<'_>> = Box::new(|_| Ok(0));
        let mut on_retain: Box<RetainCallback<'_>> =
            Box::new(|ids: &HashSet<String>| swept.lock().unwrap().push(ids.len()));

        let ctx = RenderCtx::new(
            "src",
            Path::new("/tmp"),
            "2026-01-01T00:00:00+00:00",
            &progress,
            &empty,
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
