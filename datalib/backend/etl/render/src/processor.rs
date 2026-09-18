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
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Stage};

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

/// A bucket the run rendered, with every raw row its render asked for:
/// whatever else the store holds under that bucket is gone, and a later
/// change to any of those rows names the bucket again.
pub type DeclareCallback<'a> = dyn FnMut(&str, &[Input]) -> Result<()> + Send + 'a;

/// A processor reports what it could not do with raw entities — a
/// payload that would not deserialize has no document to hang a
/// problem off — and the driver stores the rows under those entities.
/// The [`ReadScope`] says what the report is complete for: every
/// parse-stage row on an entity of a table the parse read whole is
/// replaced by the report, so a row that reads cleanly again is
/// cleared; a table read in part keeps what it had and gains the
/// report.
pub type ProblemsCallback<'a> = dyn FnMut(&ReadScope, &[ProblemRow]) -> Result<()> + Send + 'a;

/// What a problem report is the whole truth about, and so what it
/// replaces in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadScope {
    /// Every row of these raw tables was read: the report is complete
    /// for them, and an entity not in it reads cleanly now.
    Whole(Vec<&'static str>),
    /// Only some rows were read — the changed ones. The report adds
    /// to what the store holds and clears nothing it does not name.
    Partial,
    /// One document, by `markdown_uuid`, that this run could not
    /// produce at all — a conversion that failed. Replaces the
    /// document's rows, the way emitting it would; the run that does
    /// produce it clears them again.
    Document(String),
}

/// One raw row a parse could not read: the table and id that name it
/// in the raw store, and what the payload looked like. What a provider
/// records instead of `continue`ing past the row — the half of R1
/// ("drop, count, log; never abort, never hide") that a silent skip
/// loses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unparsed {
    pub table: &'static str,
    pub id: String,
    pub sample: String,
}

impl Unparsed {
    /// `sample` is the payload's first characters, or the parse error
    /// when the payload is not text at all.
    pub fn new(table: &'static str, id: impl Into<String>, sample: &str) -> Self {
        Unparsed {
            table,
            id: id.into(),
            sample: datalib_schema::problems::sample_of(sample),
        }
    }

    /// The sweep key: unique across a store's tables, so a `users` row
    /// and a `messages` row with the same id are two entities. The
    /// table is the prefix, which is how a whole-table report finds
    /// the rows it replaces.
    pub fn entity_id(&self) -> String {
        format!("{}:{}", self.table, self.id)
    }
}

/// Interior-mutable wrapper around the orchestrator's fused-Load callback so
/// a render processor can emit through a shared `&RenderCtx`. The `Mutex`
/// keeps [`RenderCtx`] `Sync` (hence every `run` future `Send`); per-source
/// render is sequential, so the lock is never actually contended.
struct DocSink<'a> {
    cb: Mutex<&'a mut DocCallback<'a>>,
}

struct DeclareSink<'a> {
    cb: Mutex<&'a mut DeclareCallback<'a>>,
}

struct ProblemsSink<'a> {
    cb: Mutex<&'a mut ProblemsCallback<'a>>,
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
    declare: DeclareSink<'a>,
    problems: ProblemsSink<'a>,
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
        on_declare: &'a mut DeclareCallback<'a>,
        on_problems: &'a mut ProblemsCallback<'a>,
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
            declare: DeclareSink {
                cb: Mutex::new(on_declare),
            },
            problems: ProblemsSink {
                cb: Mutex::new(on_problems),
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

    /// What this run could not do with raw entities, before it knew
    /// which documents they were for; `scope` says which tables the
    /// report is complete for. A problem that belongs to a document
    /// goes on the document's `RenderedMarkdown::problems` instead,
    /// where its sweep is.
    pub fn report_entity_problems(&self, scope: &ReadScope, rows: &[ProblemRow]) -> Result<()> {
        let mut cb = self.problems.cb.lock().unwrap();
        (cb)(scope, rows)
    }

    /// The common case of the above: raw payloads that would not
    /// deserialize, each dropped whole. A provider's parse collects
    /// them as [`Unparsed`] instead of `continue`ing past them, and its
    /// processor hands them here with the scope it read them under.
    pub fn report_unparsed(
        &self,
        scope: &ReadScope,
        unparsed: &[Unparsed],
        render_version: Option<u32>,
    ) -> Result<()> {
        let rows: Vec<ProblemRow> = unparsed
            .iter()
            .map(|u| {
                ProblemRow::new(
                    self.name,
                    Stage::Parse,
                    Scope::Entity(&u.entity_id()),
                    None,
                    Outcome::Dropped,
                    Problem::record(Reason::Undeserializable, &u.sample),
                    render_version,
                )
            })
            .collect();
        self.report_entity_problems(scope, &rows)
    }

    /// A document this run tried to produce and could not — the
    /// converter failed, the renderer gave up. Recorded on the
    /// document's own scope, so the run that produces it sweeps the
    /// row like any other. `sample` is the error.
    pub fn report_document_failed(
        &self,
        markdown_uuid: &str,
        error: &str,
        render_version: Option<u32>,
    ) -> Result<()> {
        let row = ProblemRow::new(
            self.name,
            Stage::Render,
            Scope::Markdown(markdown_uuid),
            Some(markdown_uuid),
            Outcome::Dropped,
            Problem::record(Reason::RenderFailed, error),
            render_version,
        );
        self.report_entity_problems(&ReadScope::Document(markdown_uuid.to_string()), &[row])
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
}
