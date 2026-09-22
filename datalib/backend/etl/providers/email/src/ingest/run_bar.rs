//! One progress bar for a whole download run, whose announced total
//! only ever grows.
//!
//! Why a type rather than bare [`Progress`] calls: the DAG runner
//! relabels every progress event to the step that emitted it, so a
//! step's increments are summed across *all* its bars while `queued`
//! is computed against the newest total it was told. A second bar that
//! announces its own small total therefore makes the Manage screen's
//! "N queued" saturate at zero halfway through a run. A run has one
//! bar and one running total, and each phase adds what it has learned
//! it will do.

use datalib_etl::progress::Progress;

pub(crate) struct RunBar {
    bar: Progress,
    announced: u64,
}

impl RunBar {
    /// `fixed` is work the run will certainly do and already knows the
    /// size of — the coarse phase ticks a bar starts with.
    pub(crate) fn new(progress: &Progress, label: &str, fixed: u64) -> Self {
        let bar = progress.child(label);
        bar.set_length(Some(fixed));
        Self {
            bar,
            announced: fixed,
        }
    }

    /// Another `more` items this run has committed to handling.
    pub(crate) fn expect(&mut self, more: u64) {
        self.announced += more;
        self.bar.set_length(Some(self.announced));
    }

    /// Raise the total to `at_least` if it is not already there. For a
    /// phase whose size is an estimate that firms up as it goes, where
    /// adding each new reading would count the same work twice.
    pub(crate) fn expect_at_least(&mut self, at_least: u64) {
        if at_least > self.announced {
            self.announced = at_least;
            self.bar.set_length(Some(self.announced));
        }
    }

    /// What has been announced so far, so a phase can raise the total
    /// to "everything before me, plus my own size".
    pub(crate) fn announced(&self) -> u64 {
        self.announced
    }

    pub(crate) fn did(&self, n: u64) {
        self.bar.inc(n);
    }

    pub(crate) fn doing(&self, what: &str) {
        self.bar.set_message(what);
    }

    pub(crate) fn finish(&self) {
        self.bar.finish_and_clear();
    }
}
