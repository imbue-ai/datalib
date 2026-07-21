//! Indicatif-backed [`ProgressSink`] shared by every binary that wants
//! a live terminal progress bar — the CLI pipeline binaries
//! and the standalone provider CLIs (`fsindex`, the various
//! `<provider>_download` bins) alike.
//!
//! The bar attaches to the process-wide `MultiProgress` published by
//! [`frankweiler_obs::init`], so log emissions suspend bar draws before
//! writing and the two never stomp each other. A caller that hasn't run
//! `obs::init` (tests, `--log-format` piping with no TTY) transparently
//! falls back to a tracing-only sink — no bar, but the structured event
//! stream is unchanged.
//!
//! Two entry points:
//!   * [`Progress::indicatif`] — the one-liner. Builds a `Progress` that
//!     fans a bar + a [`TracingSink`] out from one emission point. This
//!     is what a CLI wants: `progress: Progress::indicatif("fsindex")`.
//!   * [`make_bar`] / [`IndicatifSink::new`] — the building blocks, for
//!     callers (sync) that manage one `MultiProgress` across many bars
//!     themselves.
//!
//! Inner bars (e.g. per-channel progress within a Slack source) are
//! created on demand via `Progress::child(prefix)`, which routes to
//! [`IndicatifSink::child`] and attaches a fresh child bar to the same
//! MultiProgress so it renders nested under its parent.

use std::sync::Arc;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::progress::{FanOut, Progress, ProgressSink, TracingSink};

pub struct IndicatifSink {
    bar: ProgressBar,
    // Held so child bars can attach to the same MultiProgress. Cheap
    // to clone (it's an `Arc` internally).
    multi: Arc<MultiProgress>,
    // Nesting depth — 0 for the top-level bar, 1+ for children spawned
    // via `child(...)`. Used to indent the prefix column so nested bars
    // visually belong to their parent.
    depth: usize,
    // When true the bar renders only `{prefix} {spinner} {msg}` — no
    // `{pos}`/`{per_sec}` headline. For callers whose message already
    // carries every counter and rate (fsindex's scan dashboard), where
    // the headline would just duplicate them unlabeled.
    message_only: bool,
}

impl IndicatifSink {
    pub fn new(bar: ProgressBar, multi: Arc<MultiProgress>) -> Self {
        Self {
            bar,
            multi,
            depth: 0,
            message_only: false,
        }
    }

    fn prefix_width(&self) -> usize {
        prefix_width_at_depth(self.depth)
    }
}

