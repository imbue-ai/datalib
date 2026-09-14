//! The render processor and its run context.
//!
//! Split from [`datalib_etl::processor`], which keeps the download
//! half. The two contexts share no fields: a render pass wants the raw
//! cursor and the three document sinks, a download wants the
//! store handle, the metrics scope and the interrupt hooks. Fusing
//! them into one struct meant every field was `Option` and half the
//! accessors panicked on the wrong phase — and, because the sinks
//! carry `RenderedMarkdown`, it also put `datalib_schema` in front of
//! every downloader.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::progress::Progress;

use crate::grid_index::RenderedMarkdown;
pub use crate::indexed_markdown::Input;
use crate::inputs::RawRange;

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

    /// The knobs that shape this processor's output — a period, a label
    /// filter. The driver stores them beside the cursor and, when they
    /// differ from the stored ones, renders every bucket again rather
    /// than only the changed ones. A processor with no knobs returns the
    /// empty object, which never differs from itself.
    fn render_params(&self) -> serde_json::Value {
        serde_json::json!({})
    }
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

/// A bucket the run rendered, with every raw row its render asked for:
/// whatever else the store holds under that bucket is gone, and a later
/// change to any of those rows names the bucket again.
pub type DeclareCallback<'a> = dyn FnMut(&str, &[Input]) -> Result<()> + Send + 'a;

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

struct DeclareSink<'a> {
    cb: Mutex<&'a mut DeclareCallback<'a>>,
}

/// Driver-owned context handed to every [`RenderProcessor::run`].
pub struct RenderCtx<'a> {
    /// The source's id — its group, the directory its rendered tree
    /// lives under. Not the display name the config may also give it.
    pub name: &'a str,
    /// Workspace root — the parent of the `render_markdown/` tree render
    /// processors write into.
    pub root: &'a Path,
    /// Run timestamp, threaded through for deterministic stamping.
    pub now: &'a str,
    /// Per-source progress hook.
    pub progress: &'a Progress,
    /// The raw-store commit the previous render consumed: the `from_ref`
    /// for this run's `dolt_diff` scan. `None` means render everything —
    /// there is no earlier render, or the driver wants every bucket again
    /// (renderer version or params changed).
    pub raw_cursor: Option<&'a str>,
    /// The raw-store commit the driver pinned for this run, when it did.
    /// A provider pins the same one (`Pin::at`) rather than sampling HEAD,
    /// so what it loads is what `stale_buckets` was computed against.
    pub raw_pin: Option<&'a str>,
    /// Buckets whose declared inputs changed since `raw_cursor`, from the
    /// driver's reverse lookup in `render_inputs`: render these, plus
    /// whatever the provider's own forward scan adds. `None` when the
    /// driver could not say — no cursor, or nothing declared yet — and
    /// the provider's scan is on its own.
    pub stale_buckets: Option<&'a HashSet<String>>,
    emit: DocSink<'a>,
    remove: RemoveSink<'a>,
    declare: DeclareSink<'a>,
    consumed: Mutex<Option<String>>,
}

impl<'a> RenderCtx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &'a str,
        root: &'a Path,
        now: &'a str,
        progress: &'a Progress,
        raw_cursor: Option<&'a str>,
        raw_pin: Option<&'a str>,
        stale_buckets: Option<&'a HashSet<String>>,
        on_doc: &'a mut DocCallback<'a>,
        on_remove: &'a mut RemoveCallback<'a>,
        on_declare: &'a mut DeclareCallback<'a>,
    ) -> Self {
        Self {
            name,
            root,
            now,
            progress,
            raw_cursor,
            raw_pin,
            stale_buckets,
            emit: DocSink {
                cb: Mutex::new(on_doc),
            },
            remove: RemoveSink {
                cb: Mutex::new(on_remove),
            },
            declare: DeclareSink {
                cb: Mutex::new(on_declare),
            },
            consumed: Mutex::new(None),
        }
    }

    /// This run rendered `bucket_key`, and `inputs` is every raw row it
    /// asked for — found or not: a thread rendered while its author's
    /// `users` row had not arrived records `(users, U123)` anyway, so the
    /// row's arrival names the thread. Any document the store holds
    /// under that bucket which this run did not emit is one the bucket
    /// no longer produces, and the driver removes it at the end.
    ///
    /// Only for a bucket the run actually rendered; a bucket it skipped
    /// says nothing about its documents. Every emitted document carries
    /// its `bucket_key` — that is how the driver knows which are the
    /// bucket's.
    /// The raw store as this run should read it — see [`RawRange`].
    pub fn raw_range(&self) -> RawRange<'a> {
        RawRange {
            cursor: self.raw_cursor,
            pin: self.raw_pin,
            stale: self.stale_buckets,
        }
    }

    pub fn declare_bucket(&self, bucket_key: &str, inputs: &[Input]) -> Result<()> {
        let mut cb = self.declare.cb.lock().unwrap();
        (cb)(bucket_key, inputs)
    }

    pub fn emit_doc(&self, md: RenderedMarkdown) -> Result<()> {
        let mut cb = self.emit.cb.lock().unwrap();
        (cb)(md)
    }

    /// This run pinned `raw_commit` and rendered from it. The driver
    /// records it as the cursor in the run's final transaction, and a run
    /// that rendered everything treats it as proof the walk was complete.
    /// A processor that never read a raw store — none on disk, nothing
    /// committed — says nothing, and the cursor stays where it was.
    pub fn consumed(&self, raw_commit: &str) {
        *self.consumed.lock().unwrap() = Some(raw_commit.to_string());
    }

    pub fn consumed_commit(&self) -> Option<String> {
        self.consumed.lock().unwrap().clone()
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
}
