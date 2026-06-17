//! Per-source diagnostics capture: every WARN/ERROR `tracing` event a
//! source emits during its extract is collected so the orchestrator can
//! fold it into the per-source block of the JSON sync summary.
//!
//! The mechanism mirrors `frankweiler_etl::extract_metrics`: a
//! [`tokio::task_local`] holds a [`Diagnostics`] buffer for the duration of
//! one source's extract (installed by [`scope`]). A global
//! [`DiagnosticsLayer`] — added to the subscriber by [`crate::init`] —
//! intercepts every WARN/ERROR event and appends a rendered line to the
//! ambient buffer, if one is installed on the current task. This captures
//! both *wire-level* warnings (the shared HTTP chokepoint's rate-limit /
//! backoff `warn!`, transport-error logs) and any *internally generated*
//! `warn!` / `error!` a provider emits — with zero provider-side code.
//!
//! Caveats: only events emitted on the source's own task are attributed
//! (an event from a detached `spawn`/`spawn_blocking` won't see the
//! task-local); and capture is gated by the global `EnvFilter`, so an event
//! the log filter suppresses entirely is not collected (the default filter
//! passes WARN/ERROR for every target).

use std::fmt::Write as _;
use std::future::Future;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::Level;
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// One captured WARN/ERROR event. Fields are kept as plain strings so any
/// consumer (the sync summary JSON, a test) can render them without
/// depending on `tracing` internals.
#[derive(Debug, Clone)]
pub struct DiagnosticEntry {
    /// `"WARN"` or `"ERROR"`.
    pub level: String,
    /// The emitting module path, e.g. `frankweiler_etl::http`.
    pub target: String,
    /// The event's `message` plus any structured fields, rendered as
    /// `message  key=value …`.
    pub message: String,
}

/// Hard cap on entries retained per source, so a pathological run that
/// logs millions of warnings can't exhaust memory. Anything past the cap
/// is counted in [`Diagnostics::dropped`] but not stored.
const MAX_ENTRIES: usize = 500;

/// Per-source buffer of captured WARN/ERROR events. Cheap to clone behind
/// the `Arc` the orchestrator hands out; the same handle is installed as
/// the ambient context ([`scope`]) and read back after the extract.
#[derive(Default)]
pub struct Diagnostics {
    entries: Mutex<Vec<DiagnosticEntry>>,
    dropped: Mutex<usize>,
}

impl Diagnostics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn push(&self, entry: DiagnosticEntry) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= MAX_ENTRIES {
            *self.dropped.lock().unwrap() += 1;
            return;
        }
        entries.push(entry);
    }

    /// A copy of everything captured so far.
    pub fn snapshot(&self) -> Vec<DiagnosticEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// Count of entries discarded after hitting [`MAX_ENTRIES`].
    pub fn dropped(&self) -> usize {
        *self.dropped.lock().unwrap()
    }

    /// `(warnings, errors)` counts over the retained entries.
    pub fn counts(&self) -> (usize, usize) {
        let entries = self.entries.lock().unwrap();
        let errors = entries.iter().filter(|e| e.level == "ERROR").count();
        (entries.len() - errors, errors)
    }
}

tokio::task_local! {
    static CURRENT: Arc<Diagnostics>;
}

/// Install `diagnostics` as the ambient capture buffer for the duration of
/// `fut`. WARN/ERROR events emitted anywhere within `fut` on the same task
/// are appended to it. Everything outside any `scope` is dropped.
pub async fn scope<F>(diagnostics: Arc<Diagnostics>, fut: F) -> F::Output
where
    F: Future,
{
    CURRENT.scope(diagnostics, fut).await
}

/// Renders an event's `message` field and any structured fields into a
/// single line. The `message` field is special-cased to lead; the rest
/// trail as `key=value`, matching how the fmt layer reads.
#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: String,
}

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }
}

impl LineVisitor {
    fn finish(mut self) -> String {
        if !self.fields.is_empty() {
            if !self.message.is_empty() {
                self.message.push_str("  ");
            }
            self.message.push_str(&self.fields);
        }
        self.message
    }
}

/// The global layer that feeds [`Diagnostics`]. Added to the subscriber by
/// [`crate::init`]; a no-op on every task that has no buffer installed.
pub struct DiagnosticsLayer;

impl<S: tracing::Subscriber> Layer<S> for DiagnosticsLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if level != Level::WARN && level != Level::ERROR {
            return;
        }
        // Skip the (cheap) render entirely when no buffer is installed.
        if CURRENT.try_with(|_| ()).is_err() {
            return;
        }
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);
        let entry = DiagnosticEntry {
            level: level.to_string(),
            target: event.metadata().target().to_string(),
            message: visitor.finish(),
        };
        let _ = CURRENT.try_with(|d| d.push(entry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_only_inside_scope_and_counts() {
        let diag = Diagnostics::new();
        let probe = diag.clone();
        // Directly exercise the buffer (the layer is global + hard to
        // install per-test; on_event just funnels into `push`).
        scope(diag, async {
            let d = CURRENT.with(|d| d.clone());
            d.push(DiagnosticEntry {
                level: "WARN".into(),
                target: "t".into(),
                message: "slow down".into(),
            });
            d.push(DiagnosticEntry {
                level: "ERROR".into(),
                target: "t".into(),
                message: "boom".into(),
            });
        })
        .await;
        let snap = probe.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(probe.counts(), (1, 1));
        assert_eq!(probe.dropped(), 0);
    }

    #[test]
    fn cap_bounds_retained_entries() {
        let diag = Diagnostics::default();
        for _ in 0..(MAX_ENTRIES + 10) {
            diag.push(DiagnosticEntry {
                level: "WARN".into(),
                target: "t".into(),
                message: "x".into(),
            });
        }
        assert_eq!(diag.snapshot().len(), MAX_ENTRIES);
        assert_eq!(diag.dropped(), 10);
    }
}
