//! Indicatif-backed sink for `frankweiler_etl::progress::Progress`.
//!
//! Sync creates one `MultiProgress` for the run and one `ProgressBar`
//! per managed source; each bar is wrapped in an [`IndicatifSink`] and
//! handed to the provider via `FetchOptions.progress`. Combined with
//! `frankweiler_etl::progress::TracingSink` under a `FanOut`, every
//! emission point drives both the terminal UI and the structured event
//! stream.

use std::sync::Arc;

use frankweiler_etl::progress::ProgressSink;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

pub struct IndicatifSink {
    bar: ProgressBar,
}

impl IndicatifSink {
    pub fn new(bar: ProgressBar) -> Self {
        Self { bar }
    }
}

impl ProgressSink for IndicatifSink {
    fn set_length(&self, total: Option<u64>) {
        match total {
            Some(t) => self.bar.set_length(t),
            None => self.bar.unset_length(),
        }
    }
    fn inc(&self, delta: u64) {
        self.bar.inc(delta);
    }
    fn set_message(&self, msg: &str) {
        self.bar.set_message(msg.to_string());
    }
    fn finish(&self, msg: &str) {
        self.bar.finish_with_message(msg.to_string());
    }
}

/// Create a `MultiProgress` configured for `stderr`. Returns the
/// MultiProgress + a constructor closure for per-source bars.
pub fn make_multi() -> Arc<MultiProgress> {
    Arc::new(MultiProgress::new())
}

/// Build a fresh per-source bar attached to the given MultiProgress.
/// Starts as an indeterminate spinner with `{prefix}`; switches to a
/// determinate bar once the worker calls `set_length`.
pub fn make_bar(multi: &MultiProgress, prefix: impl Into<String>) -> ProgressBar {
    let bar = multi.add(ProgressBar::new_spinner());
    bar.set_style(
        ProgressStyle::with_template(
            "{prefix:>14} {spinner} {pos:>5}/{len:5} [{wide_bar}] {per_sec:>10} {msg}",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    bar.set_prefix(prefix.into());
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar
}
