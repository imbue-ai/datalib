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