impl ProgressSink for IndicatifSink {
    fn set_length(&self, total: Option<u64>) {
        // A message-only bar has no headline counters, so `{pos}`/`{len}`
        // are irrelevant — leave the template alone.
        if self.message_only {
            return;
        }
        // Also swap the template. Without this, indicatif's
        // length-unset state renders `{len}` in lockstep with `{pos}`
        // (visually "1185/1185, 1241/1241, ..."), which is misleading —
        // the bar implies it knows the total when it really doesn't.
        // Switch to a spinner-only template until a real total arrives,
        // and switch back when the caller learns it.
        match total {
            Some(t) => {
                self.bar.set_length(t);
                self.bar
                    .set_style(determinate_style(self.depth, self.prefix_width()));
            }
            None => {
                self.bar.unset_length();
                self.bar
                    .set_style(spinner_style(self.depth, self.prefix_width()));
            }
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
    fn finish_and_clear(&self) {
        self.bar.finish_and_clear();
    }
    fn child(&self, prefix: &str) -> Arc<dyn ProgressSink> {
        let depth = self.depth + 1;
        let child_bar = if self.message_only {
            make_message_only_bar_at_depth(&self.multi, prefix.to_string(), depth)
        } else {
            make_bar_at_depth(&self.multi, prefix.to_string(), depth)
        };
        Arc::new(IndicatifSink {
            bar: child_bar,
            multi: self.multi.clone(),
            depth,
            message_only: self.message_only,
        })
    }
}

impl Progress {
    /// Build a `Progress` that drives a live indicatif bar (attached to
    /// obs's shared `MultiProgress`) **and** a [`TracingSink`], fanned
    /// out from one emission point — the same wiring the orchestrator
    /// gives each source.
    ///
    /// Falls back to a tracing-only `Progress` when
    /// [`frankweiler_obs::shared_multi`] returns `None` (i.e. `obs::init`
    /// hasn't run — tests, or a binary that opted out of the bar). The
    /// structured event stream is identical either way; only the
    /// terminal bar is conditional.
    pub fn indicatif(prefix: impl Into<String>) -> Progress {
        Self::indicatif_inner(prefix.into(), false)
    }

    /// Like [`Progress::indicatif`] but the bar is a bare
    /// `{prefix} {spinner} {msg}` — no `{pos}`/`{per_sec}` headline. For
    /// callers whose `set_message` string already carries every counter
    /// and rate (e.g. fsindex's scan dashboard), where the headline
    /// would only duplicate them, unlabeled.
    pub fn indicatif_message_only(prefix: impl Into<String>) -> Progress {
        Self::indicatif_inner(prefix.into(), true)
    }

    fn indicatif_inner(prefix: String, message_only: bool) -> Progress {
        let tracing: Arc<dyn ProgressSink> = Arc::new(TracingSink::new(prefix.clone()));
        match frankweiler_obs::shared_multi() {
            Some(multi) => {
                let bar = if message_only {
                    make_message_only_bar(&multi, prefix)
                } else {
                    make_bar(&multi, prefix)
                };
                let sink = IndicatifSink {
                    bar,
                    multi,
                    depth: 0,
                    message_only,
                };
                let sinks = vec![Arc::new(sink) as Arc<dyn ProgressSink>, tracing];
                Progress::new(Arc::new(FanOut::new(sinks)))
            }
            None => Progress::new(tracing),
        }
    }
}

/// The process-wide `MultiProgress` owned by `frankweiler_obs`, whose
/// draws are suspended by every tracing log emission. Panics if
/// `obs::init` hasn't published one yet — callers that can't guarantee
/// that ordering should use [`Progress::indicatif`], which falls back
/// gracefully instead.
pub fn make_multi() -> Arc<MultiProgress> {
    frankweiler_obs::shared_multi()
        .expect("frankweiler_obs::init must run before indicatif_progress::make_multi")
}

/// Build a fresh top-level bar attached to the given MultiProgress.
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
    let bar = multi.add(ProgressBar::new_spinner());
    let prefix_width = prefix_width_at_depth(depth);
    // Start in spinner mode — `set_length(Some(_))` flips to the
    // determinate template later if/when the caller learns the total.
    bar.set_style(spinner_style(depth, prefix_width));
    bar.set_prefix(prefix.into());
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar
}

/// A bar that renders only `{prefix} {spinner} {msg}` — no headline
/// counters. The message is the whole display.
pub fn make_message_only_bar(multi: &MultiProgress, prefix: impl Into<String>) -> ProgressBar {
    make_message_only_bar_at_depth(multi, prefix, 0)
}

fn make_message_only_bar_at_depth(
    multi: &MultiProgress,
    prefix: impl Into<String>,
    depth: usize,
) -> ProgressBar {
    let bar = multi.add(ProgressBar::new_spinner());
    bar.set_style(message_only_style(depth, prefix_width_at_depth(depth)));
    bar.set_prefix(prefix.into());
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar
}

// `prefix:>14` columns + a 2-cell indent per nesting depth. Bound the
// indent so the prefix field never shrinks below a usable width even
// if a caller spawns deeply nested children.
const PREFIX_COL_WIDTH: usize = 14;
const INDENT_PER_DEPTH: usize = 2;

fn prefix_width_at_depth(depth: usize) -> usize {
    let indent = (depth * INDENT_PER_DEPTH).min(PREFIX_COL_WIDTH.saturating_sub(4));
    PREFIX_COL_WIDTH - indent
}

fn leading_at_depth(depth: usize) -> String {
    if depth == 0 {
        return String::new();
    }
    let indent = (depth * INDENT_PER_DEPTH).min(PREFIX_COL_WIDTH.saturating_sub(4));
    // Replace the final two indent columns with a tree marker ("↳ ")
    // so the parent/child relationship reads at a glance. Column count
    // is unchanged — "↳ " renders as two cells.
    let mut s = " ".repeat(indent.saturating_sub(2));
    s.push_str("↳ ");
    s
}

/// Determinate-style template — used once a real `set_length(Some(_))`
/// arrives. Renders the standard `{pos}/{len} [bar]` shape with
/// throughput + message tail.
fn determinate_style(depth: usize, prefix_width: usize) -> ProgressStyle {
    let leading = leading_at_depth(depth);
    let template = format!(
        "{leading}{{prefix:>{prefix_width}}} {{spinner}} {{pos:>5}}/{{len:5}} [{{wide_bar}}] {{per_sec:>10}} {{msg}}"
    );
    ProgressStyle::with_template(&template)
        .unwrap()
        .progress_chars("=> ")
}

/// Spinner-style template — used when the total is unknown. Shows the
/// running position and the message tail but no `{len}` field, because
/// indicatif renders `{len}` against a length-unset bar in a way that
/// visually mirrors `{pos}` (looks like "1185/1185" and updates in
/// lockstep), implying a known total when there is none. We elide
/// `{wide_bar}` for the same reason: a determinate bar against an
/// unknown total is misleading.
fn spinner_style(depth: usize, prefix_width: usize) -> ProgressStyle {
    let leading = leading_at_depth(depth);
    let template = format!(
        "{leading}{{prefix:>{prefix_width}}} {{spinner}} {{pos:>5}} {{per_sec:>10}} {{msg}}"
    );
    ProgressStyle::with_template(&template).unwrap()
}

/// Message-only template — `{prefix} {spinner} {msg}`, no headline
/// counters. The caller's message owns the entire readout.
fn message_only_style(depth: usize, prefix_width: usize) -> ProgressStyle {
    let leading = leading_at_depth(depth);
    let template = format!("{leading}{{prefix:>{prefix_width}}} {{spinner}} {{msg}}");
    ProgressStyle::with_template(&template).unwrap()
}
