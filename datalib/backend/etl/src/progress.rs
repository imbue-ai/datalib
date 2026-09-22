//! Progress reporting hook for long-running download / render work.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Object-safe progress sink. Implementors do whatever rendering they
/// want; the worker calls these methods to report state.
pub trait ProgressSink: Send + Sync {
    fn set_length(&self, _total: Option<u64>) {}
    fn inc(&self, _delta: u64) {}
    fn set_message(&self, _msg: &str) {}
    fn finish(&self, _msg: &str) {}
    /// Like [`finish`], but the sink should remove its visual element
    /// when possible (so hundreds of done-bars don't accumulate). For
    /// indicatif this is `finish_and_clear`; sinks that have nothing
    /// to clear can treat it as `finish`.
    fn finish_and_clear(&self) {}
    /// A producer sealed what it has written so far, and `version` names
    /// the result. Distinct from the progress calls above: those say how far
    /// along the work is, this says a consumer could start on it.
    fn checkpoint(&self, _version: &str) {}
    /// A seal that says how many rows it added over the one before.
    /// The runner keeps each consumer's queue depth from these, so a
    /// producer that knows the number should use this form.
    fn checkpoint_rows(&self, version: &str, _rows: u64) {
        self.checkpoint(version);
    }
    /// The current value of one named number — rows written, requests
    /// made, items queued. Always the whole value, never a delta: the
    /// runner's store coalesces to the newest, and a dropped position
    /// costs nothing where a dropped delta is lost work.
    fn metric(&self, _name: &str, _labels: &[(&str, &str)], _value: i64) {}
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
    pub fn checkpoint(&self, version: &str) {
        self.sink.checkpoint(version);
    }
    pub fn checkpoint_rows(&self, version: &str, rows: u64) {
        self.sink.checkpoint_rows(version, rows);
    }
    pub fn metric(&self, name: &str, labels: &[(&str, &str)], value: i64) {
        self.sink.metric(name, labels, value);
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

/// One bar for a whole run, whose announced total only ever grows.
///
/// The DAG runner relabels every child bar's events back to the step
/// that emitted them, then computes the step's `queued` metric — the
/// "N queued" the Manage screen shows — as `total - done`, where `done`
/// is the sum of *every* increment the step made and `total` is
/// whichever length it announced last. So a step with two bars, each
/// announcing its own size, pins `queued` at zero the moment the second
/// one starts: `done` already carries the first bar's work.
///
/// A run therefore has one bar and one running total, and each phase
/// adds what it has learned it will do. The total is shared and
/// interior-mutable so the handle can be passed around by reference the
/// way [`Progress`] is; a phase that fans out should tick from its
/// tasks and announce from the one that plans them.
///
/// It wraps the step's own [`Progress`] rather than a [`Progress::child`]
/// of it. A child buys a nested bar on a terminal and nothing else — the
/// runner relabels a child's events back to the step regardless — and it
/// costs correctness against any sink that does not override
/// `ProgressSink::child`, whose default returns a sink that silently
/// drops everything.
#[derive(Clone)]
pub struct RunBar {
    bar: Progress,
    announced: Arc<AtomicU64>,
}

impl RunBar {
    /// `fixed` is work the run will certainly do and already knows the
    /// size of — the coarse per-phase or per-unit ticks a bar starts
    /// with, so it reads as something other than 0/0 before the first
    /// response lands.
    ///
    /// A `fixed` of zero announces nothing at all. A run that does not
    /// yet know its size has no total, which is not the same as a total
    /// of zero: the runner publishes no `queued` for a step that never
    /// announced one, where a `queued` of 0 means "nothing left to do"
    /// and the Manage screen reads it as idle.
    pub fn new(progress: &Progress, fixed: u64) -> Self {
        if fixed > 0 {
            progress.set_length(Some(fixed));
        }
        Self {
            bar: progress.clone(),
            announced: Arc::new(AtomicU64::new(fixed)),
        }
    }

    /// Another `more` items this run has committed to handling.
    pub fn expect(&self, more: u64) {
        let total = self.announced.fetch_add(more, Ordering::SeqCst) + more;
        if total > 0 {
            self.bar.set_length(Some(total));
        }
    }

    /// Raise the total to `at_least` if it is not already there. For a
    /// phase whose size is one number repeated or refined as it goes,
    /// where adding each reading would count the same work twice.
    pub fn expect_at_least(&self, at_least: u64) {
        if self
            .announced
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                (at_least > cur).then_some(at_least)
            })
            .is_ok()
        {
            self.bar.set_length(Some(at_least));
        }
    }

    /// What has been announced so far, so a phase can raise the total to
    /// "everything before me, plus my own size".
    pub fn announced(&self) -> u64 {
        self.announced.load(Ordering::SeqCst)
    }

    pub fn did(&self, n: u64) {
        self.bar.inc(n);
    }

    pub fn doing(&self, what: &str) {
        self.bar.set_message(what);
    }

