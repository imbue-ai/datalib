//! Event vocabulary shared across provider CLIs. Helpers emit
//! `tracing::info!` with a stable field set so JSON consumers and OTLP
//! dashboards see the same shape. Keep this list short — every event
//! added here becomes a long-term commitment to downstream consumers.

pub fn indexed_batch(entity: &str, count: usize, duration_ms: u64) {
    tracing::info!(
        event = "indexed_batch",
        entity = entity,
        count = count,
        duration_ms = duration_ms,
        "indexed one batch"
    );
}
