//! Progress reporting hook for long-running Extract / Translate work.
//!
//! Each provider's `extract::fetch` (and any other unit of work that wants
//! to surface progress) accepts an optional [`Progress`] handle. When set,
//! the work emits `inc`/`set_message`/`set_length` calls as it makes
//! forward progress; the consumer (e.g. `frankweiler-sync`'s indicatif
//! `MultiProgress`) renders those into a live UI.
//!
//! `Progress` is a thin `Arc<dyn ProgressSink>` wrapper so it's cheap to
//! clone and pass between async tasks. The `noop` constructor returns a
//! handle that discards everything — useful as a default in `FetchOptions`
//! so existing callers don't need to plumb a UI through.

use std::sync::Arc;

/// Object-safe progress sink. Implementors do whatever rendering they
/// want; the worker calls these methods to report state.
pub trait ProgressSink: Send + Sync {
    /// Set the total expected unit count, if known. Pass `None` to
    /// switch back to indeterminate (spinner) mode.
    fn set_length(&self, _total: Option<u64>) {}
    /// Advance the position by `delta` units.
    fn inc(&self, _delta: u64) {}
    /// Replace the human-readable status message (e.g. "channel C123").
    fn set_message(&self, _msg: &str) {}
    /// Mark the work finished. The sink may render a final summary line.
    fn finish(&self, _msg: &str) {}
    /// Like [`finish`], but the sink should remove its visual element
    /// when possible (so hundreds of done-bars don't accumulate). For
    /// indicatif this is `finish_and_clear`; sinks that have nothing
    /// to clear can treat it as `finish`.
    fn finish_and_clear(&self) {}
    /// Spawn a nested progress sink (e.g. a per-channel inner bar
    /// underneath the per-source outer bar). Default returns a noop so
    /// existing sinks remain backward-compatible.
    fn child(&self, _prefix: &str) -> Arc<dyn ProgressSink> {
        Arc::new(NoopSink)
    }
}

/// Cheap-to-clone progress handle. Calls forward to the inner
/// [`ProgressSink`]. The default value is a no-op sink so `Default::default()`
/// works in `FetchOptions` structs.
#[derive(Clone)]
pub struct Progress {
    sink: Arc<dyn ProgressSink>,
}

impl Progress {
    pub fn new(sink: Arc<dyn ProgressSink>) -> Self {
        Self { sink }
    }
    pub fn noop() -> Self {
        Self {
            sink: Arc::new(NoopSink),
        }
    }
    pub fn set_length(&self, total: Option<u64>) {
        self.sink.set_length(total);
    }
    pub fn inc(&self, delta: u64) {
        self.sink.inc(delta);
    }
    pub fn set_message(&self, msg: &str) {
        self.sink.set_message(msg);
    }
    pub fn finish(&self, msg: &str) {
        self.sink.finish(msg);
    }
    pub fn finish_and_clear(&self) {
        self.sink.finish_and_clear();
    }
    /// Spawn a child `Progress` (e.g. an inner per-unit bar nested
    /// inside this one). Wraps `ProgressSink::child`; returns a noop
    /// handle if the underlying sink doesn't support nesting.
    pub fn child(&self, prefix: &str) -> Progress {
        Progress::new(self.sink.child(prefix))
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::noop()
    }
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Progress").finish_non_exhaustive()
    }
}

struct NoopSink;
impl ProgressSink for NoopSink {}

/// Structured-event sink: each progress call becomes a `tracing::info!`
/// event with a fixed `event = "progress.*"` field plus a `source`
/// discriminator. Lets non-TTY consumers (JSON log shipping, Tauri's
/// tracing-bridge) pick up the same stream the indicatif renderer
/// consumes.
pub struct TracingSink {
    source: String,
}

impl TracingSink {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

impl ProgressSink for TracingSink {
    fn set_length(&self, total: Option<u64>) {
        tracing::info!(
            event = "progress.length",
            source = %self.source,
            total = total.map(|t| t as i64).unwrap_or(-1),
        );
    }
    fn inc(&self, delta: u64) {
        tracing::info!(
            event = "progress.inc",
            source = %self.source,
            delta = delta,
        );
    }
    fn set_message(&self, msg: &str) {
        tracing::info!(
            event = "progress.message",
            source = %self.source,
            msg = msg,
        );
    }
    fn finish(&self, msg: &str) {
        tracing::info!(
            event = "progress.finish",
            source = %self.source,
            msg = msg,
        );
    }
    fn child(&self, prefix: &str) -> Arc<dyn ProgressSink> {
        Arc::new(TracingSink::new(format!("{}/{}", self.source, prefix)))
    }
}

/// Fan a single `Progress` call out to several sinks. Used by sync to
/// drive both an indicatif bar and the tracing event stream from one
/// emission point.
pub struct FanOut {
    sinks: Vec<Arc<dyn ProgressSink>>,
}

impl FanOut {
    pub fn new(sinks: Vec<Arc<dyn ProgressSink>>) -> Self {
        Self { sinks }
    }
}

impl ProgressSink for FanOut {
    fn set_length(&self, total: Option<u64>) {
        for s in &self.sinks {
            s.set_length(total);
        }
    }
    fn inc(&self, delta: u64) {
        for s in &self.sinks {
            s.inc(delta);
        }
    }
    fn set_message(&self, msg: &str) {
        for s in &self.sinks {
            s.set_message(msg);
        }
    }
    fn finish(&self, msg: &str) {
        for s in &self.sinks {
            s.finish(msg);
        }
    }
    fn child(&self, prefix: &str) -> Arc<dyn ProgressSink> {
        Arc::new(FanOut {
            sinks: self.sinks.iter().map(|s| s.child(prefix)).collect(),
        })
    }
}
