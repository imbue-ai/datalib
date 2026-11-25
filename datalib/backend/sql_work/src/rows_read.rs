//! How many rows the statements of one piece of work returned: what a
//! server counts for each request it answers.
//!
//! The count hangs on a tracing span, because sqlx runs each connection on
//! a thread of its own and carries the caller's span there, and nothing
//! else. So work moved to another thread or task must carry its span
//! along ([`with_count`]), or what it reads is not counted.

use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use tracing::{Instrument, Span};
use tracing_subscriber::registry::{LookupSpan, Registry};

/// The response header in which an applet tells the gateway how many rows
/// it read to answer; the gateway writes it on the request's log line.
pub const ROWS_READ_HEADER: &str = "x-datalib-rows-read";

struct Tally(Arc<AtomicU64>);

/// How many counts are under way, so that a statement in a process with
/// none costs one load.
static COUNTING: AtomicUsize = AtomicUsize::new(0);

struct Count {
    span: Span,
    /// `None` when nothing would add to it: the counter is not installed,
    /// or no subscriber keeps spans.
    rows: Option<Arc<AtomicU64>>,
}

impl Count {
    fn start() -> Self {
        let span = tracing::error_span!(target: crate::TARGET, "request");
        let rows = crate::installed().then(|| tally_on(&span)).flatten();
        COUNTING.fetch_add(1, Ordering::Relaxed);
        Self { span, rows }
    }

    fn so_far(&self) -> Option<u64> {
        self.rows.as_ref().map(|r| r.load(Ordering::Relaxed))
    }
}

impl Drop for Count {
    fn drop(&mut self) {
        COUNTING.fetch_sub(1, Ordering::Relaxed);
    }
}

fn tally_on(span: &Span) -> Option<Arc<AtomicU64>> {
    span.with_subscriber(|(id, dispatch)| {
        let registry = dispatch.downcast_ref::<Registry>()?;
        let rows = Arc::new(AtomicU64::new(0));
        registry
            .span(id)?
            .extensions_mut()
            .insert(Tally(rows.clone()));
        Some(rows)
    })
    .flatten()
}

/// Runs `work` and says how many rows its statements returned; `None` when
/// this process does not count.
pub async fn counted<F: Future>(work: F) -> (F::Output, Option<u64>) {
    let count = Count::start();
    let out = work.instrument(count.span.clone()).await;
    (out, count.so_far())
}

/// [`counted`], for work that blocks.
pub fn counted_blocking<T>(work: impl FnOnce() -> T) -> (T, Option<u64>) {
    let count = Count::start();
    let out = count.span.in_scope(work);
    (out, count.so_far())
}

/// `work` as a closure another thread can run and the count under way here
/// still sees: what to hand `spawn_blocking`.
pub fn with_count<T>(work: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let span = Span::current();
    move || span.in_scope(work)
}

/// A statement on this thread returned `rows`: add them to every count it
/// runs inside.
pub(crate) fn note(rows: u64) {
    if rows == 0 || COUNTING.load(Ordering::Relaxed) == 0 {
        return;
    }
    tracing::dispatcher::get_default(|dispatch| {
        let Some(registry) = dispatch.downcast_ref::<Registry>() else {
            return;
        };
        let Some(id) = dispatch.current_span().into_inner().map(|(id, _)| id) else {
            return;
        };
        let Some(span) = registry.span(&id) else {
            return;
        };
        for span in span.scope() {
            if let Some(Tally(total)) = span.extensions().get::<Tally>() {
                total.fetch_add(rows, Ordering::Relaxed);
            }
        }
    });
}
