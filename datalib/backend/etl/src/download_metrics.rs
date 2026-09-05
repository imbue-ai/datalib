//! General per-download-step "what changed" metrics.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::progress::ProgressSink;

// Live counters + ambient task-local context

/// Per-source live counters, accumulated at the shared chokepoints for
/// the duration of one source's download. Cheap to clone behind the
/// `Arc` the orchestrator hands out.
#[derive(Default)]
pub struct DownloadMetrics {
    /// Total requests issued through [`crate::http::latchkey_curl`].
    /// Stays 0 for file-based ingestion (mbox, vCard, Signal, WhatsApp),
    /// which never touches the network transport.
    api_requests: AtomicU64,
    /// Rows passed through the entity/CAS upsert chokepoints, keyed by
    /// table (`cas_objects` for the blob store). Counts *attempts* —
    /// some are no-op updates / `INSERT OR IGNORE` dupes — which is the
    /// requested "rows_upserted (some upserts may be updates)" signal.
    rows_upserted: Mutex<BTreeMap<String, u64>>,
    /// The real progress sink for this source's top-level bar, stored so
    /// a chokepoint can re-render the live suffix as counters move.
    /// `None` until [`DownloadMetrics::attach_bar`] runs (e.g. headless).
    bar: Mutex<Option<Arc<dyn ProgressSink>>>,
    /// The latest message the provider set on its bar, so re-renders
    /// triggered by counter updates don't clobber it.
    provider_msg: Mutex<String>,
}

impl DownloadMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn record_api_request(&self) {
        self.api_requests.fetch_add(1, Ordering::Relaxed);
        self.render();
    }

    pub fn record_upserts(&self, table: &str, n: u64) {
        if n == 0 {
            return;
        }
        {
            let mut m = self.rows_upserted.lock().unwrap();
            *m.entry(table.to_string()).or_insert(0) += n;
        }
        self.render();
    }

    pub fn api_requests(&self) -> u64 {
        self.api_requests.load(Ordering::Relaxed)
    }

    pub fn rows_upserted_total(&self) -> u64 {
        self.rows_upserted.lock().unwrap().values().sum()
    }

    pub fn rows_upserted_snapshot(&self) -> BTreeMap<String, u64> {
        self.rows_upserted.lock().unwrap().clone()
    }

    pub fn attach_bar(&self, sink: Arc<dyn ProgressSink>) {
        *self.bar.lock().unwrap() = Some(sink);
    }

    fn set_provider_message(&self, msg: &str) {
        *self.provider_msg.lock().unwrap() = msg.to_string();
        self.render();
    }

    fn compose(&self) -> String {
        let msg = self.provider_msg.lock().unwrap().clone();
        let api = self.api_requests.load(Ordering::Relaxed);
        let rows = self.rows_upserted.lock().unwrap();
        let mut suffix = String::new();
        if api > 0 {
            suffix.push_str(&format!("api={api}"));
        }
        if !rows.is_empty() {
            if !suffix.is_empty() {
                suffix.push(' ');
            }
            let parts: Vec<String> = rows.iter().map(|(t, n)| format!("{t}={n}")).collect();
            suffix.push_str(&format!("rows[{}]", parts.join(" ")));
        }
        match (msg.is_empty(), suffix.is_empty()) {
            (_, true) => msg,
            (true, false) => suffix,
            (false, false) => format!("{msg}  ·  {suffix}"),
        }
    }

    fn render(&self) {
        let sink = self.bar.lock().unwrap().clone();
        if let Some(sink) = sink {
            sink.set_message(&self.compose());
        }
    }
}

impl std::fmt::Debug for DownloadMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadMetrics")
            .field("api_requests", &self.api_requests.load(Ordering::Relaxed))
            .field("rows_upserted", &self.rows_upserted.lock().unwrap())
            .finish_non_exhaustive()
    }
}

tokio::task_local! {
    static CURRENT: Arc<DownloadMetrics>;
}

pub async fn scope<F>(metrics: Arc<DownloadMetrics>, fut: F) -> F::Output
where
    F: Future,
{
    CURRENT.scope(metrics, fut).await
}

fn with_current<R>(f: impl FnOnce(&DownloadMetrics) -> R) -> Option<R> {
    CURRENT.try_with(|m| f(m)).ok()
}

pub fn record_api_request() {
    let _ = with_current(DownloadMetrics::record_api_request);
}

pub fn record_upserts(table: &str, n: usize) {
    let _ = with_current(|m| m.record_upserts(table, n as u64));
}

// Live-suffix progress sink

/// Wraps a source's top-level bar so every `set_message` the provider
/// emits gets the live `api=… rows[…]` suffix appended. All other calls
/// pass straight through, and `child` returns the unwrapped inner sink
/// so nested per-unit bars stay clean.
pub struct MetricsSink {
    inner: Arc<dyn ProgressSink>,
    metrics: Arc<DownloadMetrics>,
}

impl MetricsSink {
    pub fn new(inner: Arc<dyn ProgressSink>, metrics: Arc<DownloadMetrics>) -> Self {
        Self { inner, metrics }
    }
}

impl ProgressSink for MetricsSink {
    fn set_length(&self, total: Option<u64>) {
        self.inner.set_length(total);
    }
    fn inc(&self, delta: u64) {
        self.inner.inc(delta);
    }
    fn set_message(&self, msg: &str) {
        // Store + recompose, then emit via the metrics' own render path
        // (which targets the same inner sink) so the message and the
        // counter suffix always render together.
        self.metrics.set_provider_message(msg);
    }
    fn finish(&self, msg: &str) {
        self.inner.finish(msg);
    }
    fn finish_and_clear(&self) {
        self.inner.finish_and_clear();
    }
    fn child(&self, prefix: &str) -> Arc<dyn ProgressSink> {
        self.inner.child(prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn counters_only_record_inside_scope() {
        // Outside any scope: silent no-op (must not panic).
        record_api_request();
        record_upserts("messages", 5);

        let metrics = DownloadMetrics::new();
        let m2 = metrics.clone();
        scope(metrics, async move {
            record_api_request();
            record_api_request();
            record_upserts("messages", 10);
            record_upserts("messages", 5);
            record_upserts("cas_objects", 2);
        })
        .await;
        assert_eq!(m2.api_requests(), 2);
        let snap = m2.rows_upserted_snapshot();
        assert_eq!(snap.get("messages").copied(), Some(15));
        assert_eq!(snap.get("cas_objects").copied(), Some(2));
        assert_eq!(m2.rows_upserted_total(), 17);
    }
}
