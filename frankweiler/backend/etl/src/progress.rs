//! Progress reporting hook for long-running download / render work.
//!
//! Each provider's `download::fetch` (and any other unit of work that wants
//! to surface progress) accepts an optional [`Progress`] handle. When set,
//! the work emits `inc`/`set_message`/`set_length` calls as it makes
//! forward progress; the consumer (e.g. the indicatif
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
    // All events fire at TRACE level. They're high-frequency
    // observability for structured-log consumers; INFO would
    // interleave each tick with indicatif's terminal control
    // sequences and visibly corrupt the progress bars on stderr.
    // Opt back in with `--log-level=trace` (or
    // `RUST_LOG=frankweiler_etl::progress=trace`) when actually
    // consuming the event stream.
    fn set_length(&self, total: Option<u64>) {
        tracing::trace!(
            event = "progress.length",
            source = %self.source,
            total = total.map(|t| t as i64).unwrap_or(-1),
        );
    }
    fn inc(&self, delta: u64) {
        tracing::trace!(
            event = "progress.inc",
            source = %self.source,
            delta = delta,
        );
    }
    fn set_message(&self, msg: &str) {
        tracing::trace!(
            event = "progress.message",
            source = %self.source,
            msg = msg,
        );
    }
    fn finish(&self, msg: &str) {
        tracing::trace!(
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
    fn finish_and_clear(&self) {
        for s in &self.sinks {
            s.finish_and_clear();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A sink that just counts how many times `finish_and_clear` fired, so a
    /// test can assert a wrapping sink (e.g. `FanOut`) forwards the call
    /// instead of silently hitting the no-op default trait method. Children
    /// share the same counter — mirroring how a real leaf sink spawns its own
    /// child bars — so the count survives nesting through `FanOut::child`.
    #[derive(Default, Clone)]
    struct RecordingSink {
        finish_and_clear: Arc<AtomicUsize>,
    }
    impl ProgressSink for RecordingSink {
        fn finish_and_clear(&self) {
            self.finish_and_clear.fetch_add(1, Ordering::SeqCst);
        }
        fn child(&self, _prefix: &str) -> Arc<dyn ProgressSink> {
            Arc::new(self.clone())
        }
    }

    // Regression: `FanOut` once implemented `inc`/`finish`/etc. but *not*
    // `finish_and_clear`, so the orchestrator's end-of-run
    // `progress.finish_and_clear()` fell through to the empty default trait
    // method and never reached the wrapped indicatif bar. The outer bar was
    // never finished, so it stayed pinned at N/N with a forever-decaying
    // per-second rate. This asserts the call reaches every wrapped sink.
    #[test]
    fn fanout_forwards_finish_and_clear_to_every_sink() {
        let a = Arc::new(RecordingSink::default());
        let b = Arc::new(RecordingSink::default());
        let sinks: Vec<Arc<dyn ProgressSink>> = vec![a.clone(), b.clone()];
        let fan = FanOut::new(sinks);

        fan.finish_and_clear();

        assert_eq!(
            a.finish_and_clear.load(Ordering::SeqCst),
            1,
            "FanOut must forward finish_and_clear to its first sink",
        );
        assert_eq!(
            b.finish_and_clear.load(Ordering::SeqCst),
            1,
            "FanOut must forward finish_and_clear to its second sink",
        );
    }

    // The same gap affected inner per-unit bars: providers call
    // `inner.finish_and_clear()` on a `FanOut::child`, which is itself a
    // `FanOut`, so the forward has to work through nesting too.
    #[test]
    fn fanout_child_forwards_finish_and_clear() {
        let leaf = Arc::new(RecordingSink::default());
        let sinks: Vec<Arc<dyn ProgressSink>> = vec![leaf.clone()];
        let fan = FanOut::new(sinks);

        let child = fan.child("inner");
        child.finish_and_clear();

        assert_eq!(
            leaf.finish_and_clear.load(Ordering::SeqCst),
            1,
            "FanOut child must forward finish_and_clear down to the leaf sink",
        );
    }
}
