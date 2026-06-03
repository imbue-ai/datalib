//! Indicatif-backed sink for `frankweiler_etl::progress::Progress`.
//!
//! Sync creates one `MultiProgress` for the run and one `ProgressBar`
//! per managed source; each bar is wrapped in an [`IndicatifSink`] and
//! handed to the provider via `FetchOptions.progress`. Combined with
//! `frankweiler_etl::progress::TracingSink` under a `FanOut`, every
//! emission point drives both the terminal UI and the structured event
//! stream.
//!
//! Inner bars (e.g. per-channel progress within a Slack source) are
//! created on demand via `Progress::child(prefix)`, which routes to
//! [`IndicatifSink::child`] and attaches a fresh child bar to the same
//! MultiProgress so it renders nested under its parent.

use std::sync::Arc;

use frankweiler_etl::progress::ProgressSink;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

pub struct IndicatifSink {
    bar: ProgressBar,
    // Held so child bars can attach to the same MultiProgress. Cheap
    // to clone (it's an `Arc` internally).
    multi: Arc<MultiProgress>,
    // Nesting depth — 0 for the per-source bar, 1+ for children
    // spawned via `child(...)`. Used to indent the prefix column so
    // nested bars visually belong to their parent.
    depth: usize,
}

impl IndicatifSink {
    pub fn new(bar: ProgressBar, multi: Arc<MultiProgress>) -> Self {
        Self {
            bar,
            multi,
            depth: 0,
        }
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
    fn child(&self, prefix: &str) -> Arc<dyn ProgressSink> {
        let depth = self.depth + 1;
        let child_bar = make_bar_at_depth(&self.multi, prefix.to_string(), depth);
        Arc::new(IndicatifSink {
            bar: child_bar,
            multi: self.multi.clone(),
            depth,
        })
    }
}

/// The process-wide `MultiProgress` owned by `frankweiler_obs`, whose
/// draws are suspended by every tracing log emission. Constructing a
/// separate `MultiProgress` is forbidden by `clippy.toml` —
/// `obs::init` is the single source of truth, and the sync binary
/// always calls it before reaching this code path.
pub fn make_multi() -> Arc<MultiProgress> {
    frankweiler_obs::shared_multi()
        .expect("frankweiler_obs::init must run before sync::progress::make_multi")
}

/// Build a fresh per-source bar attached to the given MultiProgress.
/// Starts as an indeterminate spinner with `{prefix}`; switches to a
/// determinate bar once the worker calls `set_length`.
pub fn make_bar(multi: &MultiProgress, prefix: impl Into<String>) -> ProgressBar {
    make_bar_at_depth(multi, prefix, 0)
}

/// Like [`make_bar`] but indents the prefix column by `depth` levels so
/// nested child bars visually belong to their parent. Depth 0 matches
/// `make_bar` exactly; each level adds two leading spaces and shrinks
/// the prefix field by the same amount, keeping every column aligned.
pub fn make_bar_at_depth(
    multi: &MultiProgress,
    prefix: impl Into<String>,
    depth: usize,
) -> ProgressBar {
    const PREFIX_COL_WIDTH: usize = 14;
    const INDENT_PER_DEPTH: usize = 2;

    let bar = multi.add(ProgressBar::new_spinner());
    // Bound depth so the prefix field never shrinks below a usable
    // width even if some caller spawns deeply nested children.
    let indent = (depth * INDENT_PER_DEPTH).min(PREFIX_COL_WIDTH.saturating_sub(4));
    let prefix_width = PREFIX_COL_WIDTH - indent;
    // For nested bars, replace the final two indent columns with a
    // tree marker ("↳ ") so the parent/child relationship reads at a
    // glance. Column count is unchanged — "↳ " renders as two cells.
    let leading = if depth == 0 {
        String::new()
    } else {
        let mut s = " ".repeat(indent.saturating_sub(2));
        s.push_str("↳ ");
        s
    };
    let template = format!(
        "{leading}{{prefix:>{prefix_width}}} {{spinner}} {{pos:>5}}/{{len:5}} [{{wide_bar}}] {{per_sec:>10}} {{msg}}"
    );
    bar.set_style(
        ProgressStyle::with_template(&template)
            .unwrap()
            .progress_chars("=> "),
    );
    bar.set_prefix(prefix.into());
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar
}
