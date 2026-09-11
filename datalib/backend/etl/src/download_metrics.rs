//! General per-download-step "what changed" counters, published as
//! metrics as they move.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::progress::Progress;

/// Per-source live counters, accumulated at the shared chokepoints for
/// the duration of one source's download. Every change is published as
/// a metric through the step's [`Progress`], so the runner's store — and
/// the Manage screen — see the same numbers the counters hold.
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
    sink: Progress,
}

impl DownloadMetrics {
    /// Counters that publish nowhere; what a test or a headless caller
    /// wants.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn publishing_to(sink: Progress) -> Arc<Self> {
        Arc::new(Self {
            sink,
            ..Default::default()
        })
    }

    pub fn record_api_request(&self) {
        let n = self.api_requests.fetch_add(1, Ordering::Relaxed) + 1;
        self.sink.metric("api_requests", &[], n as i64);
    }

    pub fn record_upserts(&self, table: &str, n: u64) {
        if n == 0 {
            return;
        }
        let total = {
            let mut m = self.rows_upserted.lock().unwrap();
            let t = m.entry(table.to_string()).or_insert(0);
            *t += n;
            *t
        };
        self.sink
            .metric("rows_upserted", &[("table", table)], total as i64);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::ProgressSink;

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

    type Published = (String, Vec<(String, String)>, i64);

    #[derive(Default)]
    struct Recording(Mutex<Vec<Published>>);
    impl ProgressSink for Recording {
        fn metric(&self, name: &str, labels: &[(&str, &str)], value: i64) {
            self.0.lock().unwrap().push((
                name.into(),
                labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                value,
            ));
        }
    }

    /// What reaches the wire is the running total, not the increment:
    /// the store keeps only the newest value per series.
    #[test]
    fn every_change_is_published_as_the_running_total() {
        let rec = Arc::new(Recording::default());
        let m = DownloadMetrics::publishing_to(Progress::new(rec.clone()));
        m.record_api_request();
        m.record_api_request();
        m.record_upserts("messages", 10);
        m.record_upserts("messages", 5);
        m.record_upserts("messages", 0);

        let got = rec.0.lock().unwrap();
        let table = vec![("table".to_string(), "messages".to_string())];
        assert_eq!(
            *got,
            vec![
                ("api_requests".to_string(), vec![], 1),
                ("api_requests".to_string(), vec![], 2),
                ("rows_upserted".to_string(), table.clone(), 10),
                ("rows_upserted".to_string(), table, 15),
            ],
            "a zero-row upsert publishes nothing"
        );
    }
}
