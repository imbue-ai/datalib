//! Event vocabulary shared across provider CLIs. Helpers emit
//! `tracing::info!` with a stable field set so JSON consumers and OTLP
//! dashboards see the same shape. Keep this list short — every event
//! added here becomes a long-term commitment to downstream consumers.

/// One HTTP-ish call to the upstream provider completed. Emitted at
/// `debug` because progress bars carry the cumulative counters; the
/// per-call detail is only interesting when something looks wrong.
pub fn item_fetched(url: &str, bytes: u64, duration_ms: u64) {
    tracing::debug!(
        event = "item_fetched",
        url = url,
        bytes = bytes,
        duration_ms = duration_ms,
    );
}

/// A batch of records was diffed against prior state and persisted.
pub fn indexed_batch(entity: &str, count: usize, duration_ms: u64) {
    tracing::info!(
        event = "indexed_batch",
        entity = entity,
        count = count,
        duration_ms = duration_ms,
    );
}
