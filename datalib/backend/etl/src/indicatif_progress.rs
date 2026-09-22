//! Indicatif-backed [`ProgressSink`] shared by every binary that wants
//! a live terminal progress bar — the CLI pipeline binaries
//! and the standalone provider CLIs (`fsindex`, the various
//! `<provider>_download` bins) alike.
//!
//! One bar per step, never a tree of them. A download reports through a
//! single [`crate::progress::RunBar`] whose total only grows, so there
//! is nothing for a nested bar to show that the message does not.

use std::sync::Arc;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::progress::{FanOut, Progress, ProgressSink, TracingSink};

struct IndicatifSink {
    bar: ProgressBar,
    // When true the bar renders only `{prefix} {spinner} {msg}` — no
    // `{pos}`/`{per_sec}` headline. For callers whose message already
    // carries every counter and rate (fsindex's scan dashboard), where
    // the headline would just duplicate them unlabeled.
    message_only: bool,
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
                self.bar.set_style(determinate_style());
            }
            None => {
                self.bar.unset_length();
                self.bar.set_style(spinner_style());
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
}

impl Progress {
    /// Build a `Progress` that drives a live indicatif bar (attached to
    /// obs's shared `MultiProgress`) **and** a [`TracingSink`], fanned
    /// out from one emission point — the same wiring the orchestrator
    /// gives each source.
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
        match datalib_obs::shared_multi() {
            Some(multi) => {
                let sink = IndicatifSink {
                    bar: make_bar(&multi, prefix, message_only),
                    message_only,
                };
                let sinks = vec![Arc::new(sink) as Arc<dyn ProgressSink>, tracing];
                Progress::new(Arc::new(FanOut::new(sinks)))
            }
            None => Progress::new(tracing),
        }
    }
}

fn make_bar(multi: &MultiProgress, prefix: String, message_only: bool) -> ProgressBar {
    let bar = multi.add(ProgressBar::new_spinner());
    // Starts as a spinner even when it will learn a total: `set_length`
    // flips it to the determinate template the moment one arrives.
    bar.set_style(if message_only {
        message_only_style()
    } else {
        spinner_style()
    });
    bar.set_prefix(prefix);
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar
}

const PREFIX_COL_WIDTH: usize = 14;

fn determinate_style() -> ProgressStyle {
    let template = format!(
        "{{prefix:>{PREFIX_COL_WIDTH}}} {{spinner}} {{pos:>5}}/{{len:5}} [{{wide_bar}}] {{per_sec:>10}} {{msg}}"
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
fn spinner_style() -> ProgressStyle {
    let template =
        format!("{{prefix:>{PREFIX_COL_WIDTH}}} {{spinner}} {{pos:>5}} {{per_sec:>10}} {{msg}}");
    ProgressStyle::with_template(&template).unwrap()
}

/// Message-only template — `{prefix} {spinner} {msg}`, no headline
/// counters. The caller's message owns the entire readout.
fn message_only_style() -> ProgressStyle {
    let template = format!("{{prefix:>{PREFIX_COL_WIDTH}}} {{spinner}} {{msg}}");
    ProgressStyle::with_template(&template).unwrap()
}