    pub fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

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
    // `RUST_LOG=datalib_etl::progress=trace`) when actually
    // consuming the event stream.
    fn set_length(&self, total: Option<u64>) {
        tracing::trace!(
            event = "progress.length",
            source = %self.source,
            total = total.map(|t| t as i64).unwrap_or(-1),
            "the progress bar learned its total"
        );
    }
    fn inc(&self, delta: u64) {
        tracing::trace!(
            event = "progress.inc",
            source = %self.source,
            delta = delta,
            "the progress bar advanced"
        );
    }
    // Not TRACE, unlike its neighbours: a checkpoint is rare, and it is the
    // line that explains why a consumer woke up early.
    fn checkpoint(&self, version: &str) {
        tracing::info!(
            event = "progress.checkpoint",
            source = %self.source,
            version = %version,
            "sealed a checkpoint"
        );
    }
    fn checkpoint_rows(&self, version: &str, rows: u64) {
        tracing::info!(
            event = "progress.checkpoint",
            source = %self.source,
            version = %version,
            rows = rows,
            "sealed a checkpoint"
        );
    }
    fn metric(&self, name: &str, labels: &[(&str, &str)], value: i64) {
        tracing::trace!(
            event = "progress.metric",
            source = %self.source,
            name = name,
            labels = %labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(","),
            value = value,
            "a metric was reported"
        );
    }
    fn set_message(&self, msg: &str) {
        tracing::trace!(
            event = "progress.message",
            source = %self.source,
            msg = msg,
            "the progress bar's message changed"
        );
    }
    fn finish(&self, msg: &str) {
        tracing::trace!(
            event = "progress.finish",
            source = %self.source,
            msg = msg,
            "the progress bar finished"
        );
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
    fn checkpoint(&self, version: &str) {
        for s in &self.sinks {
            s.checkpoint(version);
        }
    }
    fn checkpoint_rows(&self, version: &str, rows: u64) {
        for s in &self.sinks {
            s.checkpoint_rows(version, rows);
        }
    }
    fn metric(&self, name: &str, labels: &[(&str, &str)], value: i64) {
        for s in &self.sinks {
            s.metric(name, labels, value);
        }
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A sink that just counts how many times `finish_and_clear` fired, so a
    /// test can assert a wrapping sink (e.g. `FanOut`) forwards the call
    /// instead of silently hitting the no-op default trait method..
    #[derive(Default, Clone)]
    struct RecordingSink {
        finish_and_clear: Arc<AtomicUsize>,
        checkpoints: Arc<std::sync::Mutex<Vec<String>>>,
        metrics: Arc<std::sync::Mutex<Vec<(String, i64)>>>,
    }
    impl ProgressSink for RecordingSink {
        fn finish_and_clear(&self) {
            self.finish_and_clear.fetch_add(1, Ordering::SeqCst);
        }
        fn checkpoint(&self, version: &str) {
            self.checkpoints.lock().unwrap().push(version.to_string());
        }
        fn checkpoint_rows(&self, version: &str, rows: u64) {
            self.checkpoints
                .lock()
                .unwrap()
                .push(format!("{version}+{rows}"));
        }
        fn metric(&self, name: &str, _labels: &[(&str, &str)], value: i64) {
            self.metrics.lock().unwrap().push((name.to_string(), value));
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

    /// Same gap as the test above, one method later. A producer's seal
    /// reaches the wire through `FanOut`, so a missing forward here means the
    /// `checkpoint` event silently never leaves the step -- and nothing
    /// downstream would look broken, it would just never stream.
    #[test]
    fn fanout_forwards_checkpoint_to_every_sink() {
        let a = Arc::new(RecordingSink::default());
        let b = Arc::new(RecordingSink::default());
        let sinks: Vec<Arc<dyn ProgressSink>> = vec![a.clone(), b.clone()];
        let fan = FanOut::new(sinks);

        fan.checkpoint("deadbeef");

        assert_eq!(
            *a.checkpoints.lock().unwrap(),
            vec!["deadbeef".to_string()],
            "FanOut must forward checkpoint to its first sink",
        );
        assert_eq!(
            *b.checkpoints.lock().unwrap(),
            vec!["deadbeef".to_string()],
            "FanOut must forward checkpoint to its second sink",
        );
    }

    /// A seal with a row count must reach the leaf *as* that form, not
    /// fall through to the plain one — the default trait method does
    /// exactly that fall-through, so a `FanOut` that forgot to forward
    /// would silently drop every row count.
    #[test]
    fn fanout_forwards_checkpoint_rows_as_itself() {
        let a = Arc::new(RecordingSink::default());
        let sinks: Vec<Arc<dyn ProgressSink>> = vec![a.clone()];
        FanOut::new(sinks).checkpoint_rows("abc", 12);
        assert_eq!(*a.checkpoints.lock().unwrap(), vec!["abc+12".to_string()]);
    }

    /// One method later again: a metric that stops at `FanOut` never
    /// reaches the wire, and the Manage screen shows a step with no numbers
    /// rather than anything that looks broken.
    #[test]
    fn fanout_forwards_metric_to_every_sink() {
        let a = Arc::new(RecordingSink::default());
        let b = Arc::new(RecordingSink::default());
        let sinks: Vec<Arc<dyn ProgressSink>> = vec![a.clone(), b.clone()];
        let fan = FanOut::new(sinks);

        fan.metric("rows_upserted", &[("table", "messages")], 12);

        for (name, sink) in [("first", &a), ("second", &b)] {
            assert_eq!(
                *sink.metrics.lock().unwrap(),
                vec![("rows_upserted".to_string(), 12)],
                "FanOut must forward metric to its {name} sink",
            );
        }
    }
}
