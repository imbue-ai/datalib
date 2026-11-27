//! What the statements of one piece of work did, by the name each carries,
//! and how many rows they returned: what a server counts for each request
//! it answers.
//!
//! The count hangs on a tracing span, because sqlx runs each connection on
//! a thread of its own and carries the caller's span there, and nothing
//! else. So work moved to another thread or task must carry its span
//! along ([`with_count`]), or what it reads is not counted.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{Instrument, Span};

use crate::totals::{ByTag, OneRun, TagWork};
use tracing_subscriber::registry::{LookupSpan, Registry};

/// The response header in which an applet tells the gateway how many rows
/// it read to answer; the gateway writes it on the request's log line.
pub const ROWS_READ_HEADER: &str = "x-datalib-rows-read";

/// The response header in which an applet tells the gateway what its
/// statements did to answer, by name: [`header_value`] of a
/// [`Counted::statements`]. The gateway writes it on the request's line.
pub const SQL_WORK_HEADER: &str = "x-datalib-sql-work";

/// The longest [`header_value`]: well under what an HTTP client takes.
const HEADER_BYTES: usize = 16_000;

#[derive(Default)]
struct Tallied {
    rows: AtomicU64,
    statements: Mutex<HashMap<String, TagWork>>,
}

struct Tally(Arc<Tallied>);

/// What one piece of work's statements did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counted {
    /// The rows they returned to the code that ran them.
    pub rows: u64,
    pub statements: ByTag,
}

/// How many counts are under way, so that a statement in a process with
/// none costs one load.
static COUNTING: AtomicUsize = AtomicUsize::new(0);

struct Count {
    span: Span,
    /// `None` when nothing would add to it: the counter is not installed,
    /// or no subscriber keeps spans.
    tally: Option<Arc<Tallied>>,
}

impl Count {
    fn start() -> Self {
        let span = tracing::error_span!(target: crate::TARGET, "request");
        let tally = crate::installed().then(|| tally_on(&span)).flatten();
        COUNTING.fetch_add(1, Ordering::Relaxed);
        Self { span, tally }
    }

    fn so_far(&self) -> Option<Counted> {
        self.tally.as_ref().map(|t| Counted {
            rows: t.rows.load(Ordering::Relaxed),
            statements: lock(&t.statements)
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        })
    }
}

impl Drop for Count {
    fn drop(&mut self) {
        COUNTING.fetch_sub(1, Ordering::Relaxed);
    }
}

fn tally_on(span: &Span) -> Option<Arc<Tallied>> {
    span.with_subscriber(|(id, dispatch)| {
        let registry = dispatch.downcast_ref::<Registry>()?;
        let tally = Arc::new(Tallied::default());
        registry
            .span(id)?
            .extensions_mut()
            .insert(Tally(tally.clone()));
        Some(tally)
    })
    .flatten()
}

/// Runs `work` and says what its statements did; `None` when this process
/// does not count.
pub async fn counted<F: Future>(work: F) -> (F::Output, Option<Counted>) {
    let count = Count::start();
    let out = work.instrument(count.span.clone()).await;
    (out, count.so_far())
}

/// [`counted`], for work that blocks.
pub fn counted_blocking<T>(work: impl FnOnce() -> T) -> (T, Option<Counted>) {
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

/// A statement on this thread ran as `run` and returned `rows`: add it to
/// every count it runs inside.
pub(crate) fn note(rows: u64, name: &str, run: &OneRun<'_>) {
    if COUNTING.load(Ordering::Relaxed) == 0 {
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
            if let Some(Tally(tally)) = span.extensions().get::<Tally>() {
                tally.rows.fetch_add(rows, Ordering::Relaxed);
                crate::totals::add_to(&mut lock(&tally.statements), name, run);
            }
        }
    });
}

/// `statements` as the [`SQL_WORK_HEADER`] carries them: JSON, each name's
/// loops left off, then the cheapest names, until it fits.
pub fn header_value(statements: &ByTag) -> String {
    let whole = serde_json::to_string(statements).unwrap_or_default();
    if whole.len() <= HEADER_BYTES {
        return whole;
    }
    let mut by_time: Vec<(&String, TagWork)> = statements
        .iter()
        .map(|(k, v)| {
            (
                k,
                TagWork {
                    loops: Default::default(),
                    ..v.clone()
                },
            )
        })
        .collect();
    by_time.sort_by_key(|(_, w)| std::cmp::Reverse(w.micros));
    loop {
        let kept: ByTag = by_time
            .iter()
            .map(|(k, v)| ((*k).clone(), v.clone()))
            .collect();
        let text = serde_json::to_string(&kept).unwrap_or_default();
        if text.len() <= HEADER_BYTES || by_time.is_empty() {
            return text;
        }
        by_time.pop();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
